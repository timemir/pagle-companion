//! Recovery through the queue's authenticated HTTP boundary, real
//! SavedVariables reads and the production owned-folder writer.
use super::*;
use crate::readings::fixture::LOG_B;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, Request};

fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!("../../fixtures/recovery/staged-v1.json")).unwrap()
}
async fn recovery_harness() -> Harness {
    let mut h = Harness::new(1).await;
    h.uploads = Arc::new(Uploads::with_recovery_test_key());
    let folder = h.confirmed.remove(LOG_A).unwrap();
    h.confirmed.insert(LOG_B.into(), folder);
    h.state
        .update(|s| {
            let log = s.logs.remove(LOG_A).unwrap();
            s.logs.insert(LOG_B.into(), log);
        })
        .unwrap();
    let f = fixture();
    save(&h.folder, f["sourceLua"].as_str().unwrap());
    let flavor = h.folder.ancestors().nth(3).unwrap();
    let addon = flavor.join("Interface/AddOns/Pagle");
    std::fs::create_dir_all(&addon).unwrap();
    std::fs::write(addon.join("Pagle.toc"), b"## Interface: 16001\n").unwrap();
    let replacement = crate::recovery::snapshot::decode(
        &STANDARD
            .decode(f["replacementSnapshot"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    h.pagle.hold(
        &upload_doc::page(&replacement, 1, replacement.len(), 1)
            .unwrap()
            .document,
    );
    h
}
fn staged_folder(h: &Harness) -> PathBuf {
    h.folder
        .ancestors()
        .nth(3)
        .unwrap()
        .join("Interface/AddOns/Pagle_Recovery")
}
async fn serve(h: &Harness, signed: serde_json::Value) {
    let f = fixture();
    Mock::given(method("POST"))
        .and(path(format!("/api/v1/catch-logs/{LOG_B}/recovery")))
        .and(body_json(json!({"snapshot":f["sourceSnapshot"],"checkpoint":{"seq":1,"digest":f["acceptedDigest"],"revision":1}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(signed))
        .mount(&h.server).await;
}
async fn recover(h: &Harness) -> Result<String, String> {
    h.uploads.trigger_recovery(LOG_B);
    let mut ends = h.uploads.run_queued(&h.ctx()).await;
    match ends.pop().unwrap() {
        RunEnd::Recovery(result) => result,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn signed_recovery_stages_only_the_selected_install_without_changing_savedvariables() {
    let h = recovery_harness().await;
    let f = fixture();
    let original = std::fs::read(h.folder.join("SavedVariables/Pagle.lua")).unwrap();
    let other = h.tmp.path().join("other/Interface/AddOns/Pagle");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("Pagle.toc"), b"## Interface: 16001\n").unwrap();
    serve(&h, f["signed"].clone()).await;
    let instructions = recover(&h).await.unwrap();
    assert!(instructions.contains("character selection"));
    let folder = staged_folder(&h);
    let lua = std::fs::read_to_string(folder.join("Recovery.lua")).unwrap();
    assert!(lua.contains(f["signed"]["payload"].as_str().unwrap()));
    let toc = std::fs::read_to_string(folder.join("Pagle_Recovery.toc")).unwrap();
    assert!(toc.contains("## LoadOnDemand: 1"));
    assert!(!toc.contains("Dependencies:"));
    assert_eq!(
        std::fs::read(h.folder.join("SavedVariables/Pagle.lua")).unwrap(),
        original
    );
    assert!(!other.parent().unwrap().join("Pagle_Recovery").exists());
    let mut h = h.restart();
    h.uploads = Arc::new(Uploads::with_recovery_test_key());
    let pending = h.state.read(|s| s.logs[LOG_B].recovery.clone().unwrap());
    assert_eq!((pending.accepted_seq, pending.staged), (1, 1));
    assert_eq!(
        pending.application_id,
        f["signed"]["applicationId"].as_str().unwrap()
    );
    assert!(recover(&h).await.unwrap().contains("/reload"));
    assert_eq!(
        std::fs::read_to_string(folder.join("Recovery.lua")).unwrap(),
        lua
    );
}

#[tokio::test]
async fn unknown_keys_altered_signatures_and_wrong_application_ids_never_stage() {
    for (field, value) in [
        ("keyId", "untrusted"),
        ("signature", "AAAA"),
        (
            "applicationId",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ),
    ] {
        let h = recovery_harness().await;
        let mut signed = fixture()["signed"].clone();
        signed[field] = value.into();
        serve(&h, signed).await;
        assert!(recover(&h).await.is_err(), "{field}");
        assert!(!staged_folder(&h).exists());
        assert!(h.state.read(|s| s.logs[LOG_B].recovery.is_none()));
    }
}

#[tokio::test]
async fn a_metadata_change_after_planning_refuses_staging() {
    let h = recovery_harness().await;
    let folder = h.folder.clone();
    let f = fixture();
    let signed = f["signed"].clone();
    Mock::given(method("POST"))
        .and(path(format!("/api/v1/catch-logs/{LOG_B}/recovery")))
        .respond_with(move |_: &Request| {
            // The game saved new metadata while the backend prepared the package.
            save(
                &folder,
                &f["sourceLua"]
                    .as_str()
                    .unwrap()
                    .replace("selfFound=false", "selfFound=true"),
            );
            ResponseTemplate::new(200).set_body_json(signed.clone())
        })
        .mount(&h.server)
        .await;
    assert!(recover(&h).await.unwrap_err().contains("changed"));
    assert!(!staged_folder(&h).exists());
}

#[tokio::test]
async fn a_recovery_folder_link_cannot_write_outside_the_selected_install() {
    let h = recovery_harness().await;
    let outside = h.tmp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    #[cfg(target_os = "linux")]
    std::os::unix::fs::symlink(&outside, staged_folder(&h)).unwrap();
    #[cfg(windows)]
    assert!(
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(staged_folder(&h))
            .arg(&outside)
            .output()
            .unwrap()
            .status
            .success()
    );
    serve(&h, fixture()["signed"].clone()).await;
    assert!(recover(&h).await.is_err());
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
}

#[tokio::test]
async fn completion_waits_for_a_real_full_upload_and_retries_after_a_refused_confirmation() {
    let h = recovery_harness().await;
    serve(&h, fixture()["signed"].clone()).await;
    recover(&h).await.unwrap();
    let application = fixture()["signed"]["applicationId"]
        .as_str()
        .unwrap()
        .to_owned();
    let confirm = format!("/api/v1/catch-logs/{LOG_B}/recovery/{application}/confirm");
    assert!(
        h.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() != confirm)
    );
    // The addon wrote its guarded replacement. The complete original remains
    // owned by the addon; Companion reads only this newly saved normal log.
    save(
        &h.folder,
        &fixture()["sourceLua"]
            .as_str()
            .unwrap()
            .replace(", \"opaque broken source record\"", ""),
    );
    Mock::given(method("POST"))
        .and(path(&confirm))
        .respond_with(
            ResponseTemplate::new(409).set_body_json(json!({"reason":"validation_unavailable"})),
        )
        .up_to_n_times(1)
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path(&confirm))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result":"consistent"})))
        .with_priority(6)
        .mount(&h.server)
        .await;
    for retry in [false, true] {
        h.uploads.trigger(LOG_B, Trigger::UploadNow);
        assert_eq!(
            h.uploads.run_queued(&h.ctx()).await,
            [RunEnd::Recorded(RunResult::Uploaded)]
        );
        assert_eq!(h.state.read(|s| s.logs[LOG_B].recovery.is_none()), retry);
    }
    let requests = h.server.received_requests().await.unwrap();
    let proof = requests
        .iter()
        .position(|r| r.url.path() == "/api/v1/uploads")
        .unwrap();
    let confirmation = requests
        .iter()
        .position(|r| r.url.path() == confirm)
        .unwrap();
    assert!(proof < confirmation);
    assert!(h.pagle.pages().iter().all(|p| p.from == 1 && p.count == 1));
}

/// Explicit cross-repository acceptance seam. Never compiled into the product
/// binary and never enabled by an ordinary test run. Credentials stay outside
/// fixture files and results.
#[tokio::test]
#[ignore = "requires a coordinated live backend and actual addon"]
async fn coordinated_live_recovery() {
    let phase = std::env::var("PAGLE_RECOVERY_PHASE").expect("stage or resume");
    let base = std::env::var("PAGLE_RECOVERY_BASE_URL").unwrap();
    let credential_file = std::env::var("PAGLE_RECOVERY_CREDENTIAL_FILE").unwrap();
    let folder = std::env::var("PAGLE_RECOVERY_ACCOUNT_FOLDER").unwrap();
    let output = PathBuf::from(std::env::var("PAGLE_RECOVERY_OUTPUT").unwrap());
    std::fs::create_dir_all(&output).unwrap();
    let source = crate::recovery::read_source(&folder).unwrap();
    let log_id = source.log_id.clone();
    let state_path = output.join("state.json");
    let state = if state_path.exists() {
        match state::load(&state_path).unwrap() {
            Loaded::State(state) => *state,
            other => panic!("{other:?}"),
        }
    } else {
        let mut state = State {
            first_run_done: true,
            battle_tag: Some("Recovery Journey#1".into()),
            ..State::default()
        };
        state.logs.insert(
            log_id.clone(),
            LogState {
                answer: Some(Answer::Upload),
                folder: None,
                characters: source.characters_display(),
                last_folder: folder.clone(),
                uploaded_snapshot: None,
                observed_snapshot: None,
                recovery: None,
                recovery_notice: None,
                last_run: None,
                last_uploaded_at: None,
            },
        );
        state
    };
    let os = MemoryStore::new("test-only Recovery journey");
    let credential = std::fs::read_to_string(credential_file).unwrap();
    os.write(credential.trim()).unwrap();
    let link = Link::new(Keychain::new(Box::new(os), None), Hooks::default());
    let api = Api::new("0.1.0", base).unwrap();
    let state = Store::new(state_path, state);
    let notifications = Notifications::new(Recorder::default());
    let confirmed = [(log_id.clone(), folder)].into();
    let env = TestEnv::default();
    let uploads = Uploads::with_recovery_test_key();
    let ctx = Ctx {
        account: link.account(&api, &state, &notifications),
        state: &state,
        version: "0.1.0",
        confirmed: &confirmed,
        env: &env,
    };
    let ends = match phase.as_str() {
        "stage" => {
            uploads.trigger_recovery(&log_id);
            uploads.run_queued(&ctx).await
        }
        "resume" => {
            uploads.trigger(&log_id, Trigger::UploadNow);
            uploads.run_queued(&ctx).await
        }
        _ => panic!("phase must be stage or resume"),
    };
    let result = json!({"phase":phase,"ends":format!("{ends:?}"),"state":state.read(Clone::clone)});
    std::fs::write(
        output.join(format!("{phase}-result.json")),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
    match phase.as_str() {
        "stage" => assert!(matches!(&ends[..], [RunEnd::Recovery(Ok(_))]), "{ends:?}"),
        "resume" => {
            assert_eq!(ends, [RunEnd::Recorded(RunResult::Uploaded)]);
            assert!(
                state.read(|s| s.logs[&log_id].recovery.is_none()),
                "Recovery confirmation did not establish consistency; inspect result JSON"
            );
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn semantic_snapshot_preserves_nil_false_unicode_and_full_metadata_on_the_wire() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../fixtures/recovery/snapshot-v1.json")).unwrap();
    for vector in vectors["vectors"].as_array().unwrap() {
        let h = recovery_harness().await;
        let mut source = fixture()["sourceLua"]
            .as_str()
            .unwrap()
            .replace(", \"opaque broken source record\"", "");
        match vector["name"].as_str().unwrap() {
            "nil-selfFound" => source = source.replace("selfFound=false", "selfFound=nil"),
            "false-test" => source = source.replace("format = 3,", "format = 3, test = false,"),
            "unicode" => source = source.replace("name=\"Ada\"", "name=\"Bé\""),
            "base" => {}
            other => panic!("unhandled independent snapshot vector {other}"),
        }
        save(&h.folder, &source);
        recover(&h).await.unwrap_err(); // No package endpoint: inspect the request.
        let requests = h.server.received_requests().await.unwrap();
        let request = requests.iter().find(|r| r.method == "POST").unwrap();
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["snapshot"], vector["snapshot"], "{}", vector["name"]);
    }
}

#[tokio::test]
async fn authenticated_data_still_must_match_its_domain_target_snapshot_checkpoint_and_strict_grammar()
 {
    let variants: serde_json::Value =
        serde_json::from_str(include_str!("../../fixtures/recovery/adversarial-v1.json")).unwrap();
    for (name, signed) in variants.as_object().unwrap() {
        let h = recovery_harness().await;
        serve(&h, signed.clone()).await;
        let result = recover(&h).await;
        if name == "capacity-limit" {
            assert!(result.is_ok(), "{name}: {result:?}");
        } else {
            assert!(result.is_err(), "accepted invalid signed data: {name}");
            assert!(!staged_folder(&h).exists(), "{name}");
        }
    }
}

#[tokio::test]
async fn production_trust_does_not_include_the_fixture_signing_key() {
    let mut h = recovery_harness().await;
    h.uploads = Arc::new(Uploads::new());
    serve(&h, fixture()["signed"].clone()).await;
    assert!(
        recover(&h)
            .await
            .unwrap_err()
            .contains("Unknown Recovery signing key")
    );
    assert!(!staged_folder(&h).exists());
}

#[tokio::test]
async fn an_interrupted_stage_preserves_complete_data_and_retries_safely() {
    let h = recovery_harness().await;
    let folder = staged_folder(&h);
    std::fs::create_dir(&folder).unwrap();
    let obstacle = folder.join(".Pagle_Recovery.toc.tmp");
    std::fs::create_dir(&obstacle).unwrap();
    serve(&h, fixture()["signed"].clone()).await;
    assert!(recover(&h).await.is_err());
    let first = std::fs::read(folder.join("Recovery.lua")).unwrap();
    assert!(h.state.read(|s| s.logs[LOG_B].recovery.is_some()));
    assert!(!folder.join("Pagle_Recovery.toc").exists());
    std::fs::remove_dir(obstacle).unwrap();
    assert!(recover(&h).await.unwrap().contains("character selection"));
    assert_eq!(std::fs::read(folder.join("Recovery.lua")).unwrap(), first);
    assert!(folder.join("Pagle_Recovery.toc").is_file());
}

#[tokio::test]
async fn changing_the_selected_folder_during_planning_cannot_stage_into_the_old_install() {
    let h = recovery_harness().await;
    let received = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = received.clone();
    let signed = fixture()["signed"].clone();
    Mock::given(method("POST"))
        .and(path(format!("/api/v1/catch-logs/{LOG_B}/recovery")))
        .respond_with(move |_: &Request| {
            seen.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200)
                .set_body_json(signed.clone())
                .set_delay(Duration::from_millis(250))
        })
        .mount(&h.server)
        .await;
    let work = recover(&h);
    tokio::pin!(work);
    tokio::select! { _=&mut work=>panic!("Recovery completed before the HTTP race"), ()=until(||received.load(Ordering::SeqCst))=>{} }
    h.state
        .update(|s| s.logs.get_mut(LOG_B).unwrap().folder = Some("another WoW folder".into()))
        .unwrap();
    assert!(work.await.is_err());
    assert!(!staged_folder(&h).exists());
}

#[tokio::test]
async fn unlink_cleanup_removes_the_private_recovery_folder_and_preserves_savedvariables() {
    let h = recovery_harness().await;
    serve(&h, fixture()["signed"].clone()).await;
    recover(&h).await.unwrap();
    let file = h.folder.join("SavedVariables/Pagle.lua");
    let source = std::fs::read(&file).unwrap();
    crate::in_game::folders::delete_data_addon_folders(&[h
        .folder
        .ancestors()
        .nth(3)
        .unwrap()
        .to_string_lossy()
        .into_owned()]);
    assert!(!staged_folder(&h).exists());
    assert_eq!(std::fs::read(file).unwrap(), source);
}

#[tokio::test]
async fn a_stopped_or_unlinked_writer_cannot_recreate_a_recovery_folder() {
    let h = recovery_harness().await;
    let writer = crate::in_game::folders::Writer::new();
    assert!(
        writer
            .stage_recovery(&h.folder.to_string_lossy(), b"data", || false)
            .is_err()
    );
    writer.stop();
    assert!(
        writer
            .stage_recovery(&h.folder.to_string_lossy(), b"data", || true)
            .is_err()
    );
    assert!(!staged_folder(&h).exists());
}

#[tokio::test]
async fn cleanup_cannot_follow_an_interface_directory_link_outside_the_install() {
    let h = recovery_harness().await;
    let flavor = h.folder.ancestors().nth(3).unwrap();
    let interface = flavor.join("Interface");
    std::fs::rename(&interface, flavor.join("OriginalInterface")).unwrap();
    let outside = h.tmp.path().join("outside");
    let precious = outside.join("AddOns/Pagle_Recovery/precious.txt");
    std::fs::create_dir_all(precious.parent().unwrap()).unwrap();
    std::fs::write(&precious, b"keep this").unwrap();
    #[cfg(target_os = "linux")]
    std::os::unix::fs::symlink(&outside, &interface).unwrap();
    #[cfg(windows)]
    assert!(
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&interface)
            .arg(&outside)
            .output()
            .unwrap()
            .status
            .success()
    );
    crate::in_game::folders::delete_data_addon_folders(&[flavor.to_string_lossy().into_owned()]);
    assert_eq!(std::fs::read(&precious).unwrap(), b"keep this");
}

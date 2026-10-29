//! Recovery through the queue's authenticated HTTP boundary, real
//! SavedVariables reads and the production owned-folder writer.
use super::*;
use crate::in_game::folders::fixture::link_folder;
use crate::readings::fixture::LOG_B;
use crate::upload::{ANOTHER_RECOVERY, CONFIRM_GAVE_UP, CONFIRM_TRIES};
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
    std::fs::write(
        addon.join("Pagle.toc"),
        b"## Interface: 16002\n## AllowLoadGameType: camelot\n",
    )
    .unwrap();
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
    assert!(toc.starts_with("## Interface: 16002\n## AllowLoadGameType: camelot\n"));
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

#[test]
fn recovery_planning_uses_the_recovery_request_timeout() {
    // CMP Tunables: Recovery request timeout, not the HTTP timeouts' 120 s
    assert_eq!(
        Uploads::new().recovery_timeout,
        crate::api::RECOVERY_REQUEST_TIMEOUT
    );
}

#[tokio::test]
async fn recovery_planning_sets_its_own_timeout_over_the_client_default() {
    // CMP Tunables: Recovery request timeout, applied to the planning
    // request alone; the client's 120 s would have waited for this reply
    let mut h = recovery_harness().await;
    h.uploads = Arc::new(Uploads {
        recovery_timeout: Duration::from_millis(200),
        ..Uploads::with_recovery_test_key()
    });
    let f = fixture();
    Mock::given(method("POST"))
        .and(path(format!("/api/v1/catch-logs/{LOG_B}/recovery")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(f["signed"].clone())
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&h.server)
        .await;
    assert_eq!(
        recover(&h).await.unwrap_err(),
        "Couldn't prepare Recovery; try again"
    );
    assert!(!staged_folder(&h).exists());
    assert!(h.state.read(|s| s.logs[LOG_B].recovery.is_none()));
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
    link_folder(&staged_folder(&h), &outside);
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
            ResponseTemplate::new(409).set_body_json(json!({"reason":"recovery_not_uploaded"})),
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
        assert_eq!(staged_folder(&h).exists(), !retry);
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
    link_folder(&interface, &outside);
    crate::in_game::folders::delete_data_addon_folders(&[flavor.to_string_lossy().into_owned()]);
    assert_eq!(std::fs::read(&precious).unwrap(), b"keep this");
}

#[tokio::test]
async fn every_recovery_endpoint_obeys_required_stops() {
    for endpoint in ["cursor", "recovery", "confirm"] {
        for (code, message) in [
            ("suspended", SUSPENDED),
            ("update_required", UPDATE_REQUIRED),
        ] {
            let mut h = recovery_harness().await;
            let updates = Arc::new(AtomicUsize::new(0));
            let updated = updates.clone();
            let cancelled = h.uploads.clone();
            let os = MemoryStore::new("test link");
            os.write(CREDENTIAL).unwrap();
            h.link = Link::new(
                Keychain::new(Box::new(os), None),
                Hooks {
                    cancel_queue: Box::new(move || cancelled.cancel()),
                    entered_update_required: Box::new(move || {
                        updated.fetch_add(1, Ordering::SeqCst);
                    }),
                    ..Hooks::default()
                },
            );
            let target = if endpoint == "confirm" {
                serve(&h, fixture()["signed"].clone()).await;
                recover(&h).await.unwrap();
                save(
                    &h.folder,
                    &fixture()["sourceLua"]
                        .as_str()
                        .unwrap()
                        .replace(", \"opaque broken source record\"", ""),
                );
                format!(
                    "/api/v1/catch-logs/{LOG_B}/recovery/{}/confirm",
                    fixture()["signed"]["applicationId"].as_str().unwrap()
                )
            } else {
                format!("/api/v1/catch-logs/{LOG_B}/{endpoint}")
            };
            Mock::given(path(&target))
                .respond_with(
                    ResponseTemplate::new(403)
                        .set_body_json(json!({"reason": code, "message": message})),
                )
                .with_priority(1)
                .mount(&h.server)
                .await;
            if endpoint == "confirm" {
                h.uploads.trigger(LOG_B, Trigger::UploadNow);
                assert_eq!(
                    h.uploads.run_queued(&h.ctx()).await,
                    [RunEnd::Interrupted],
                    "{endpoint}: {code}"
                );
                assert!(h.state.read(|s| s.logs[LOG_B].recovery.is_some()));
            } else {
                assert!(recover(&h).await.is_err());
            }
            let account = h.link.account(&h.api, &h.state, &h.notifications);
            assert!(account.is_stopped(), "{endpoint}: {code}");
            assert_eq!(account.stop_message().as_deref(), Some(message));
            assert!(h.state.read(|s| s.logs[LOG_B].last_run.is_none()));
            assert_eq!(
                updates.load(Ordering::SeqCst),
                usize::from(code == "update_required")
            );
            if code == "suspended" {
                assert_eq!(*h.shown.0.lock().unwrap(), [SUSPENDED]);
            }
            assert!(matches!(
                account.send(reqwest::Method::GET, "probe", |r| r).await,
                Err(SendError::Stopped)
            ));
            assert!(
                h.server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .all(|r| r.url.path() != "/api/v1/probe")
            );
        }
    }
}

#[tokio::test]
async fn linking_another_player_discards_the_previous_players_recovery() {
    let h = recovery_harness().await;
    serve(&h, fixture()["signed"].clone()).await;
    recover(&h).await.unwrap();
    h.state
        .update(|s| {
            let log = s.logs.get_mut(LOG_B).unwrap();
            log.recovery_notice = Some("old Player notice".into());
            log.uploaded_snapshot = Some("old Player checkpoint".into());
        })
        .unwrap();
    let before = h.state.read(|s| s.logs[LOG_B].clone());
    h.link
        .account(&h.api, &h.state, &h.notifications)
        .store_link(Approval {
            credential: "pgc_another_player".into(),
            battle_tag: "Another#2345".into(),
        })
        .await
        .unwrap();
    let after = h.state.read(|s| s.logs[LOG_B].clone());
    assert!(after.recovery.is_none());
    assert!(after.recovery_notice.is_none());
    assert!(after.uploaded_snapshot.is_none());
    assert_eq!(after.answer, before.answer);
    assert_eq!(after.folder, before.folder);
    assert_eq!(after.last_folder, before.last_folder);
    save(
        &h.folder,
        &fixture()["sourceLua"]
            .as_str()
            .unwrap()
            .replace(", \"opaque broken source record\"", ""),
    );
    h.uploads.trigger(LOG_B, Trigger::PostLink);
    assert_eq!(
        h.uploads.run_queued(&h.ctx()).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    h.uploads.trigger(LOG_B, Trigger::UploadNow);
    assert_eq!(
        h.uploads.run_queued(&h.ctx()).await,
        [RunEnd::Recorded(RunResult::UpToDate)]
    );
    assert!(
        h.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| !r.url.path().ends_with("/confirm"))
    );
}

#[tokio::test]
async fn recovery_retirement_is_idempotent_and_keeps_another_staged_package() {
    let h = recovery_harness().await;
    serve(&h, fixture()["signed"].clone()).await;
    recover(&h).await.unwrap();
    let pending = h.state.read(|s| s.logs[LOG_B].recovery.clone().unwrap());
    let folder = staged_folder(&h);
    let file = folder.join("Recovery.lua");
    let bytes = std::fs::read(&file).unwrap();
    let different = [bytes.as_slice(), b"-- another package\n"].concat();
    std::fs::write(&file, &different).unwrap();
    let writer = crate::in_game::folders::Writer::new();
    writer.retire_recovery(&pending).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), different);
    std::fs::write(&file, &bytes).unwrap();
    let savedvariables = std::fs::read(h.folder.join("SavedVariables/Pagle.lua")).unwrap();
    writer.retire_recovery(&pending).unwrap();
    writer.retire_recovery(&pending).unwrap();
    assert!(!folder.exists());
    assert_eq!(
        std::fs::read(h.folder.join("SavedVariables/Pagle.lua")).unwrap(),
        savedvariables
    );
}

#[tokio::test]
async fn a_failed_retirement_keeps_pending_recovery_until_upload_retries_cleanup() {
    let h = recovery_harness().await;
    serve(&h, fixture()["signed"].clone()).await;
    recover(&h).await.unwrap();
    let pending = h.state.read(|s| s.logs[LOG_B].recovery.clone().unwrap());
    let obstacle = staged_folder(&h).parent().unwrap().join(format!(
        ".Pagle_Recovery.{}.retired",
        pending.package_digest.as_ref().unwrap()
    ));
    std::fs::create_dir(&obstacle).unwrap();
    std::fs::write(obstacle.join("keep"), b"occupied").unwrap();
    save(
        &h.folder,
        &fixture()["sourceLua"]
            .as_str()
            .unwrap()
            .replace(", \"opaque broken source record\"", ""),
    );
    Mock::given(path(format!(
        "/api/v1/catch-logs/{LOG_B}/recovery/{}/confirm",
        pending.application_id
    )))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": "consistent"})))
    .mount(&h.server)
    .await;
    h.uploads.trigger(LOG_B, Trigger::UploadNow);
    h.uploads.run_queued(&h.ctx()).await;
    assert!(h.state.read(|s| {
        s.logs[LOG_B]
            .recovery
            .as_ref()
            .unwrap()
            .instructions
            .contains("retire")
    }));
    assert!(staged_folder(&h).join("Recovery.lua").is_file());
    std::fs::remove_dir_all(obstacle).unwrap();
    h.uploads.trigger(LOG_B, Trigger::UploadNow);
    h.uploads.run_queued(&h.ctx()).await;
    assert!(h.state.read(|s| s.logs[LOG_B].recovery.is_none()));
    assert!(!staged_folder(&h).exists());
}

#[tokio::test]
async fn recovery_retirement_cannot_follow_a_folder_link_outside_the_install() {
    let h = recovery_harness().await;
    serve(&h, fixture()["signed"].clone()).await;
    recover(&h).await.unwrap();
    let pending = h.state.read(|s| s.logs[LOG_B].recovery.clone().unwrap());
    let folder = staged_folder(&h);
    let outside = h.tmp.path().join("outside-recovery");
    std::fs::rename(&folder, &outside).unwrap();
    link_folder(&folder, &outside);
    assert!(crate::in_game::folders::retire_recovery(&pending).is_err());
    assert!(outside.join("Recovery.lua").is_file());
}

/// The addon applied the package: the restored log, without the staged
/// original record.
fn save_applied(h: &Harness) {
    save(
        &h.folder,
        &fixture()["sourceLua"]
            .as_str()
            .unwrap()
            .replace(", \"opaque broken source record\"", ""),
    );
}
fn confirm_path() -> String {
    format!(
        "/api/v1/catch-logs/{LOG_B}/recovery/{}/confirm",
        fixture()["signed"]["applicationId"].as_str().unwrap()
    )
}
async fn received(h: &Harness, at: &str) -> usize {
    h.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == at)
        .count()
}

#[tokio::test]
async fn an_unapplied_recovery_never_reuploads_the_original_branch() {
    let h = recovery_harness().await;
    serve(&h, fixture()["signed"].clone()).await;
    recover(&h).await.unwrap();
    let source = crate::recovery::read_source(&h.folder.to_string_lossy()).unwrap();
    assert_eq!(
        h.state
            .read(|s| s.logs[LOG_B].recovery.clone().unwrap().expected_digest),
        Some(crate::recovery::snapshot_digest(&source))
    );
    // The fixture's original branch holds a record the normal reader
    // refuses. Stand in an original branch an ordinary Upload would send,
    // as after a cross-log conflict, as the plan's expected local log.
    let original = fixture()["sourceLua"]
        .as_str()
        .unwrap()
        .replace("opaque broken source record", "S1|1|1792422100|0|87|150|25");
    save(&h.folder, &original);
    let expected = crate::recovery::snapshot_digest(
        &crate::recovery::read_source(&h.folder.to_string_lossy()).unwrap(),
    );
    h.state
        .update(|s| {
            s.logs
                .get_mut(LOG_B)
                .unwrap()
                .recovery
                .as_mut()
                .unwrap()
                .expected_digest = Some(expected)
        })
        .unwrap();
    // Character selection saves SavedVariables again before the addon has
    // applied the package: still exactly the plan's expected local log.
    // Sending its conflicting entries would move the backend's conflict
    // revision and leave the plan stale at confirmation.
    save(&h.folder, &original);
    for trigger in [Trigger::Scan, Trigger::UploadNow] {
        h.uploads.trigger(LOG_B, trigger);
        assert_eq!(
            h.uploads.run_queued(&h.ctx()).await,
            [RunEnd::NotStarted("Recovery isn't applied yet")],
            "{trigger:?}"
        );
    }
    assert_eq!(received(&h, "/api/v1/uploads").await, 0);
    assert_eq!(received(&h, &confirm_path()).await, 0);
    assert!(h.state.read(|s| s.logs[LOG_B].recovery.is_some()));
    assert!(h.state.read(|s| s.logs[LOG_B].last_run.is_none()));
    // Once the local log reflects the applied package, it uploads and confirms.
    save_applied(&h);
    Mock::given(method("POST"))
        .and(path(confirm_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result":"consistent"})))
        .mount(&h.server)
        .await;
    h.uploads.trigger(LOG_B, Trigger::Scan);
    assert_eq!(
        h.uploads.run_queued(&h.ctx()).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert!(h.state.read(|s| s.logs[LOG_B].recovery.is_none()));
    assert_eq!(received(&h, &confirm_path()).await, 1);
}

#[tokio::test]
async fn a_transient_confirmation_failure_keeps_recovery_pending_and_retries_after_the_backoff() {
    let mut h = recovery_harness().await;
    serve(&h, fixture()["signed"].clone()).await;
    recover(&h).await.unwrap();
    save_applied(&h);
    // The backend's one Recovery permit is busy: 503 validation_unavailable.
    Mock::given(method("POST"))
        .and(path(confirm_path()))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(json!({"reason":"validation_unavailable"})),
        )
        .up_to_n_times(2)
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path(confirm_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result":"consistent"})))
        .with_priority(6)
        .mount(&h.server)
        .await;
    // Stopped during the backoff: Recovery stays pending and no result is
    // recorded.
    h.env.cancel_at_wait = Some((1, h.uploads.clone()));
    h.uploads.trigger(LOG_B, Trigger::UploadNow);
    assert_eq!(h.uploads.run_queued(&h.ctx()).await, [RunEnd::Interrupted]);
    assert_eq!(
        h.state
            .read(|s| s.logs[LOG_B].recovery.clone().unwrap().instructions),
        crate::upload::CONFIRM_RETRY
    );
    assert!(h.state.read(|s| s.logs[LOG_B].last_run.is_none()));
    assert!(staged_folder(&h).exists());
    // Left to wait, the run starts again after the backoff, uploads a new
    // proof and confirms, all in this one run.
    h.env.cancel_at_wait = None;
    h.uploads.trigger(LOG_B, Trigger::UploadNow);
    assert_eq!(
        h.uploads.run_queued(&h.ctx()).await,
        [RunEnd::Requeued, RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(h.env.sleeps().len(), 2);
    assert_eq!(received(&h, &confirm_path()).await, 3);
    assert!(h.state.read(|s| s.logs[LOG_B].recovery.is_none()));
    assert!(!staged_folder(&h).exists());
    let requests = h.server.received_requests().await.unwrap();
    let paths: Vec<_> = requests.iter().map(|r| r.url.path()).collect();
    let last_confirm = paths.iter().rposition(|p| *p == confirm_path()).unwrap();
    let before = &paths[..last_confirm];
    let previous_confirm = before.iter().rposition(|p| *p == confirm_path()).unwrap();
    assert!(before[previous_confirm..].contains(&"/api/v1/uploads"));
}

#[tokio::test]
async fn a_second_catch_log_in_the_same_install_cannot_overwrite_a_pending_recovery() {
    let h = recovery_harness().await;
    // Another WoW account of this install staged its own Recovery, which is
    // still pending. The addon loads exactly one package per install.
    let package = staged_folder(&h);
    std::fs::create_dir_all(&package).unwrap();
    let theirs: &[u8] =
        b"PagleRecoveryData = { v = 1, keyId = \"testKey\", payload = \"QQ==\", sig = \"QQ==\" }\n";
    std::fs::write(package.join("Recovery.lua"), theirs).unwrap();
    std::fs::write(package.join("Pagle_Recovery.toc"), b"## Interface: 16002\n").unwrap();
    let other = h.folder.parent().unwrap().join("BOB#2");
    h.state
        .update(|s| {
            let mut log = s.logs[LOG_B].clone();
            log.recovery = Some(crate::recovery::Pending {
                staged_folder: Some(other.to_string_lossy().into_owned()),
                package_digest: Some(crate::in_game::folders::sha256_hex(theirs)),
                expected_digest: None,
                confirm_failures: 0,
                application_id: "a".repeat(64),
                replacement_digest: "b".repeat(64),
                accepted_seq: 1,
                staged: 0,
                instructions: "Recovery is ready.".into(),
            });
            s.logs.insert(LOG_A.into(), log);
        })
        .unwrap();
    serve(&h, fixture()["signed"].clone()).await;
    let refused = recover(&h).await.unwrap_err();
    assert!(refused.contains("Another Catch log"), "{refused}");
    assert_eq!(std::fs::read(package.join("Recovery.lua")).unwrap(), theirs);
    assert!(h.state.read(|s| s.logs[LOG_B].recovery.is_none()));
    assert_eq!(
        h.state.read(|s| s.logs[LOG_B].recovery_notice.clone()),
        Some(refused)
    );
    assert!(h.state.read(|s| s.logs[LOG_A].recovery.is_some()));
    // Not even planned: the backend's Recovery permit stays free.
    assert_eq!(
        received(&h, &format!("/api/v1/catch-logs/{LOG_B}/recovery")).await,
        0
    );
    // Once that package is retired, this Catch log stages its own.
    std::fs::remove_dir_all(&package).unwrap();
    assert!(recover(&h).await.unwrap().contains("character selection"));
}

/// Another confirmed Catch log, `log_id`, in `folder`, with no Recovery.
fn add_log(h: &mut Harness, log_id: &str, folder: &Path) {
    save(folder, &pagle_lua_text(log_id, 2));
    let text = folder.to_string_lossy().into_owned();
    h.state
        .update(|s| {
            let mut log = s.logs[LOG_B].clone();
            log.recovery = None;
            log.recovery_notice = None;
            log.last_folder = text.clone();
            s.logs.insert(log_id.into(), log);
        })
        .unwrap();
    h.confirmed.insert(log_id.into(), text);
}

fn pending(h: &Harness, log_id: &str) -> Option<crate::recovery::Pending> {
    h.state.read(|s| s.logs[log_id].recovery.clone())
}

#[tokio::test]
async fn busy_confirmations_let_other_uploads_through_and_stop_after_their_budget_until_upload_now()
{
    let mut h = recovery_harness().await;
    serve(&h, fixture()["signed"].clone()).await;
    recover(&h).await.unwrap();
    save_applied(&h);
    let other = account_folder(&h.tmp.path().join("second"), "CY#3");
    add_log(&mut h, LOG_A, &other);
    Mock::given(method("POST"))
        .and(path(confirm_path()))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(json!({"reason":"validation_unavailable"})),
        )
        .mount(&h.server)
        .await;
    h.uploads.trigger(LOG_B, Trigger::Scan);
    h.uploads.trigger(LOG_A, Trigger::Scan);
    // Each busy confirmation waits one retry backoff, as an ordinary retry
    // would, then queues again behind the other Catch log's run.
    assert_eq!(
        h.uploads.run_queued(&h.ctx()).await,
        [
            RunEnd::Requeued,
            RunEnd::Recorded(RunResult::Uploaded),
            RunEnd::Requeued,
            RunEnd::Recorded(RunResult::Uploaded),
        ]
    );
    assert_eq!(h.env.sleeps(), [30, 30]);
    assert_eq!(received(&h, &confirm_path()).await, CONFIRM_TRIES as usize);
    let paths: Vec<String> = h
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect();
    let others = paths
        .iter()
        .position(|p| *p == format!("/api/v1/catch-logs/{LOG_A}/cursor"))
        .unwrap();
    let second_confirm = paths
        .iter()
        .enumerate()
        .filter(|(_, p)| **p == confirm_path())
        .nth(1)
        .unwrap()
        .0;
    assert!(others < second_confirm);
    assert_eq!(
        h.state
            .read(|s| s.logs[LOG_A].last_run.clone().unwrap().result),
        RunResult::Uploaded
    );
    // Out of tries: Recovery stays pending for Upload now, and its row says so.
    let waiting = pending(&h, LOG_B).unwrap();
    assert_eq!(waiting.instructions, CONFIRM_GAVE_UP);
    assert_eq!(waiting.confirm_failures, CONFIRM_TRIES);
    assert!(staged_folder(&h).exists());
    // Automatic runs no longer resend the whole log or confirm.
    let uploads = received(&h, "/api/v1/uploads").await;
    h.uploads.trigger(LOG_B, Trigger::Scan);
    h.uploads.run_queued(&h.ctx()).await;
    assert_eq!(received(&h, "/api/v1/uploads").await, uploads);
    assert_eq!(received(&h, &confirm_path()).await, CONFIRM_TRIES as usize);
    assert_eq!(pending(&h, LOG_B).unwrap().instructions, CONFIRM_GAVE_UP);
    // Upload now uploads a new proof and checks again.
    Mock::given(method("POST"))
        .and(path(confirm_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result":"consistent"})))
        .with_priority(1)
        .mount(&h.server)
        .await;
    h.uploads.trigger(LOG_B, Trigger::UploadNow);
    assert_eq!(
        h.uploads.run_queued(&h.ctx()).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert!(received(&h, "/api/v1/uploads").await > uploads);
    assert!(pending(&h, LOG_B).is_none());
    assert!(!staged_folder(&h).exists());
}

#[tokio::test]
async fn an_unknown_confirmation_refusal_ends_recovery_without_retrying() {
    let h = recovery_harness().await;
    serve(&h, fixture()["signed"].clone()).await;
    recover(&h).await.unwrap();
    save_applied(&h);
    Mock::given(method("POST"))
        .and(path(confirm_path()))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({"reason":"something_new"})))
        .mount(&h.server)
        .await;
    for end in [RunResult::Uploaded, RunResult::UpToDate] {
        h.uploads.trigger(LOG_B, Trigger::UploadNow);
        assert_eq!(
            h.uploads.run_queued(&h.ctx()).await,
            [RunEnd::Recorded(end)]
        );
    }
    assert!(h.env.sleeps().is_empty());
    assert_eq!(received(&h, &confirm_path()).await, 1);
    assert!(pending(&h, LOG_B).is_none());
    assert_eq!(
        h.state.read(|s| s.logs[LOG_B].recovery_notice.clone()),
        Some(crate::recovery::REFUSED.to_owned())
    );
    assert!(!staged_folder(&h).exists());
}

#[tokio::test]
async fn a_refused_recovery_stops_claiming_the_install_for_another_catch_log() {
    let mut h = recovery_harness().await;
    // Another WoW account of this install staged its Recovery, and the addon
    // applied it there.
    let package = staged_folder(&h);
    std::fs::create_dir_all(&package).unwrap();
    let theirs: &[u8] =
        b"PagleRecoveryData = { v = 1, keyId = \"testKey\", payload = \"QQ==\", sig = \"QQ==\" }\n";
    std::fs::write(package.join("Recovery.lua"), theirs).unwrap();
    std::fs::write(package.join("Pagle_Recovery.toc"), b"## Interface: 16002\n").unwrap();
    let other = account_folder(h.tmp.path(), "BOB#2");
    add_log(&mut h, LOG_A, &other);
    let application = "a".repeat(64);
    h.state
        .update(|s| {
            s.logs.get_mut(LOG_A).unwrap().recovery = Some(crate::recovery::Pending {
                staged_folder: Some(other.to_string_lossy().into_owned()),
                package_digest: Some(crate::in_game::folders::sha256_hex(theirs)),
                expected_digest: None,
                confirm_failures: 0,
                application_id: application.clone(),
                replacement_digest: "b".repeat(64),
                accepted_seq: 1,
                staged: 0,
                instructions: "Recovery is ready.".into(),
            });
        })
        .unwrap();
    serve(&h, fixture()["signed"].clone()).await;
    assert_eq!(recover(&h).await.unwrap_err(), ANOTHER_RECOVERY);
    // Pagle refuses that Recovery for good.
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v1/catch-logs/{LOG_A}/recovery/{application}/confirm"
        )))
        .respond_with(
            ResponseTemplate::new(409).set_body_json(json!({"reason":"operator_review_required"})),
        )
        .mount(&h.server)
        .await;
    h.uploads.trigger(LOG_A, Trigger::UploadNow);
    assert_eq!(
        h.uploads.run_queued(&h.ctx()).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert!(pending(&h, LOG_A).is_none());
    assert_eq!(
        h.state
            .read(|s| s.logs[LOG_A].recovery_notice.clone())
            .as_deref(),
        Some(crate::recovery::refusal_message(Some(
            "operator_review_required"
        )))
    );
    assert!(!package.exists());
    // Even had its package stayed, nothing claims it any more: this Catch log
    // stages its own in its place.
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("Recovery.lua"), theirs).unwrap();
    recover(&h).await.unwrap();
    assert_ne!(std::fs::read(package.join("Recovery.lua")).unwrap(), theirs);
    assert!(pending(&h, LOG_B).is_some());
}

#[tokio::test]
async fn a_package_that_changes_nothing_locally_still_uploads_and_confirms() {
    let h = recovery_harness().await;
    serve(&h, fixture()["signed"].clone()).await;
    recover(&h).await.unwrap();
    save_applied(&h);
    // A plan whose replacement is exactly the local log: applying it leaves
    // the local snapshot as it was.
    let local = crate::recovery::snapshot_digest(
        &crate::recovery::read_source(&h.folder.to_string_lossy()).unwrap(),
    );
    h.state
        .update(|s| {
            let pending = s.logs.get_mut(LOG_B).unwrap().recovery.as_mut().unwrap();
            pending.expected_digest = Some(local.clone());
            pending.replacement_digest = local.clone();
        })
        .unwrap();
    Mock::given(method("POST"))
        .and(path(confirm_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result":"consistent"})))
        .mount(&h.server)
        .await;
    h.uploads.trigger(LOG_B, Trigger::Scan);
    assert_eq!(
        h.uploads.run_queued(&h.ctx()).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(received(&h, &confirm_path()).await, 1);
    assert!(pending(&h, LOG_B).is_none());
}

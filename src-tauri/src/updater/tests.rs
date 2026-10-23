//! The updater against a stub release server: `latest.json` and signed files
//! served locally, signed with a throwaway minisign key pair generated in a
//! temp folder that is deleted afterwards. Nothing here reaches GitHub.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use base64::Engine;
use minisign::KeyPair;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;

const HOUR: Duration = Duration::from_secs(60 * 60);
const MINUTE: Duration = Duration::from_secs(60);

fn t0() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

fn base64(text: String) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
}

/// A throwaway updater key pair, written to `dir` as `tauri signer
/// generate` would (unencrypted here), and the public key as
/// `plugins.updater.pubkey` holds it.
struct Keys {
    pair: KeyPair,
    pubkey: String,
}

impl Keys {
    fn generate(dir: &Path) -> Self {
        let pair = KeyPair::generate_unencrypted_keypair().unwrap();
        let public = pair.pk.to_box().unwrap().into_string();
        let secret = pair.sk.to_box(None).unwrap().into_string();
        fs::write(dir.join("updater.key.pub"), &public).unwrap();
        fs::write(dir.join("updater.key"), secret).unwrap();
        let pubkey = base64(fs::read_to_string(dir.join("updater.key.pub")).unwrap());
        Self { pair, pubkey }
    }

    /// The `.sig` content for `data`, as `latest.json` carries it.
    fn sign(&self, data: &[u8], trusted_comment: &str) -> String {
        let signature = minisign::sign(
            Some(&self.pair.pk),
            &self.pair.sk,
            std::io::Cursor::new(data),
            Some(trusted_comment),
            Some("signature from tauri secret key"),
        )
        .unwrap();
        base64(signature.into_string())
    }
}

/// Serves whatever path was last published, else `404`.
#[derive(Clone, Default)]
struct Files(Arc<Mutex<HashMap<String, Vec<u8>>>>);

impl Respond for Files {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        match self.0.lock().unwrap().get(request.url.path()) {
            Some(body) => ResponseTemplate::new(200).set_body_bytes(body.clone()),
            None => ResponseTemplate::new(404),
        }
    }
}

/// A stub of the GitHub Release, its key pair, and a state directory.
struct World {
    tmp: tempfile::TempDir,
    rt: tokio::runtime::Runtime,
    server: MockServer,
    files: Files,
    keys: Keys,
    /// Signs with a key the Companion doesn't embed.
    other_keys: Keys,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["keys", "other-keys", "state"] {
            fs::create_dir(tmp.path().join(dir)).unwrap();
        }
        let keys = Keys::generate(&tmp.path().join("keys"));
        let other_keys = Keys::generate(&tmp.path().join("other-keys"));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let server = rt.block_on(MockServer::start());
        let files = Files::default();
        rt.block_on(
            Mock::given(any())
                .respond_with(files.clone())
                .mount(&server),
        );
        Self {
            tmp,
            rt,
            server,
            files,
            keys,
            other_keys,
        }
    }

    fn state_dir(&self) -> PathBuf {
        self.tmp.path().join("state")
    }

    fn source(&self) -> Source {
        Source::new(
            format!("{}/latest.json", self.server.uri()),
            &self.keys.pubkey,
        )
        .unwrap()
    }

    /// An updater running `version`, keeping installers as Windows does.
    fn updater(&self, version: &str) -> Updater {
        self.updater_with(version, self.installer_keeper(), Hooks::default())
    }

    fn installer_keeper(&self) -> Keeper {
        Keeper::Installer {
            dir: self.state_dir(),
        }
    }

    fn updater_with(&self, version: &str, keeper: Keeper, hooks: Hooks) -> Updater {
        Updater::with(
            self.source(),
            version,
            keeper,
            Failures::load(self.state_dir().join("updater.json")),
            hooks,
        )
    }

    /// Publishes `version` as the latest Release, its file signed with the
    /// Companion's key.
    fn publish(&self, version: &str) -> Vec<u8> {
        let bytes = format!("MZ installer {version}").into_bytes();
        let signature = self.keys.sign(&bytes, &trusted_comment(version));
        self.publish_signed(version, &bytes, &signature);
        bytes
    }

    /// Publishes `version` as the latest Release, its file an AppImage
    /// signed with the Companion's key.
    fn publish_appimage(&self, version: &str) -> Vec<u8> {
        let bytes = format!("AppImage {version}").into_bytes();
        let signature = self.keys.sign(&bytes, &appimage_comment(version));
        self.publish_signed(version, &bytes, &signature);
        bytes
    }

    fn publish_signed(&self, version: &str, bytes: &[u8], signature: &str) {
        let path = format!("/download/v{version}/Pagle.Companion_{version}_x64-setup.exe");
        let latest = serde_json::json!({
            "version": version,
            "notes": "",
            "pub_date": "2026-11-20T10:00:00Z",
            "platforms": {
                release::platform(): {
                    "signature": signature,
                    "url": format!("{}{path}", self.server.uri()),
                },
            },
        });
        let mut files = self.files.0.lock().unwrap();
        files.insert("/latest.json".into(), latest.to_string().into_bytes());
        files.insert(path, bytes.to_vec());
    }

    /// Makes the published latest.json name `url` as this platform's file.
    fn point_latest_at(&self, url: &str) {
        let mut files = self.files.0.lock().unwrap();
        let mut latest: serde_json::Value = serde_json::from_slice(&files["/latest.json"]).unwrap();
        latest["platforms"][release::platform()]["url"] = url.into();
        files.insert("/latest.json".into(), latest.to_string().into_bytes());
    }

    /// Every request the stub received.
    fn requests(&self) -> Vec<Request> {
        self.rt
            .block_on(self.server.received_requests())
            .unwrap_or_default()
    }

    fn downloads(&self) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.url.path().starts_with("/download/"))
            .count()
    }

    fn checks(&self) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.url.path() == "/latest.json")
            .count()
    }

    fn state_files(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.state_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn updater_json(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.state_dir().join("updater.json")).unwrap()).unwrap()
    }
}

/// The trusted comment `tauri signer sign` writes for the Windows
/// installer.
fn trusted_comment(version: &str) -> String {
    format!("timestamp:1800000000\tfile:Pagle Companion_{version}_x64-setup.exe")
}

/// The trusted comment `tauri signer sign` writes for the AppImage.
fn appimage_comment(version: &str) -> String {
    format!("timestamp:1800000000\tfile:Pagle Companion_{version}_amd64.AppImage")
}

fn v(version: &str) -> Version {
    Version::parse(version).unwrap()
}

#[test]
fn a_start_checks_once_and_sends_nothing_about_the_machine() {
    // DST-R50, DST-R51, CMP-R6: at start, one GET of the endpoint, with no
    // Authorization, cookie, User-Agent or version header
    let world = World::new();
    world.publish("1.0.0");
    let updater = world.updater("1.0.0");

    assert_eq!(updater.tick(t0()), Some(Checked::UpToDate));

    let requests = world.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method.as_str(), "GET");
    assert_eq!(requests[0].url.path(), "/latest.json");
    assert_eq!(requests[0].url.query(), None);
    for header in [
        "authorization",
        "cookie",
        "user-agent",
        "pagle-companion-version",
    ] {
        assert!(
            !requests[0].headers.contains_key(header),
            "{header}: {:?}",
            requests[0].headers
        );
    }
}

#[test]
fn the_next_check_is_six_hours_later_and_a_failed_one_waits_for_it() {
    // DST-R51: then whenever the update interval has passed, tested every
    // tick; a failed check (here no latest.json) is retried at the next
    // interval, not at the next tick
    let world = World::new();
    let updater = world.updater("1.0.0");

    assert_eq!(updater.tick(t0()), Some(Checked::Failed));
    assert_eq!(updater.tick(t0() + MINUTE), None);
    assert_eq!(updater.tick(t0() + 6 * HOUR - MINUTE), None);
    assert_eq!(world.checks(), 1);

    world.publish("1.0.0");
    assert_eq!(updater.tick(t0() + 6 * HOUR), Some(Checked::UpToDate));
    assert_eq!(world.checks(), 2);
}

#[test]
fn a_clock_jump_past_six_hours_checks_at_the_next_tick() {
    // DST-R51: a check overdue after sleep runs within a tick of waking
    let world = World::new();
    world.publish("1.0.0");
    let updater = world.updater("1.0.0");
    updater.tick(t0());
    assert_eq!(updater.tick(t0() + 9 * HOUR), Some(Checked::UpToDate));
    assert_eq!(world.checks(), 2);
}

#[test]
fn a_bad_document_is_a_failed_check() {
    // DST-R51: a bad document fails the check
    let world = World::new();
    world
        .files
        .0
        .lock()
        .unwrap()
        .insert("/latest.json".into(), b"<html>Not Found</html>".to_vec());
    assert_eq!(world.updater("1.0.0").tick(t0()), Some(Checked::Failed));
}

#[test]
fn only_a_higher_version_is_downloaded() {
    // DST-R52: a lower or equal version is never offered
    for (latest, offered) in [("0.9.0", false), ("1.0.0", false), ("1.0.1", true)] {
        let world = World::new();
        world.publish(latest);
        let checked = world.updater("1.0.0").tick(t0()).unwrap();
        assert_eq!(checked == Checked::Waiting(v(latest)), offered, "{latest}");
        assert_eq!(world.downloads(), usize::from(offered), "{latest}");
    }
}

#[test]
fn a_tester_build_downloads_the_release_above_it() {
    // DST-R48, DST-R52: 0.0.1-test.5 updates to the higher Release 0.0.1
    let world = World::new();
    world.publish("0.0.1");
    assert_eq!(
        world.updater("0.0.1-test.5").tick(t0()),
        Some(Checked::Waiting(v("0.0.1")))
    );
}

#[test]
fn a_good_signature_makes_the_update_waiting_in_the_state_directory() {
    // DST-R53, DST-R54: downloaded at once, verified against the public key,
    // and kept in the state directory as the waiting update
    let world = World::new();
    let bytes = world.publish("1.2.0");
    let updater = world.updater("1.0.0");

    assert_eq!(updater.tick(t0()), Some(Checked::Waiting(v("1.2.0"))));

    let waiting = updater.waiting().unwrap();
    assert_eq!(waiting.version, v("1.2.0"));
    assert_eq!(
        waiting.file,
        world
            .state_dir()
            .join("pagle-companion_1.2.0_x64-setup.exe")
    );
    assert_eq!(fs::read(&waiting.file).unwrap(), bytes);
    assert_eq!(updater.verify_waiting(&waiting), Ok(()));
    assert_eq!(updater.failed_verifications(), 0);
    assert_eq!(
        world.state_files(),
        [
            "pagle-companion_1.2.0_x64-setup.exe",
            "pagle-companion_1.2.0_x64-setup.exe.sig",
            "updater.json",
        ]
    );
}

#[test]
fn a_bad_signature_is_discarded_and_fetched_again_at_the_next_check() {
    // DST-R53, DST-R57: one that fails verification is discarded, never
    // waiting, and fetched again at the next check; each failure is counted
    // in updater.json, and a good verification resets the count
    let world = World::new();
    let bytes = b"MZ installer 1.2.0".to_vec();
    let forged = world.other_keys.sign(&bytes, &trusted_comment("1.2.0"));
    world.publish_signed("1.2.0", &bytes, &forged);
    let updater = world.updater("1.0.0");

    assert_eq!(
        updater.tick(t0()),
        Some(Checked::NotVerified { in_a_row: 1 })
    );
    assert_eq!(updater.waiting(), None);
    assert_eq!(world.state_files(), ["updater.json"]);
    assert_eq!(
        world.updater_json(),
        serde_json::json!({ "v": 1, "failedVerifications": 1, "forVersion": "1.2.0" })
    );

    assert_eq!(updater.tick(t0() + MINUTE), None);
    assert_eq!(
        updater.tick(t0() + 6 * HOUR),
        Some(Checked::NotVerified { in_a_row: 2 })
    );
    assert_eq!(world.downloads(), 2);

    world.publish("1.2.0");
    assert_eq!(
        updater.tick(t0() + 12 * HOUR),
        Some(Checked::Waiting(v("1.2.0")))
    );
    assert_eq!(world.updater_json()["failedVerifications"], 0);
}

#[test]
fn tampered_bytes_fail_verification() {
    // DST-R53: verified against the embedded public key; a file changed after
    // signing is discarded
    let world = World::new();
    let signature = world
        .keys
        .sign(b"MZ installer 1.2.0", &trusted_comment("1.2.0"));
    world.publish_signed("1.2.0", b"MZ installer 1.2.0 with malware", &signature);
    assert_eq!(
        world.updater("1.0.0").tick(t0()),
        Some(Checked::NotVerified { in_a_row: 1 })
    );
}

#[test]
fn a_file_signed_for_another_version_fails_verification() {
    // DST-R53, as the plugin verifies: an old Release's signed file offered
    // as a new version is refused
    let world = World::new();
    let bytes = b"MZ installer 1.1.0".to_vec();
    let signature = world.keys.sign(
        &bytes,
        &format!("{}\tversion:1.1.0", trusted_comment("1.1.0")),
    );
    world.publish_signed("1.2.0", &bytes, &signature);
    assert_eq!(
        world.updater("1.0.0").tick(t0()),
        Some(Checked::NotVerified { in_a_row: 1 })
    );
}

#[test]
fn an_old_releases_file_offered_as_a_new_version_fails_verification() {
    // DST-R52, DST-R53: latest.json isn't signed, so a forged one could
    // announce 1.2.0 and serve 1.1.0's validly signed installer. The version
    // in the signed file name (as `tauri signer` writes it) refuses that.
    let world = World::new();
    let old = b"MZ installer 1.1.0".to_vec();
    let signature = world.keys.sign(&old, &trusted_comment("1.1.0"));
    world.publish_signed("1.2.0", &old, &signature);
    let updater = world.updater("1.1.5");
    assert_eq!(
        updater.tick(t0()),
        Some(Checked::NotVerified { in_a_row: 1 })
    );
    assert_eq!(updater.waiting(), None);
    assert_eq!(world.state_files(), ["updater.json"]);
}

#[test]
fn the_other_platforms_file_of_the_release_fails_verification() {
    // DST-R53, DST Contracts "Release assets": a forged latest.json could
    // name the Windows installer, validly signed for the same version, as
    // the Linux file; renamed over the AppImage, it could never start or
    // update again. And the other way round.
    let world = World::new();
    let appimage = world.tmp.path().join("pagle-companion.AppImage");
    fs::write(&appimage, b"old AppImage").unwrap();
    world.publish("1.2.0");
    let linux = world.updater_with(
        "1.0.0",
        Keeper::AppImage {
            path: Some(appimage.clone()),
        },
        Hooks::default(),
    );
    assert_eq!(linux.tick(t0()), Some(Checked::NotVerified { in_a_row: 1 }));
    assert_eq!(fs::read(&appimage).unwrap(), b"old AppImage");

    let world = World::new();
    world.publish_appimage("1.2.0");
    let windows = world.updater("1.0.0");
    assert_eq!(
        windows.tick(t0()),
        Some(Checked::NotVerified { in_a_row: 1 })
    );
    assert_eq!(world.state_files(), ["updater.json"]);
}

#[test]
fn a_signature_naming_no_version_fails_verification() {
    // DST-R52, DST-R53: a signed file whose trusted comment names no version
    // could be any Release's
    let world = World::new();
    let bytes = b"MZ installer 1.2.0".to_vec();
    let signature = world.keys.sign(&bytes, "timestamp:1800000000");
    world.publish_signed("1.2.0", &bytes, &signature);
    assert_eq!(
        world.updater("1.0.0").tick(t0()),
        Some(Checked::NotVerified { in_a_row: 1 })
    );
}

#[test]
fn a_file_outside_the_releases_is_never_downloaded() {
    // DST-R50: latest.json can point the updater only at the Releases' own
    // files; anything else is a bad document, checked again next interval
    let world = World::new();
    world.publish("1.2.0");
    world.point_latest_at("http://127.0.0.2:9/Pagle.Companion_1.2.0_x64-setup.exe");
    let updater = world.updater("1.0.0");

    assert_eq!(updater.tick(t0()), Some(Checked::Failed));
    assert_eq!(world.downloads(), 0);
    assert_eq!(updater.waiting(), None);
}

#[test]
fn a_new_version_in_latest_json_resets_the_failed_verification_count() {
    // DST-R57: reset by a new version in latest.json
    let world = World::new();
    let bytes = b"MZ".to_vec();
    let forged = world.other_keys.sign(&bytes, &trusted_comment("1.2.0"));
    world.publish_signed("1.2.0", &bytes, &forged);
    let updater = world.updater("1.0.0");
    updater.tick(t0());
    updater.tick(t0() + 6 * HOUR);
    assert_eq!(updater.failed_verifications(), 2);

    let forged = world.other_keys.sign(&bytes, &trusted_comment("1.2.1"));
    world.publish_signed("1.2.1", &bytes, &forged);
    assert_eq!(
        updater.tick(t0() + 12 * HOUR),
        Some(Checked::NotVerified { in_a_row: 1 })
    );
}

#[test]
fn nothing_is_downloaded_while_the_same_or_a_higher_version_waits() {
    // DST-R53: a second check while 1.2.0 waits downloads nothing for 1.2.0
    // or lower
    let world = World::new();
    world.publish("1.2.0");
    let updater = world.updater("1.0.0");
    updater.tick(t0());
    assert_eq!(world.downloads(), 1);

    assert_eq!(
        updater.tick(t0() + 6 * HOUR),
        Some(Checked::Waiting(v("1.2.0")))
    );
    world.publish("1.1.0");
    assert_eq!(
        updater.tick(t0() + 12 * HOUR),
        Some(Checked::Waiting(v("1.2.0")))
    );
    assert_eq!(world.downloads(), 1);
    assert_eq!(world.checks(), 3);
}

#[test]
fn a_higher_version_replaces_the_waiting_installer() {
    // DST-R53, DST-R54: 1.3.0 replaces the waiting 1.2.0 installer, which is
    // deleted
    let world = World::new();
    world.publish("1.2.0");
    let updater = world.updater("1.0.0");
    updater.tick(t0());

    let bytes = world.publish("1.3.0");
    assert_eq!(
        updater.tick(t0() + 6 * HOUR),
        Some(Checked::Waiting(v("1.3.0")))
    );
    assert_eq!(fs::read(updater.waiting().unwrap().file).unwrap(), bytes);
    assert_eq!(
        world.state_files(),
        [
            "pagle-companion_1.3.0_x64-setup.exe",
            "pagle-companion_1.3.0_x64-setup.exe.sig",
            "updater.json",
        ]
    );
}

#[test]
fn a_kept_installer_waits_across_a_restart_until_the_companion_reaches_it() {
    // DST-R53, DST-R54: the installer is kept until installed; a waiting
    // installer not above the running version (the Companion was updated
    // another way) is deleted
    let world = World::new();
    world.publish("1.2.0");
    world.updater("1.0.0").tick(t0());

    let next_start = world.updater("1.0.0");
    assert_eq!(next_start.waiting(), None);
    assert_eq!(next_start.find_waiting().unwrap().version, v("1.2.0"));
    // It is waiting, so the check at start downloads nothing.
    assert_eq!(next_start.tick(t0()), Some(Checked::Waiting(v("1.2.0"))));
    assert_eq!(world.downloads(), 1);

    let updated = world.updater("1.2.0");
    assert_eq!(updated.find_waiting(), None);
    assert_eq!(world.state_files(), ["updater.json"]);
}

#[test]
fn a_waiting_installer_changed_after_download_fails_its_second_verification() {
    // DST-R54: verified again against the public key just before it runs; a
    // tampered one is deleted, and fetched again at the next check
    let world = World::new();
    world.publish("1.2.0");
    let updater = world.updater("1.0.0");
    updater.tick(t0());
    let waiting = updater.waiting().unwrap();
    fs::write(&waiting.file, b"MZ tampered").unwrap();

    assert!(matches!(
        updater.verify_waiting(&waiting),
        Err(release::VerifyError::BadSignature(_))
    ));
    updater.discard_waiting();
    assert_eq!(updater.waiting(), None);
    assert_eq!(world.state_files(), ["updater.json"]);

    assert_eq!(
        updater.tick(t0() + 6 * HOUR),
        Some(Checked::Waiting(v("1.2.0")))
    );
    assert_eq!(world.downloads(), 2);
}

#[test]
fn the_appimage_is_replaced_as_soon_as_it_verifies() {
    // DST-R53, DST-R54: on Linux the file at $APPIMAGE is the new version
    // once verified, written beside it and renamed over it
    let world = World::new();
    let appimage = world.tmp.path().join("pagle-companion.AppImage");
    fs::write(&appimage, b"old AppImage").unwrap();
    let bytes = world.publish_appimage("1.2.0");
    let updater = world.updater_with(
        "1.0.0",
        Keeper::AppImage {
            path: Some(appimage.clone()),
        },
        Hooks::default(),
    );

    assert_eq!(updater.tick(t0()), Some(Checked::Waiting(v("1.2.0"))));
    assert_eq!(fs::read(&appimage).unwrap(), bytes);
    assert_eq!(updater.waiting().unwrap().file, appimage);
    // Nothing is kept in the state directory on Linux.
    assert_eq!(world.state_files(), ["updater.json"]);

    // It waits until the restart: the same version isn't fetched again.
    updater.tick(t0() + 6 * HOUR);
    assert_eq!(world.downloads(), 1);
}

#[test]
fn without_appimage_it_checks_but_never_downloads() {
    // DST-R54: with APPIMAGE unset the Linux updater checks but never
    // downloads
    let world = World::new();
    world.publish("1.2.0");
    let updater = world.updater_with("1.0.0", Keeper::AppImage { path: None }, Hooks::default());

    assert_eq!(updater.tick(t0()), Some(Checked::CantDownload));
    assert_eq!(world.checks(), 1);
    assert_eq!(world.downloads(), 0);
}

#[cfg(unix)]
#[test]
fn a_read_only_appimage_folder_checks_but_never_downloads() {
    // DST-R54: when the AppImage's folder isn't writable the Linux updater
    // checks but never downloads
    use std::os::unix::fs::PermissionsExt;
    let world = World::new();
    world.publish("1.2.0");
    let dir = world.tmp.path().join("ro");
    fs::create_dir(&dir).unwrap();
    let appimage = dir.join("pagle-companion.AppImage");
    fs::write(&appimage, b"old").unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let updater = world.updater_with(
        "1.0.0",
        Keeper::AppImage {
            path: Some(appimage.clone()),
        },
        Hooks::default(),
    );

    let checked = updater.tick(t0());
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(checked, Some(Checked::CantDownload));
    assert_eq!(world.downloads(), 0);
    assert_eq!(fs::read(&appimage).unwrap(), b"old");
}

#[test]
fn the_hooks_hear_every_check_and_every_tick() {
    // DST-R51, DST-R57: every check is followed by the hook (the config read
    // is there, `update_required`), with its outcome; DST-R56: every tick
    // too, for the required-state config reads
    let world = World::new();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let ticks = Arc::new(Mutex::new(Vec::new()));
    let hooks = Hooks {
        after_check: {
            let heard = heard.clone();
            Box::new(move |checked| heard.lock().unwrap().push(checked.clone()))
        },
        ticked: {
            let ticks = ticks.clone();
            Box::new(move |now| ticks.lock().unwrap().push(now))
        },
    };
    let updater = world.updater_with("1.0.0", world.installer_keeper(), hooks);

    updater.tick(t0());
    updater.tick(t0() + MINUTE);
    world.publish("1.2.0");
    updater.tick(t0() + 6 * HOUR);
    assert_eq!(
        *heard.lock().unwrap(),
        [Checked::Failed, Checked::Waiting(v("1.2.0"))]
    );
    assert_eq!(
        *ticks.lock().unwrap(),
        [t0(), t0() + MINUTE, t0() + 6 * HOUR]
    );
}

#[test]
fn check_soon_checks_at_the_next_tick() {
    // DST-R56: an update check at once when update required is entered
    let world = World::new();
    world.publish("1.0.0");
    let updater = world.updater("1.0.0");
    updater.tick(t0());
    updater.check_soon();
    assert_eq!(updater.tick(t0() + MINUTE), Some(Checked::UpToDate));
    assert_eq!(world.checks(), 2);
}

/// Records what would have started, and starts nothing.
#[derive(Default)]
struct Recorder {
    launches: Mutex<Vec<install::Launch>>,
    fails: bool,
}

impl Recorder {
    fn failing() -> Self {
        Self {
            fails: true,
            ..Self::default()
        }
    }

    fn launches(&self) -> Vec<install::Launch> {
        self.launches.lock().unwrap().clone()
    }
}

impl install::Launcher for Recorder {
    fn launch(&self, launch: &install::Launch) -> std::io::Result<()> {
        if self.fails {
            return Err(std::io::Error::other("couldn't start"));
        }
        self.launches.lock().unwrap().push(launch.clone());
        Ok(())
    }
}

fn args(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// The kept installer of `version`, run with `line`.
fn installer_run(world: &World, version: &str, line: &str) -> install::Launch {
    install::Launch {
        program: world
            .state_dir()
            .join(format!("pagle-companion_{version}_x64-setup.exe")),
        command_line: install::CommandLine::Raw(line.into()),
    }
}

#[test]
fn nothing_waiting_installs_nothing() {
    // DST-R54: Restart to update exists only while an update waits; without
    // one nothing starts
    let world = World::new();
    world.publish("1.0.0");
    let updater = world.updater("1.0.0");
    updater.tick(t0());
    let recorder = Recorder::default();
    assert_eq!(updater.install(&recorder, &[]), Installed::NothingWaiting);
    assert!(recorder.launches().is_empty());
}

#[test]
fn restart_to_update_runs_the_verified_installer_passive_with_the_same_arguments() {
    // DST-R54, DST-R13: verified again, then run passive as an update that
    // starts the new version with the old arguments; the next start, the
    // new version, deletes the installer
    let world = World::new();
    world.publish("1.2.0");
    let updater = world.updater("1.0.0");
    updater.tick(t0());
    let recorder = Recorder::default();

    assert_eq!(
        updater.install(&recorder, &args(&["--autostart"])),
        Installed::Started(v("1.2.0"))
    );
    assert_eq!(
        recorder.launches(),
        [installer_run(
            &world,
            "1.2.0",
            "/P /UPDATE /R /ARGS --autostart"
        )]
    );

    let updated = world.updater("1.2.0");
    assert_eq!(
        updated.install_at_start(&recorder, &[]),
        Installed::NothingWaiting
    );
    assert_eq!(world.state_files(), ["updater.json"]);
    assert_eq!(recorder.launches().len(), 1);
}

#[test]
fn a_tampered_installer_is_refused_deleted_counted_and_fetched_again() {
    // DST-R54, DST-R53, DST-R57: a kept installer changed after download
    // fails its second verification, never runs, is deleted and counted in
    // updater.json, and is fetched again at the next check
    let world = World::new();
    world.publish("1.2.0");
    let updater = world.updater("1.0.0");
    updater.tick(t0());
    fs::write(updater.waiting().unwrap().file, b"MZ tampered").unwrap();
    let recorder = Recorder::default();

    assert_eq!(
        updater.install(&recorder, &[]),
        Installed::NotVerified { in_a_row: 1 }
    );
    assert!(recorder.launches().is_empty());
    assert_eq!(updater.waiting(), None);
    assert_eq!(world.state_files(), ["updater.json"]);
    assert_eq!(world.updater_json()["failedVerifications"], 1);

    assert_eq!(updater.tick(t0() + MINUTE), None);
    assert_eq!(
        updater.tick(t0() + 6 * HOUR),
        Some(Checked::Waiting(v("1.2.0")))
    );
    assert_eq!(world.downloads(), 2);
    assert_eq!(world.updater_json()["failedVerifications"], 0);
}

#[test]
fn a_start_with_an_update_waiting_installs_it_before_any_request() {
    // DST-R54, DST-R100: at the next start the waiting installer runs before
    // anything is scanned, checked or uploaded; a login start comes back
    // with --autostart (DST-R13)
    let world = World::new();
    world.publish("1.2.0");
    world.updater("1.0.0").tick(t0());
    let requests = world.requests().len();

    let next_start = world.updater("1.0.0");
    let recorder = Recorder::default();
    assert_eq!(
        next_start.install_at_start(&recorder, &args(&["--autostart"])),
        Installed::Started(v("1.2.0"))
    );
    assert_eq!(
        recorder.launches(),
        [installer_run(
            &world,
            "1.2.0",
            "/P /UPDATE /R /ARGS --autostart"
        )]
    );
    assert_eq!(world.requests().len(), requests);
}

#[test]
fn a_tampered_installer_at_start_is_counted_and_the_start_carries_on() {
    // DST-R54, DST-R57: at start too, a kept installer that fails its second
    // verification is deleted and counted; the check at start fetches it
    // again (DST-R51, DST-R53)
    let world = World::new();
    world.publish("1.2.0");
    let first = world.updater("1.0.0");
    first.tick(t0());
    fs::write(first.waiting().unwrap().file, b"MZ tampered").unwrap();

    let next_start = world.updater("1.0.0");
    let recorder = Recorder::default();
    assert_eq!(
        next_start.install_at_start(&recorder, &[]),
        Installed::NotVerified { in_a_row: 1 }
    );
    assert!(recorder.launches().is_empty());
    assert_eq!(world.state_files(), ["updater.json"]);

    assert_eq!(
        next_start.tick(t0() + MINUTE),
        Some(Checked::Waiting(v("1.2.0")))
    );
    assert_eq!(world.downloads(), 2);
}

#[test]
fn an_installer_that_cant_start_stays_waiting() {
    // DST-R54: the installer is kept until installed
    let world = World::new();
    world.publish("1.2.0");
    let updater = world.updater("1.0.0");
    updater.tick(t0());

    assert_eq!(
        updater.install(&Recorder::failing(), &[]),
        Installed::CouldntStart
    );
    let waiting = updater.waiting().unwrap();
    assert_eq!(waiting.version, v("1.2.0"));
    assert!(waiting.file.is_file());
}

#[test]
fn restart_to_update_on_linux_starts_the_replaced_appimage_with_the_same_arguments() {
    // DST-R54: the AppImage was replaced when it verified; Restart to update
    // starts the new file at $APPIMAGE with the same arguments
    let world = World::new();
    let appimage = world.tmp.path().join("pagle-companion.AppImage");
    fs::write(&appimage, b"old AppImage").unwrap();
    let bytes = world.publish_appimage("1.2.0");
    let updater = world.updater_with(
        "1.0.0",
        Keeper::AppImage {
            path: Some(appimage.clone()),
        },
        Hooks::default(),
    );
    updater.tick(t0());
    let recorder = Recorder::default();

    assert_eq!(
        updater.install(&recorder, &args(&["--autostart"])),
        Installed::Started(v("1.2.0"))
    );
    assert_eq!(
        recorder.launches(),
        [install::Launch {
            program: appimage.clone(),
            command_line: install::CommandLine::Args(vec!["--autostart".into()]),
        }]
    );
    assert_eq!(fs::read(&appimage).unwrap(), bytes);
}

#[test]
fn on_linux_nothing_is_installed_at_start() {
    // DST-R54: at a normal Linux start the replaced AppImage is what runs;
    // there is nothing more to install
    let world = World::new();
    let appimage = world.tmp.path().join("pagle-companion.AppImage");
    fs::write(&appimage, b"new AppImage").unwrap();
    let updater = world.updater_with(
        "1.2.0",
        Keeper::AppImage {
            path: Some(appimage),
        },
        Hooks::default(),
    );
    let recorder = Recorder::default();
    assert_eq!(
        updater.install_at_start(&recorder, &[]),
        Installed::NothingWaiting
    );
    assert!(recorder.launches().is_empty());
}

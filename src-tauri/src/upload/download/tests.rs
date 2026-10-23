//! The in-game downloads end to end, against a stub that serves the
//! backend's own bodies ([`FakeInGame`]): the timer's checks over a clock the
//! tests move, the queue, the requests' headers, gzip, the check, the writes
//! into temporary installs, `inGame`, and every reply of CMP-R50 a download
//! can get.

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use wiremock::{MockServer, ResponseTemplate};

use super::{DECODED_CAP, DownloadEnd};
use crate::api::Api;
use crate::catch_logs::Confirmed;
use crate::credential_store::testing::MemoryStore;
use crate::credential_store::{CredentialStore as _, Keychain};
use crate::device_flow::Approval;
use crate::in_game::backend_bodies::{
    self, ANGLERS_EXAMPLE, FOLLOWING_EXAMPLE, FOLLOWING_NONE, anglers_file,
};
use crate::in_game::content_vector::{self, content_file};
use crate::in_game::folders::fixture::{FaultyFs, Op, install};
use crate::in_game::folders::{DataFile, Qualifying, WriteReport, Writer, etag_of};
use crate::in_game::timer::{
    ANGLERS_REFETCH_AGE, CHECK_INTERVAL, CONTENT_POLL_INTERVAL, FOLLOWING_INTERVAL,
};
use crate::link::{Hooks, Link};
use crate::notify::{Notifications, Notifier};
use crate::readings::fixture::{LOG_A, account_folder, pagle_lua_text, save};
use crate::readings::{Reading, read};
use crate::state::{Answer, InGame, LogState, State, Store};
use crate::stub_server;
use crate::upload::fake_pagle::{FakeInGame, FakePagle, gzip, refusal};
use crate::upload::queue::Trigger;
use crate::upload::{Ctx, Env, RunEnd, Uploads};

const CREDENTIAL: &str = "pgc_test_credential";
const START: i64 = 1_791_800_000;
const PAGLE_TOC: &[u8] = b"## Interface: 11508\n## Title: Pagle\n";
const CHECK: i64 = CHECK_INTERVAL.as_secs() as i64;

#[derive(Default, Clone)]
struct Recorder(Arc<Mutex<Vec<String>>>);

impl Notifier for Recorder {
    fn show(&self, text: &str) {
        self.0.lock().unwrap().push(text.to_owned());
    }
}

/// What the downloads work with in tests: a clock the test moves, a backoff
/// that records its waits, and a writer over the harness's installs.
struct TestEnv {
    now: AtomicI64,
    sleeps: Mutex<Vec<Duration>>,
    writer: Arc<Writer>,
    installs: Arc<Mutex<Vec<String>>>,
}

impl TestEnv {
    fn installs(&self) -> Vec<String> {
        self.installs.lock().unwrap().clone()
    }
}

impl Env for TestEnv {
    fn read(&self, folder: &str) -> impl Future<Output = Reading> + Send {
        let reading = read::read(Path::new(folder));
        async move { reading }
    }

    fn sleep(&self, wait: Duration) -> impl Future<Output = ()> + Send {
        self.sleeps.lock().unwrap().push(wait);
        tokio::task::yield_now()
    }

    fn now(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }

    fn changed(&self) {}

    fn qualifying(&self) -> Qualifying {
        self.writer.qualifying(&self.installs())
    }

    fn data_files_hash_to(&self, file: DataFile, etag: &str) -> impl Future<Output = bool> + Send {
        let hashes_to = self.writer.data_files_hash_to(&self.installs(), file, etag);
        async move { hashes_to }
    }

    fn write(&self, file: DataFile, body: Vec<u8>) -> impl Future<Output = WriteReport> + Send {
        let report = self.writer.write(&self.installs(), file, &body);
        async move { report }
    }

    fn refresh_tocs(&self) {
        self.writer.refresh_tocs(&self.installs());
    }
}

struct Harness {
    tmp: tempfile::TempDir,
    server: MockServer,
    in_game: FakeInGame,
    api: Api,
    link: Link,
    state: Store,
    notifications: Notifications<Recorder>,
    env: TestEnv,
    uploads: Arc<Uploads>,
    confirmed: Confirmed,
    /// The installs with the Pagle addon, in order.
    flavors: Vec<String>,
    ops: Arc<Mutex<Vec<(Op, PathBuf)>>>,
}

impl Harness {
    /// Linked, with one install that has the Pagle addon.
    async fn new() -> Self {
        Self::build(true, |_, _| false).await
    }

    /// As [`Harness::new`], with the writes and deletes `fails` says yes to
    /// failing.
    async fn failing(fails: impl Fn(Op, &Path) -> bool + Send + Sync + 'static) -> Self {
        Self::build(true, fails).await
    }

    async fn build(
        linked: bool,
        fails: impl Fn(Op, &Path) -> bool + Send + Sync + 'static,
    ) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let server = stub_server::start().await;
        FakePagle::default().mount(&server).await;
        let in_game = FakeInGame::default();
        in_game.mount(&server).await;
        let folder = account_folder(tmp.path(), "ADA#1");
        save(&folder, &pagle_lua_text(LOG_A, 3));
        let folder_text = folder.to_string_lossy().into_owned();

        let fs = FaultyFs::new(fails);
        let ops = fs.ops();
        let writer = Arc::new(Writer::with_fs(Box::new(fs)));
        let installs = Arc::new(Mutex::new(Vec::new()));
        let os = MemoryStore::new("Windows Credential Manager");
        if linked {
            os.write(CREDENTIAL).unwrap();
        }
        let uploads = Arc::new(Uploads::new());
        let hooks = {
            let (cancel, post_link) = (uploads.clone(), uploads.clone());
            let (writer, known) = (writer.clone(), installs.clone());
            Hooks {
                cancel_queue: Box::new(move || cancel.cancel()),
                start_post_link_runs: Box::new(move || {
                    post_link.trigger(LOG_A, Trigger::PostLink);
                    post_link.owe_downloads();
                }),
                delete_data_addon_folders: Box::new(move || {
                    writer.delete_folders(&known.lock().unwrap())
                }),
                ..Hooks::default()
            }
        };
        let mut state = State {
            first_run_done: true,
            battle_tag: linked.then(|| "Ada#1234".into()),
            ..State::default()
        };
        state.logs.insert(
            LOG_A.into(),
            LogState {
                answer: Some(Answer::Upload),
                folder: None,
                characters: "Ada Brook".into(),
                last_folder: folder_text.clone(),
                whole_send_pending: false,
                last_run: None,
                last_uploaded_at: None,
            },
        );
        let mut h = Self {
            api: stub_server::api(&server),
            state: Store::new(tmp.path().join("state.json"), state),
            link: Link::new(Keychain::new(Box::new(os), None), hooks),
            notifications: Notifications::new(Recorder::default()),
            env: TestEnv {
                now: AtomicI64::new(START),
                sleeps: Mutex::default(),
                writer,
                installs,
            },
            confirmed: [(LOG_A.to_owned(), folder_text)].into(),
            tmp,
            server,
            in_game,
            uploads,
            flavors: Vec::new(),
            ops,
        };
        h.add_install("WoW/_classic_", Some(PAGLE_TOC));
        h
    }

    /// An install the scan found, with the Pagle addon if `pagle_toc` is
    /// given.
    fn add_install(&mut self, rel: &str, pagle_toc: Option<&[u8]>) -> String {
        let flavor = install(self.tmp.path(), rel, pagle_toc);
        self.env.installs.lock().unwrap().push(flavor.clone());
        if pagle_toc.is_some() {
            self.flavors.push(flavor.clone());
        }
        flavor
    }

    fn ctx(&self) -> Ctx<'_, Recorder, TestEnv> {
        Ctx {
            account: self
                .link
                .account(&self.api, &self.state, &self.notifications),
            state: &self.state,
            version: "1.0.0",
            confirmed: &self.confirmed,
            env: &self.env,
        }
    }

    /// A check of the timer, then the queue run empty.
    async fn check(&self) -> Vec<RunEnd> {
        self.uploads.in_game_check(&self.ctx());
        self.uploads.run_queued(&self.ctx()).await
    }

    fn advance(&self, seconds: i64) {
        self.env.now.fetch_add(seconds, Ordering::SeqCst);
    }

    fn now(&self) -> i64 {
        self.env.now.load(Ordering::SeqCst)
    }

    fn path(&self, flavor: &str, file: DataFile) -> PathBuf {
        Path::new(flavor)
            .join("Interface/AddOns")
            .join(file.folder())
            .join(file.file_name())
    }

    /// `file`'s data file in the install `flavor`.
    fn data_file(&self, flavor: &str, file: DataFile) -> Option<Vec<u8>> {
        fs::read(self.path(flavor, file)).ok()
    }

    fn in_game_state(&self) -> InGame {
        self.state.read(|s| s.in_game.clone())
    }

    /// The writes the writer made to `file`'s data files so far.
    fn writes_of(&self, file: DataFile) -> usize {
        let tmp = format!(".{}.tmp", file.file_name());
        self.ops
            .lock()
            .unwrap()
            .iter()
            .filter(|(op, path)| *op == Op::WriteSynced && path.ends_with(&tmp))
            .count()
    }

    fn requests(&self, file: DataFile) -> usize {
        self.in_game.received_of(file).len()
    }
}

fn ends(ends: &[DownloadEnd]) -> Vec<RunEnd> {
    ends.iter().cloned().map(RunEnd::Download).collect()
}

/// The vector's `good` file, the stub's Content body at first.
fn good_content() -> Vec<u8> {
    content_file("good").into_bytes()
}

/// `503 content_unavailable` before the first content build (HEV-R37, HEV
/// Contracts, Endpoints).
fn content_unavailable(retry_after: &str) -> ResponseTemplate {
    ResponseTemplate::new(503)
        .insert_header("Retry-After", retry_after)
        .set_body_json(json!({ "reason": "content_unavailable",
            "message": "The in-game content isn't ready yet. Try again in a minute." }))
}

fn rate_limited(retry_after: &str) -> ResponseTemplate {
    ResponseTemplate::new(429)
        .insert_header("Retry-After", retry_after)
        .set_body_json(
            json!({ "reason": "rate_limited", "message": "Too many requests. Try again later." }),
        )
}

// --- the timer (CMP-R81, IGV-R50, IGV-R51)

#[tokio::test]
async fn at_start_every_download_runs_once_and_is_written_and_kept() {
    // CMP-R81: the Following, Anglers and Content downloads at start; a
    // download is successful once written in at least one install, and its
    // time (and the Anglers and Content ETags) goes into inGame
    let h = Harness::new().await;
    assert_eq!(
        h.check().await,
        ends(&[
            DownloadEnd::Written(1),
            DownloadEnd::Written(1),
            DownloadEnd::Written(1)
        ])
    );
    let flavor = &h.flavors[0];
    assert_eq!(
        h.data_file(flavor, DataFile::Following).unwrap(),
        FOLLOWING_EXAMPLE.as_bytes()
    );
    assert_eq!(
        h.data_file(flavor, DataFile::Anglers).unwrap(),
        ANGLERS_EXAMPLE.as_bytes()
    );
    assert_eq!(
        h.data_file(flavor, DataFile::Content).unwrap(),
        good_content()
    );
    assert_eq!(
        h.in_game_state(),
        InGame {
            following_at: Some(START),
            anglers_at: Some(START),
            anglers_etag: Some(etag_of(ANGLERS_EXAMPLE.as_bytes())),
            content_at: Some(START),
            content_etag: Some(etag_of(&good_content())),
        }
    );
    // Once only.
    assert_eq!(h.check().await, []);
    assert_eq!(h.in_game.received().len(), 3);
}

#[tokio::test]
async fn in_game_is_kept_in_state_json_under_its_spec_names() {
    // CMP Data: state.json holds inGame.followingAt, anglersAt and
    // anglersEtag, and (HEV-R50) contentAt and contentEtag
    let h = Harness::new().await;
    h.check().await;
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(h.tmp.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(
        json["inGame"],
        json!({ "followingAt": START, "anglersAt": START,
                "anglersEtag": etag_of(ANGLERS_EXAMPLE.as_bytes()),
                "contentAt": START, "contentEtag": etag_of(&good_content()) })
    );
}

#[tokio::test]
async fn every_download_carries_the_credential_version_and_gzip() {
    // CMP-R81: every download sends Accept-Encoding: gzip, the version
    // header (DST-R60) and the credential (CMP-R49)
    let h = Harness::new().await;
    h.check().await;
    let received = h.in_game.received();
    assert_eq!(received.len(), 3);
    for download in received {
        assert_eq!(download.accept_encoding.as_deref(), Some("gzip"));
        assert_eq!(download.version.as_deref(), Some("1.0.0"));
        assert_eq!(download.authorization, Some(format!("Bearer {CREDENTIAL}")));
        // No ETag kept yet.
        assert_eq!(download.if_none_match, None);
    }
}

#[tokio::test]
async fn a_gzip_body_is_decoded_and_written_byte_for_byte() {
    // CMP-R81, IGV-R34, IGV-R40: the body after gzip decoding is written
    // byte for byte
    let h = Harness::new().await;
    let body = anglers_file(1_791_730_000, &[]);
    h.in_game.set_body(DataFile::Anglers, body.clone());
    h.check().await;
    assert_eq!(h.in_game.gzipped(), [true, true, true]);
    assert_eq!(
        h.data_file(&h.flavors[0], DataFile::Anglers).unwrap(),
        body.as_bytes()
    );
}

#[tokio::test]
async fn following_runs_again_every_hour() {
    // CMP-R81, IGV-R50: the Following download then every following
    // interval (1 h); checks between send nothing
    let h = Harness::new().await;
    h.check().await;
    h.advance(FOLLOWING_INTERVAL - 1);
    assert_eq!(h.check().await, []);
    h.advance(1);
    // Content's poll interval is the same hour; its file is unchanged.
    assert_eq!(
        h.check().await,
        ends(&[DownloadEnd::Written(1), DownloadEnd::NotModified])
    );
    assert_eq!(h.requests(DataFile::Following), 2);
    assert_eq!(h.requests(DataFile::Anglers), 1);
    assert_eq!(h.in_game_state().following_at, Some(h.now()));
}

#[tokio::test]
async fn anglers_runs_again_after_12_hours_with_if_none_match_and_a_304_writes_nothing() {
    // CMP-R81, IGV-R51: the Anglers download once its last success is the
    // re-fetch age (12 h) old, with If-None-Match set to the kept ETag; a
    // 304 is successful, updates anglersAt and writes no data file
    let h = Harness::new().await;
    h.check().await;
    let etag = etag_of(ANGLERS_EXAMPLE.as_bytes());
    let written = h.writes_of(DataFile::Anglers);
    h.advance(ANGLERS_REFETCH_AGE - CHECK);
    h.check().await;
    assert_eq!(h.requests(DataFile::Anglers), 1);
    h.advance(CHECK);
    let ended = h.check().await;
    assert!(ended.contains(&RunEnd::Download(DownloadEnd::NotModified)));
    let anglers = h.in_game.received_of(DataFile::Anglers);
    assert_eq!(anglers.len(), 2);
    assert_eq!(anglers[1].if_none_match.as_deref(), Some(etag.as_str()));
    assert_eq!(h.writes_of(DataFile::Anglers), written);
    let in_game = h.in_game_state();
    assert_eq!(in_game.following_at, Some(h.now() - CHECK));
    assert_eq!(in_game.anglers_at, Some(h.now()));
    assert_eq!(in_game.anglers_etag, Some(etag));
}

#[tokio::test]
async fn following_never_sends_if_none_match() {
    // IGV-R31: Following has no ETag; CMP-R81 sends If-None-Match on the
    // Anglers download only
    let h = Harness::new().await;
    h.check().await;
    h.advance(FOLLOWING_INTERVAL);
    h.check().await;
    assert!(
        h.in_game
            .received_of(DataFile::Following)
            .iter()
            .all(|d| d.if_none_match.is_none())
    );
}

#[tokio::test]
async fn right_after_linking_every_download_runs_without_if_none_match() {
    // CMP-R101: right after any link, the in-game downloads (CMP-R81:
    // Content "right after linking" too); CMP-R64: Link again… clears
    // inGame, so they send no If-None-Match
    let h = Harness::new().await;
    h.check().await;
    h.advance(60);
    h.ctx()
        .account
        .store_link(Approval {
            credential: CREDENTIAL.into(),
            battle_tag: "Ada#1234".into(),
        })
        .await
        .unwrap();
    assert_eq!(h.in_game_state(), InGame::default());
    h.check().await;
    assert_eq!(h.requests(DataFile::Following), 2);
    let anglers = h.in_game.received_of(DataFile::Anglers);
    assert_eq!(anglers.len(), 2);
    assert_eq!(anglers[1].if_none_match, None);
    assert_eq!(h.in_game_state().anglers_at, Some(h.now()));
    let content = h.in_game.received_of(DataFile::Content);
    assert_eq!(content.len(), 2);
    assert_eq!(content[1].if_none_match, None);
    assert_eq!(h.in_game_state().content_at, Some(h.now()));
}

#[tokio::test]
async fn an_edited_anglers_lua_sends_no_if_none_match_and_gets_the_full_body_everywhere() {
    // CMP-R81: If-None-Match only when Anglers.lua in every qualifying
    // install hashes to the kept ETag; otherwise a full body, written to
    // every install (CMP-R83)
    let mut h = Harness::new().await;
    let other = h.add_install("WoW/_classic_era_", Some(PAGLE_TOC));
    h.check().await;
    fs::write(h.path(&other, DataFile::Anglers), b"edited").unwrap();
    h.advance(ANGLERS_REFETCH_AGE);
    let ended = h.check().await;
    assert!(ended.contains(&RunEnd::Download(DownloadEnd::Written(2))));
    let anglers = h.in_game.received_of(DataFile::Anglers);
    assert_eq!(anglers[1].if_none_match, None);
    for flavor in &h.flavors {
        assert_eq!(
            h.data_file(flavor, DataFile::Anglers).unwrap(),
            ANGLERS_EXAMPLE.as_bytes()
        );
    }
}

#[tokio::test]
async fn a_deleted_anglers_lua_makes_the_download_due_at_once_without_if_none_match() {
    // CMP-R80: an install that qualifies but lacks a data file makes that
    // download due at once; CMP-R81: and it gets a full body
    let h = Harness::new().await;
    h.check().await;
    let flavor = &h.flavors[0];
    fs::remove_file(h.path(flavor, DataFile::Anglers)).unwrap();
    h.advance(CHECK);
    assert_eq!(h.check().await, ends(&[DownloadEnd::Written(1)]));
    let anglers = h.in_game.received_of(DataFile::Anglers);
    assert_eq!(anglers.len(), 2);
    assert_eq!(anglers[1].if_none_match, None);
    assert_eq!(
        h.data_file(flavor, DataFile::Anglers).unwrap(),
        ANGLERS_EXAMPLE.as_bytes()
    );
}

#[tokio::test]
async fn a_new_install_with_the_pagle_addon_gets_every_file_at_the_next_check() {
    // CMP-R80: an install that newly qualifies lacks every data file, so
    // every download is due at the next check
    let mut h = Harness::new().await;
    h.check().await;
    let new = h.add_install("WoW/_anniversary_", Some(PAGLE_TOC));
    h.advance(CHECK);
    assert_eq!(
        h.check().await,
        ends(&[
            DownloadEnd::Written(2),
            DownloadEnd::Written(2),
            DownloadEnd::Written(2)
        ])
    );
    assert_eq!(
        h.data_file(&new, DataFile::Content).unwrap(),
        good_content()
    );
    assert_eq!(
        h.data_file(&new, DataFile::Following).unwrap(),
        FOLLOWING_EXAMPLE.as_bytes()
    );
    assert_eq!(
        h.data_file(&new, DataFile::Anglers).unwrap(),
        ANGLERS_EXAMPLE.as_bytes()
    );
}

#[tokio::test]
async fn no_qualifying_install_means_no_in_game_request() {
    // CMP-R80: while no install qualifies, the Companion makes no in-game
    // download
    let mut h = Harness::new().await;
    h.env.installs.lock().unwrap().clear();
    h.flavors.clear();
    h.add_install("WoW/_classic_era_", None);
    assert_eq!(h.check().await, []);
    assert!(h.in_game.received().is_empty());
    // Once the addon is installed there, both are due.
    fs::create_dir_all(
        h.tmp
            .path()
            .join("WoW/_classic_era_/Interface/AddOns/Pagle"),
    )
    .unwrap();
    fs::write(
        h.tmp
            .path()
            .join("WoW/_classic_era_/Interface/AddOns/Pagle/Pagle.toc"),
        PAGLE_TOC,
    )
    .unwrap();
    h.advance(CHECK);
    assert_eq!(h.check().await.len(), 3);
}

#[tokio::test]
async fn nothing_is_downloaded_while_not_linked() {
    // IGV-R53: an unlinked Companion downloads nothing
    let h = Harness::build(false, |_, _| false).await;
    assert_eq!(h.check().await, []);
    assert!(h.in_game.received().is_empty());
}

#[tokio::test]
async fn every_check_refreshes_the_tocs() {
    // CMP-R83: at every in-game check, a TOC whose copied lines differ from
    // Pagle.toc's is rewritten
    let h = Harness::new().await;
    h.check().await;
    let flavor = &h.flavors[0];
    fs::write(
        Path::new(flavor).join("Interface/AddOns/Pagle/Pagle.toc"),
        b"## Interface: 11600\n",
    )
    .unwrap();
    h.advance(CHECK);
    assert_eq!(h.check().await, []);
    let toc = fs::read_to_string(
        Path::new(flavor).join("Interface/AddOns/Pagle_Following/Pagle_Following.toc"),
    )
    .unwrap();
    assert!(toc.starts_with("## Interface: 11600\n"), "{toc}");
}

// --- the queue (CMP-R46)

#[tokio::test]
async fn an_upload_run_waiting_goes_before_a_due_download() {
    // CMP-R46: downloads start only when no upload run is waiting
    let h = Harness::new().await;
    h.uploads.in_game_check(&h.ctx());
    h.uploads.trigger(LOG_A, Trigger::Start);
    let ended = h.uploads.run_queued(&h.ctx()).await;
    assert_eq!(ended.len(), 4);
    assert!(matches!(ended[0], RunEnd::Recorded(_)), "{ended:?}");
    let paths: Vec<String> = h
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect();
    let first_download = paths.iter().position(|p| p.contains("in-game")).unwrap();
    let last_upload = paths.iter().rposition(|p| p.contains("uploads")).unwrap();
    assert!(last_upload < first_download, "{paths:?}");
}

// --- checking and writing (CMP-R82, CMP-R83)

#[tokio::test]
async fn a_body_that_fails_its_check_is_never_written_and_waits_one_following_interval() {
    // CMP-R82: a body that fails is never written, the data files on disk
    // stay, and that download isn't tried again before one following
    // interval has passed
    let h = Harness::new().await;
    h.check().await;
    let bad = FOLLOWING_EXAMPLE.replace("v = 1,", "v = 2,");
    h.in_game.set_body(DataFile::Following, bad);
    h.advance(FOLLOWING_INTERVAL);
    let failed_at = h.now();
    assert_eq!(
        h.check().await,
        ends(&[DownloadEnd::Rejected, DownloadEnd::NotModified])
    );
    assert_eq!(
        h.data_file(&h.flavors[0], DataFile::Following).unwrap(),
        FOLLOWING_EXAMPLE.as_bytes()
    );
    assert_eq!(h.in_game_state().following_at, Some(START));
    for _ in 0..11 {
        h.advance(CHECK);
        h.check().await;
    }
    assert_eq!(h.now(), failed_at + FOLLOWING_INTERVAL - CHECK);
    assert_eq!(h.requests(DataFile::Following), 2);
    h.in_game.set_body(DataFile::Following, FOLLOWING_NONE);
    h.advance(CHECK);
    assert_eq!(
        h.check().await,
        ends(&[DownloadEnd::Written(1), DownloadEnd::NotModified])
    );
    assert_eq!(
        h.data_file(&h.flavors[0], DataFile::Following).unwrap(),
        FOLLOWING_NONE.as_bytes()
    );
}

#[tokio::test]
async fn a_body_past_the_decoded_cap_is_rejected_without_inflating_it_whole() {
    // CMP-R82, IGV-R41: at most the Anglers cap of bytes; the gzip body is
    // decoded only up to it
    let h = Harness::new().await;
    let bomb = gzip(&vec![b' '; DECODED_CAP + 1]);
    h.in_game.script(
        DataFile::Anglers,
        vec![Some(
            ResponseTemplate::new(200)
                .insert_header("Content-Encoding", "gzip")
                .set_body_bytes(bomb),
        )],
    );
    assert_eq!(
        h.check().await,
        ends(&[
            DownloadEnd::Written(1),
            DownloadEnd::Rejected,
            DownloadEnd::Written(1)
        ])
    );
    assert_eq!(h.data_file(&h.flavors[0], DataFile::Anglers), None);
    assert_eq!(h.in_game_state().anglers_at, None);
}

#[tokio::test]
async fn every_body_the_backend_writes_is_downloaded_and_written() {
    // CMP-R82, IGV-R41: the backend's own bodies pass and are written byte
    // for byte, blocks emptied by trimming (IGV-R33) included
    let h = Harness::new().await;
    for (what, body) in backend_bodies::anglers() {
        h.in_game.set_body(DataFile::Anglers, body.clone());
        fs::remove_file(h.path(&h.flavors[0], DataFile::Anglers)).ok();
        h.advance(CHECK);
        let ended = h.check().await;
        assert!(
            ended.contains(&RunEnd::Download(DownloadEnd::Written(1))),
            "{what}: {ended:?}"
        );
        assert_eq!(
            h.data_file(&h.flavors[0], DataFile::Anglers).unwrap(),
            body.as_bytes(),
            "{what}"
        );
    }
    for body in backend_bodies::FOLLOWING {
        h.in_game.set_body(DataFile::Following, body);
        h.advance(FOLLOWING_INTERVAL);
        h.check().await;
        assert_eq!(
            h.data_file(&h.flavors[0], DataFile::Following).unwrap(),
            body.as_bytes()
        );
    }
}

#[tokio::test]
async fn a_body_written_nowhere_isnt_successful_and_waits_one_following_interval() {
    // CMP-R81, CMP-R83: a valid body that can't be written to any install
    // isn't successful, shows the write-failure message, and isn't tried
    // again before one following interval has passed
    let h = Harness::failing(|op, path| {
        op == Op::WriteSynced && path.to_string_lossy().contains("Following.lua")
    })
    .await;
    assert_eq!(
        h.check().await,
        ends(&[
            DownloadEnd::Unwritten,
            DownloadEnd::Written(1),
            DownloadEnd::Written(1)
        ])
    );
    assert_eq!(h.in_game_state().following_at, None);
    let failures = h.env.writer.failures();
    assert_eq!(failures.len(), 1);
    assert!(
        failures[0]
            .text
            .starts_with("Couldn't write the in-game files to "),
        "{failures:?}"
    );
    for _ in 0..11 {
        h.advance(CHECK);
        h.check().await;
    }
    assert_eq!(h.requests(DataFile::Following), 1);
    h.advance(CHECK);
    assert_eq!(
        h.check().await,
        ends(&[DownloadEnd::Unwritten, DownloadEnd::NotModified])
    );
    assert_eq!(h.requests(DataFile::Following), 2);
}

#[tokio::test]
async fn a_body_written_in_one_install_is_successful() {
    // CMP-R81: successful once written in at least one install; CMP-R83: the
    // failed install keeps its message
    let mut h = Harness::failing(|op, path| {
        op == Op::WriteSynced && path.to_string_lossy().contains("_classic_era_")
    })
    .await;
    h.add_install("WoW/_classic_era_", Some(PAGLE_TOC));
    assert_eq!(
        h.check().await,
        ends(&[
            DownloadEnd::Written(1),
            DownloadEnd::Written(1),
            DownloadEnd::Written(1)
        ])
    );
    assert_eq!(h.in_game_state().following_at, Some(START));
    assert_eq!(h.env.writer.failures().len(), 3);
}

// --- failed downloads (CMP-R50, CMP-R84)

#[tokio::test]
async fn credential_invalid_unlinks_and_deletes_the_three_folders() {
    // CMP-R84, CMP-R65, CMP-R85: credential_invalid is the no-longer-linked
    // state, and the three data addon folders are deleted
    let h = Harness::new().await;
    h.check().await;
    h.in_game.script(
        DataFile::Following,
        vec![Some(stub_server::refusal(401, "credential_invalid"))],
    );
    h.advance(FOLLOWING_INTERVAL);
    assert_eq!(h.check().await, ends(&[DownloadEnd::Interrupted]));
    assert!(!h.ctx().account.is_linked());
    let addons = Path::new(&h.flavors[0]).join("Interface/AddOns");
    assert!(!addons.join("Pagle_Following").exists());
    assert!(!addons.join("Pagle_Anglers").exists());
    assert!(!addons.join("Pagle_Content").exists());
    assert_eq!(h.in_game_state(), InGame::default());
    // IGV-R53: nothing more is downloaded.
    h.advance(ANGLERS_REFETCH_AGE);
    assert_eq!(h.check().await, []);
}

#[tokio::test]
async fn suspended_and_update_required_stop_downloads_and_keep_the_files() {
    // CMP-R84: suspended and update_required stop it with everything else;
    // the files stay (CMP-R124)
    for code in ["suspended", "update_required"] {
        let h = Harness::new().await;
        h.check().await;
        h.in_game
            .script(DataFile::Following, vec![Some(refusal(403, code))]);
        h.advance(ANGLERS_REFETCH_AGE);
        // Anglers, queued behind it, is cancelled with the queue.
        assert_eq!(h.check().await, ends(&[DownloadEnd::Interrupted]), "{code}");
        assert!(h.ctx().account.is_stopped(), "{code}");
        assert_eq!(
            h.data_file(&h.flavors[0], DataFile::Anglers).unwrap(),
            ANGLERS_EXAMPLE.as_bytes()
        );
        assert!(h.data_file(&h.flavors[0], DataFile::Following).is_some());
        assert_eq!(
            h.data_file(&h.flavors[0], DataFile::Content).unwrap(),
            good_content()
        );
        // CMP-R81: nothing during the stop.
        h.advance(CHECK);
        assert_eq!(h.check().await, [], "{code}");
        assert_eq!(h.in_game.received().len(), 4, "{code}");
    }
}

#[tokio::test]
async fn a_429_drops_the_download_until_retry_after_and_the_queue_doesnt_wait() {
    // CMP-R50: on a download, 429 drops it and it isn't due again before
    // Retry-After has passed; the queue doesn't wait
    let h = Harness::new().await;
    h.in_game
        .script(DataFile::Anglers, vec![Some(rate_limited("600"))]);
    assert_eq!(
        h.check().await,
        ends(&[
            DownloadEnd::Written(1),
            DownloadEnd::RateLimited,
            DownloadEnd::Written(1)
        ])
    );
    assert!(h.env.sleeps.lock().unwrap().is_empty());
    h.advance(599);
    assert_eq!(h.check().await, []);
    h.advance(1);
    assert_eq!(h.check().await, ends(&[DownloadEnd::Written(1)]));
    assert_eq!(h.requests(DataFile::Anglers), 2);
}

#[tokio::test]
async fn a_5xx_waits_the_backoff_then_the_download_runs_again() {
    // CMP-R84, CMP-R50, CMP-R51: a 5xx waits the retry backoff, then the
    // download is sent again
    let h = Harness::new().await;
    h.in_game
        .script(DataFile::Following, vec![Some(ResponseTemplate::new(503))]);
    let ended = h.check().await;
    assert_eq!(
        ended,
        ends(&[
            DownloadEnd::Retry,
            DownloadEnd::Written(1),
            DownloadEnd::Written(1),
            DownloadEnd::Written(1)
        ])
    );
    assert_eq!(*h.env.sleeps.lock().unwrap(), [Duration::from_secs(30)]);
    assert_eq!(h.requests(DataFile::Following), 2);
    assert_eq!(h.in_game_state().following_at, Some(START));
}

#[tokio::test]
async fn a_body_that_doesnt_decode_counts_as_a_lost_connection() {
    // CMP-R50, CMP-R84: a 200 whose gzip body breaks off can't be told from
    // a connection lost mid-body, so it waits the retry backoff and is sent
    // again; nothing is written from it
    let h = Harness::new().await;
    let mut broken = gzip(FOLLOWING_EXAMPLE.as_bytes());
    broken.truncate(broken.len() / 2);
    h.in_game.script(
        DataFile::Following,
        vec![Some(
            ResponseTemplate::new(200)
                .insert_header("Content-Encoding", "gzip")
                .set_body_bytes(broken),
        )],
    );
    assert_eq!(
        h.check().await,
        ends(&[
            DownloadEnd::Retry,
            DownloadEnd::Written(1),
            DownloadEnd::Written(1),
            DownloadEnd::Written(1)
        ])
    );
    assert_eq!(
        h.data_file(&h.flavors[0], DataFile::Following).unwrap(),
        FOLLOWING_EXAMPLE.as_bytes()
    );
}

#[tokio::test]
async fn any_other_refusal_is_due_again_at_the_next_check() {
    // CMP-R50: for a download, "ends the run" drops it until the timer's next
    // check (CMP-R81)
    let h = Harness::new().await;
    h.in_game
        .script(DataFile::Following, vec![Some(ResponseTemplate::new(404))]);
    assert_eq!(
        h.check().await,
        ends(&[
            DownloadEnd::Failed,
            DownloadEnd::Written(1),
            DownloadEnd::Written(1)
        ])
    );
    h.advance(CHECK);
    assert_eq!(h.check().await, ends(&[DownloadEnd::Written(1)]));
}

#[tokio::test]
async fn unlink_clears_in_game_and_deletes_the_folders() {
    // CMP Data: Unlink clears inGame; CMP-R85: and deletes the three
    // folders (PUSH-R91: the addon then reads Pagle_Content as missing)
    let h = Harness::new().await;
    h.check().await;
    h.ctx().account.unlink().await;
    assert_eq!(h.in_game_state(), InGame::default());
    let addons = Path::new(&h.flavors[0]).join("Interface/AddOns");
    for file in DataFile::ALL {
        assert!(!addons.join(file.folder()).exists(), "{file:?}");
    }
    assert!(addons.join("Pagle/Pagle.toc").is_file());
    assert_eq!(h.check().await, []);
}

// --- the Content download (CMP-R81, HEV-R37, HEV-R50, HEV-R51)

#[tokio::test]
async fn content_runs_every_hour_and_a_304_keeps_the_file() {
    // CMP-R81, HEV Tunables: the Content download every content poll
    // interval (1 h), with If-None-Match set to the kept ETag (the SHA-256
    // of Content.lua, HEV-R37); a 304 is successful, updates contentAt and
    // writes nothing
    let h = Harness::new().await;
    h.check().await;
    let etag = etag_of(&good_content());
    let written = h.writes_of(DataFile::Content);
    h.advance(CONTENT_POLL_INTERVAL - CHECK);
    h.check().await;
    assert_eq!(h.requests(DataFile::Content), 1);
    h.advance(CHECK);
    let ended = h.check().await;
    assert!(ended.contains(&RunEnd::Download(DownloadEnd::NotModified)));
    let content = h.in_game.received_of(DataFile::Content);
    assert_eq!(content.len(), 2);
    assert_eq!(content[0].if_none_match, None);
    assert_eq!(content[1].if_none_match.as_deref(), Some(etag.as_str()));
    assert_eq!(h.writes_of(DataFile::Content), written);
    assert_eq!(
        h.data_file(&h.flavors[0], DataFile::Content).unwrap(),
        good_content()
    );
    let in_game = h.in_game_state();
    assert_eq!(in_game.content_at, Some(h.now()));
    assert_eq!(in_game.content_etag, Some(etag));
}

#[tokio::test]
async fn a_new_content_build_is_written_byte_for_byte_and_its_etag_kept() {
    // HEV-R37: a new build always makes a new body, answered 200 to the old
    // ETag; HEV-R50: written byte for byte, payload and sig unchanged
    let h = Harness::new().await;
    h.check().await;
    let big = content_file("big");
    h.in_game.set_body(DataFile::Content, big.clone());
    h.advance(CONTENT_POLL_INTERVAL);
    let ended = h.check().await;
    assert!(ended.contains(&RunEnd::Download(DownloadEnd::Written(1))));
    let content = h.in_game.received_of(DataFile::Content);
    assert_eq!(
        content[1].if_none_match,
        Some(etag_of(&good_content())),
        "sent with the old ETag"
    );
    assert_eq!(
        h.data_file(&h.flavors[0], DataFile::Content).unwrap(),
        big.as_bytes()
    );
    assert_eq!(
        h.in_game_state().content_etag,
        Some(etag_of(big.as_bytes()))
    );
}

#[tokio::test]
async fn an_edited_or_deleted_content_lua_gets_a_full_body_without_if_none_match() {
    // CMP-R81: If-None-Match only when Content.lua of every qualifying
    // install hashes to the kept ETag; CMP-R80: a missing one is due at once
    let h = Harness::new().await;
    h.check().await;
    let flavor = &h.flavors[0];
    fs::remove_file(h.path(flavor, DataFile::Content)).unwrap();
    h.advance(CHECK);
    assert_eq!(h.check().await, ends(&[DownloadEnd::Written(1)]));
    fs::write(h.path(flavor, DataFile::Content), b"edited").unwrap();
    h.advance(CONTENT_POLL_INTERVAL);
    let ended = h.check().await;
    assert!(ended.contains(&RunEnd::Download(DownloadEnd::Written(1))));
    let content = h.in_game.received_of(DataFile::Content);
    assert_eq!(content.len(), 3);
    assert!(content.iter().all(|d| d.if_none_match.is_none()));
    assert_eq!(
        h.data_file(flavor, DataFile::Content).unwrap(),
        good_content()
    );
}

#[tokio::test]
async fn content_unavailable_holds_the_download_off_until_retry_after() {
    // HEV-R37: 503 content_unavailable with Retry-After before the first
    // build; it costs a rate-limit token (HEV-R38), so it is dropped and not
    // sent again before Retry-After has passed, as a 429 (CMP-R50); the queue
    // doesn't wait, and nothing is written
    let h = Harness::new().await;
    h.in_game
        .script(DataFile::Content, vec![Some(content_unavailable("60"))]);
    assert_eq!(
        h.check().await,
        ends(&[
            DownloadEnd::Written(1),
            DownloadEnd::Written(1),
            DownloadEnd::Unavailable
        ])
    );
    assert!(h.env.sleeps.lock().unwrap().is_empty());
    assert_eq!(h.data_file(&h.flavors[0], DataFile::Content), None);
    assert_eq!(h.in_game_state().content_at, None);
    // Lacking its file doesn't make it due before Retry-After.
    h.advance(59);
    assert_eq!(h.check().await, []);
    assert_eq!(h.requests(DataFile::Content), 1);
    h.advance(1);
    assert_eq!(h.check().await, ends(&[DownloadEnd::Written(1)]));
    assert_eq!(
        h.data_file(&h.flavors[0], DataFile::Content).unwrap(),
        good_content()
    );
}

#[tokio::test]
async fn any_other_503_is_a_5xx() {
    // CMP-R50: any 5xx (JSON or not) waits the retry backoff, then the
    // download runs again; only content_unavailable is held off (HEV-R37)
    let h = Harness::new().await;
    h.in_game.script(
        DataFile::Content,
        vec![Some(
            ResponseTemplate::new(503).set_body_json(json!({ "reason": "maintenance" })),
        )],
    );
    assert_eq!(
        h.check().await,
        ends(&[
            DownloadEnd::Written(1),
            DownloadEnd::Written(1),
            DownloadEnd::Retry,
            DownloadEnd::Written(1)
        ])
    );
    assert_eq!(*h.env.sleeps.lock().unwrap(), [Duration::from_secs(30)]);
}

#[tokio::test]
async fn a_content_body_that_fails_its_check_is_never_written_and_waits() {
    // HEV-R51, CMP-R82: a body outside the grammar (the vector's functionCall
    // and singleQuote, and every file under bad/) is never written, the file
    // on disk stays, and the download isn't tried again before one following
    // interval has passed
    let h = Harness::new().await;
    h.check().await;
    let mut bodies: Vec<(String, Vec<u8>)> = ["functionCall", "singleQuote"]
        .iter()
        .map(|name| (name.to_string(), content_file(name).into_bytes()))
        .collect();
    bodies.extend(
        content_vector::BAD
            .iter()
            .map(|(name, body)| (name.to_string(), body.to_vec())),
    );
    for (name, body) in bodies {
        h.in_game.set_body(DataFile::Content, body);
        h.advance(FOLLOWING_INTERVAL);
        let ended = h.check().await;
        assert!(
            ended.contains(&RunEnd::Download(DownloadEnd::Rejected)),
            "{name}: {ended:?}"
        );
        assert_eq!(
            h.data_file(&h.flavors[0], DataFile::Content).unwrap(),
            good_content(),
            "{name}"
        );
        assert_eq!(h.in_game_state().content_at, Some(START), "{name}");
        // Held one following interval, though its last success is old.
        let requests = h.requests(DataFile::Content);
        h.advance(FOLLOWING_INTERVAL - CHECK);
        h.check().await;
        assert_eq!(h.requests(DataFile::Content), requests, "{name}");
    }
}

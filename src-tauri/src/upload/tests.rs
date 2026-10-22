//! Upload runs end to end, against a stub backend that keeps a cursor
//! ([`FakePagle`]): the queue, the cursor, the pages, the whole-send flag,
//! the backoff, every reply of CMP-R50 with the row it leaves and the
//! notifications it raises, and what local state records. The pure mapping
//! of refusals is at the end.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use wiremock::{MockServer, ResponseTemplate};

use super::fake_pagle::{FakePagle, Seen, refusal};
use super::queue::Trigger;
use super::{Ctx, Env, Refusal, RunEnd, Step, Uploads, on_refusal};
use crate::api::Api;
use crate::catch_logs::{self, Confirmed, Line, Note};
use crate::credential_store::testing::MemoryStore;
use crate::credential_store::{CredentialStore as _, Keychain};
use crate::device_flow::Approval;
use crate::in_game::folders::{DataFile, Qualifying, WriteReport};
use crate::link::{Hooks, Link, SendError, Stop};
use crate::notify::{Notifications, Notifier};
use crate::pagle_log::{self, Found, LocalLog};
use crate::readings::fixture::{LOG_A, account_folder, pagle_lua_text, save};
use crate::readings::{Reading, read};
use crate::state::{self, Answer, Loaded, LogState, RunResult, State, Store};
use crate::stub_server;
use crate::upload_doc;

const CREDENTIAL: &str = "pgc_test_credential";
const NOW: i64 = 1_791_800_000;

/// Records every notification shown.
#[derive(Default, Clone)]
struct Recorder(Arc<Mutex<Vec<String>>>);

impl Notifier for Recorder {
    fn show(&self, text: &str) {
        self.0.lock().unwrap().push(text.to_owned());
    }
}

const SUSPENDED: &str = "This Player is suspended. Contact support@pagle.app.";
const UPDATE_REQUIRED: &str =
    "This version of Pagle Companion is too old. Update it to keep uploading.";
const LOG_CAP: &str = "You already have 20 Catch logs on Pagle. Delete one to add another.";
const DIFFERS: &str = "This Catch log differs from the copy on Pagle.";
const FOREVER_API: &str = "Battle.net doesn't list WoW Forever characters yet. Your Catch log \
                           is safe here and will be verified once it does.";
const HELD: &str = "A new character needs verifying. Sign in again to verify it.";
const LEFT_OFF: &str =
    "This Angler is left off Leaderboards and Region firsts. Your Angler page says why.";

/// What runs work with in tests: real reads of the fixture's files, a
/// backoff that records its waits instead of waiting, and a fixed clock.
#[derive(Default)]
struct TestEnv {
    sleeps: Mutex<Vec<Duration>>,
    changed: AtomicUsize,
    /// Cancels the queue at the n-th wait, like an Unlink during a backoff.
    cancel_at_wait: Option<(usize, Arc<Uploads>)>,
}

impl TestEnv {
    fn sleeps(&self) -> Vec<u64> {
        self.sleeps
            .lock()
            .unwrap()
            .iter()
            .map(Duration::as_secs)
            .collect()
    }
}

impl Env for TestEnv {
    fn read(&self, folder: &str) -> impl Future<Output = Reading> + Send {
        let reading = read::read(Path::new(folder));
        async move { reading }
    }

    fn sleep(&self, wait: Duration) -> impl Future<Output = ()> + Send {
        let waits = {
            let mut sleeps = self.sleeps.lock().unwrap();
            sleeps.push(wait);
            sleeps.len()
        };
        let cancel = match &self.cancel_at_wait {
            Some((n, uploads)) if *n == waits => Some(uploads.clone()),
            _ => None,
        };
        async move {
            match cancel {
                // An Unlink while waiting: the wait never ends on its own.
                Some(uploads) => {
                    uploads.cancel();
                    std::future::pending::<()>().await;
                }
                None => tokio::task::yield_now().await,
            }
        }
    }

    fn now(&self) -> i64 {
        NOW
    }

    fn changed(&self) {
        self.changed.fetch_add(1, Ordering::SeqCst);
    }

    // No install has the Pagle addon here; the downloads' tests have them.
    fn qualifying(&self) -> Qualifying {
        Qualifying::default()
    }

    async fn data_files_hash_to(&self, _: DataFile, _: &str) -> bool {
        false
    }

    async fn write(&self, _: DataFile, _: Vec<u8>) -> WriteReport {
        WriteReport::default()
    }

    fn refresh_tocs(&self) {}
}

struct Harness {
    tmp: tempfile::TempDir,
    server: MockServer,
    pagle: FakePagle,
    api: Api,
    link: Link,
    state: Store,
    notifications: Notifications<Recorder>,
    shown: Recorder,
    /// The running Companion version (CMP-R45).
    version: &'static str,
    env: TestEnv,
    uploads: Arc<Uploads>,
    folder: PathBuf,
    confirmed: Confirmed,
}

impl Harness {
    /// Linked, with one confirmed Catch log of `entries` entries on disk.
    async fn new(entries: u64) -> Self {
        Self::build(entries, true).await
    }

    async fn unlinked(entries: u64) -> Self {
        Self::build(entries, false).await
    }

    async fn build(entries: u64, linked: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let server = stub_server::start().await;
        let pagle = FakePagle::default();
        pagle.mount(&server).await;
        let folder = account_folder(tmp.path(), "ADA#1");
        save(&folder, &pagle_lua_text(LOG_A, entries));
        let folder_text = folder.to_string_lossy().into_owned();

        let os = MemoryStore::new("Windows Credential Manager");
        if linked {
            os.write(CREDENTIAL).unwrap();
        }
        let uploads = Arc::new(Uploads::new());
        let hooks = {
            let (cancel, post_link) = (uploads.clone(), uploads.clone());
            Hooks {
                cancel_queue: Box::new(move || cancel.cancel()),
                start_post_link_runs: Box::new(move || post_link.trigger(LOG_A, Trigger::PostLink)),
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
        let shown = Recorder::default();
        Self {
            api: stub_server::api(&server),
            state: Store::new(tmp.path().join("state.json"), state),
            link: Link::new(Keychain::new(Box::new(os), None), hooks),
            notifications: Notifications::new(shown.clone()),
            shown,
            version: "1.0.0",
            env: TestEnv::default(),
            confirmed: [(LOG_A.to_owned(), folder_text)].into(),
            tmp,
            server,
            pagle,
            uploads,
            folder,
        }
    }

    fn ctx(&self) -> Ctx<'_, Recorder, TestEnv> {
        Ctx {
            account: self
                .link
                .account(&self.api, &self.state, &self.notifications),
            state: &self.state,
            version: self.version,
            confirmed: &self.confirmed,
            env: &self.env,
        }
    }

    /// Triggers a run of the Catch log and runs the queue empty.
    async fn run(&self, trigger: Trigger) -> Vec<RunEnd> {
        self.uploads.trigger(LOG_A, trigger);
        self.uploads.run_queued(&self.ctx()).await
    }

    fn log(&self) -> LogState {
        self.state.read(|s| s.logs[LOG_A].clone())
    }

    /// The game saves the file again, now with `entries` entries.
    fn save(&self, entries: u64) {
        save(&self.folder, &pagle_lua_text(LOG_A, entries));
    }

    /// A save by an addon that writes `format` (LOG-R…, CMP-R45).
    fn save_format(&self, entries: u64, format: u64) {
        let text = pagle_lua_text(LOG_A, entries)
            .replace("[\"format\"] = 1", &format!("[\"format\"] = {format}"));
        save(&self.folder, &text);
    }

    /// The stub holds the first `upto` entries of `log`.
    fn hold(&self, log: &LocalLog, upto: u64) {
        self.hold_at(log, upto, 1);
    }

    /// The stub holds the first `upto` entries of `log`, uploaded with
    /// `as_of` (UPL-R35).
    fn hold_at(&self, log: &LocalLog, upto: u64, as_of: u64) {
        self.pagle
            .hold(&upload_doc::page(log, 1, upto, as_of).unwrap().document);
    }

    /// The notifications shown so far.
    fn shown(&self) -> Vec<String> {
        self.shown.0.lock().unwrap().clone()
    }

    /// The row's last-run line now (CMP UI, Catch logs).
    fn line(&self) -> Option<Line> {
        catch_logs::run_line(&self.log(), NOW)
    }

    /// The stop message, while a stop holds (CMP UI, Status).
    fn stop(&self) -> Option<String> {
        self.ctx().account.stop_message()
    }

    /// The file's as-of time (CMP-R30).
    fn as_of(&self) -> u64 {
        let Reading::Valid(read) = read::read(&self.folder) else {
            panic!("the fixture doesn't read");
        };
        read.as_of as u64
    }

    /// A restart (CMP-R106): local state read back from `state.json`, and
    /// a new, empty queue.
    fn restart(mut self) -> Self {
        let Loaded::State(state) = state::load(&self.tmp.path().join("state.json")).unwrap() else {
            panic!("state.json didn't load");
        };
        self.state = Store::new(self.tmp.path().join("state.json"), *state);
        self.uploads = Arc::new(Uploads::new());
        self
    }
}

fn local(entries: u64) -> LocalLog {
    parse(&pagle_lua_text(LOG_A, entries))
}

/// The same `logId` with other entries: another PC's fork (UPL-R99).
fn fork(entries: u64) -> LocalLog {
    parse(&pagle_lua_text(LOG_A, entries).replace("|17917350", "|17000000"))
}

fn parse(text: &str) -> LocalLog {
    match pagle_log::parse(text.as_bytes()).unwrap() {
        Found::Valid(log) => log,
        other => panic!("not valid: {other:?}"),
    }
}

fn seen(from: u64, count: u64, total: u64, anchor: &str, as_of: u64) -> Seen {
    Seen {
        log_id: LOG_A.into(),
        as_of,
        total,
        from,
        anchor: anchor.into(),
        count,
    }
}

fn applied() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "result": "applied", "reason": null, "message": null, "logId": LOG_A,
        "end": { "seq": 1, "entryId": "x" }, "newestAsOf": 1, "catchLog": null }))
}

async fn until(what: impl Fn() -> bool) {
    while !what() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_first_upload_sends_45000_entries_in_three_pages() {
    // CMP-R47: with E = 0, f = 1; pages of at most 20,000 entries with
    // total = L and one as-of, the next page at f + count
    let h = Harness::new(45_000).await;
    let as_of = h.as_of();
    let log = local(45_000);

    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );

    assert_eq!(
        h.pagle.pages(),
        [
            seen(1, 20_000, 45_000, "", as_of),
            seen(
                20_001,
                20_000,
                45_000,
                &log.entry_id(20_000).unwrap(),
                as_of
            ),
            seen(40_001, 5_000, 45_000, &log.entry_id(40_000).unwrap(), as_of),
        ]
    );
    assert_eq!(h.pagle.end(LOG_A), 45_000);
}

#[tokio::test]
async fn a_finished_run_records_last_run_and_last_uploaded() {
    // CMP Data: lastRun with the file as read, its format, the Companion's
    // version and the reply's catchLog; lastUploadedAt per page (CMP-R50)
    let h = Harness::new(15).await;
    h.run(Trigger::Start).await;

    let log = h.log();
    let run = log.last_run.unwrap();
    let Reading::Valid(read) = read::read(&h.folder) else {
        panic!()
    };
    assert_eq!(run.result, RunResult::Uploaded);
    assert_eq!(run.at, NOW);
    assert_eq!(
        (run.file_mtime_ms, run.file_size),
        (read.file.mtime_ms, read.file.size)
    );
    assert_eq!(run.format, 1);
    assert_eq!(run.companion_version, "1.0.0");
    assert_eq!((run.code, run.message), (None, None));
    let catch_log = run.catch_log.unwrap();
    assert_eq!(catch_log.verification, "pending");
    assert_eq!(catch_log.pending_reason.as_deref(), Some("forever-api"));
    assert_eq!(log.last_uploaded_at, Some(NOW));
    assert!(!log.whole_send_pending);
    assert!(h.env.changed.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn a_cursor_behind_the_disk_sends_the_rest_with_its_anchor() {
    // CMP-R47: with E = 12, L = 15 and matching entry IDs, one page from 13,
    // count 3, with the anchor of entry 12
    let h = Harness::new(15).await;
    let log = local(15);
    h.hold(&log, 12);

    assert_eq!(
        h.run(Trigger::Change).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(
        h.pagle.pages(),
        [seen(13, 3, 15, &log.entry_id(12).unwrap(), h.as_of())]
    );
    assert!(!h.log().whole_send_pending);
}

#[tokio::test]
async fn a_cursor_at_the_end_is_up_to_date_except_right_after_linking() {
    // CMP-R47: with E = L the run ends up to date and sends no Upload;
    // CMP-R101: right after linking it sends from = L, count = 1, anchored
    // at L − 1
    let h = Harness::new(15).await;
    let log = local(15);
    h.hold(&log, 15);

    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::UpToDate)]
    );
    assert!(h.pagle.pages().is_empty());
    assert_eq!(h.pagle.cursor_reads(), 1);
    assert_eq!(h.log().last_uploaded_at, None);

    assert_eq!(
        h.run(Trigger::PostLink).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(
        h.pagle.pages(),
        [seen(15, 1, 15, &log.entry_id(14).unwrap(), h.as_of())]
    );
    // Linking sets no whole-send flag.
    assert!(!h.log().whole_send_pending);
}

#[tokio::test]
async fn up_to_date_keeps_the_catch_log_of_the_last_reply() {
    // CMP UI, Catch logs: a run that sends nothing keeps the previous
    // catchLog
    let h = Harness::new(15).await;
    h.run(Trigger::Start).await;
    h.run(Trigger::UploadNow).await;
    let run = h.log().last_run.unwrap();
    assert_eq!(run.result, RunResult::UpToDate);
    assert!(run.catch_log.is_some());
}

#[tokio::test]
async fn a_server_ahead_of_the_disk_gets_the_whole_log() {
    // CMP-R47: with E > L the run sends the whole log from 1 (ADR 0001)
    let h = Harness::new(15).await;
    h.hold(&local(20), 20);

    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(h.pagle.pages(), [seen(1, 15, 15, "", h.as_of())]);
    assert_eq!(h.pagle.end(LOG_A), 15);
    assert!(!h.log().whole_send_pending);
}

#[tokio::test]
async fn a_whole_send_cut_off_after_page_1_starts_again_from_seq_1() {
    // CMP-R48, CMP-R106: the whole-send flag is set when a whole send
    // starts and kept in state.json, so a whole send cut off between pages
    // starts again from seq 1 at the next run
    let h = Harness::new(45_000).await;
    h.pagle.script_uploads(vec![
        None,
        Some(applied().set_delay(Duration::from_secs(5))),
    ]);
    let pagle = h.pagle.clone();
    tokio::select! {
        _ = h.run(Trigger::Start) => panic!("the run wasn't cut off"),
        () = until(|| pagle.pages().len() == 2) => {}
    }
    // The process ends here, with page 1 applied.
    assert_eq!(h.pagle.end(LOG_A), 20_000);

    let h = h.restart();
    assert!(h.log().whole_send_pending);
    let before = h.pagle.pages().len();
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    let pages: Vec<(u64, u64)> = h.pagle.pages()[before..]
        .iter()
        .map(|p| (p.from, p.count))
        .collect();
    assert_eq!(pages, [(1, 20_000), (20_001, 20_000), (40_001, 5_000)]);
    assert!(!h.log().whole_send_pending);
    assert_eq!(h.pagle.end(LOG_A), 45_000);
}

#[tokio::test]
async fn a_503_makes_the_queue_wait_30_then_60_seconds_and_a_success_resets_it() {
    // CMP-R51: after a 5xx, wait 30 s, doubling; the queue's next success
    // resets it; CMP-R50: then restart the run from step 1
    let h = Harness::new(15).await;
    h.pagle.script_cursor(vec![
        Some(ResponseTemplate::new(503)),
        Some(ResponseTemplate::new(503).set_body_string("<html>down</html>")),
    ]);
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(h.env.sleeps(), [30, 60]);
    assert_eq!(h.pagle.cursor_reads(), 3);

    // A page's 5xx restarts the run from the cursor, after the first retry
    // delay again.
    h.save(16);
    h.pagle
        .script_uploads(vec![Some(ResponseTemplate::new(502))]);
    assert_eq!(
        h.run(Trigger::Change).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(h.env.sleeps(), [30, 60, 30]);
    assert_eq!(h.pagle.cursor_reads(), 5);
    assert_eq!(h.pagle.end(LOG_A), 16);
}

#[tokio::test]
async fn no_connection_waits_the_backoff_and_unlink_ends_the_wait() {
    // CMP-R51: no connection waits the retry backoff, doubling; CMP-R63
    // step 1: Unlink cancels the running item, which leaves lastRun as it
    // was (CMP-R50)
    let mut h = Harness::new(15).await;
    h.api = Api::new("1.0.0", stub_server::unreachable_base()).unwrap();
    h.env.cancel_at_wait = Some((3, h.uploads.clone()));

    assert_eq!(h.run(Trigger::Start).await, [RunEnd::Interrupted]);
    assert_eq!(h.env.sleeps(), [30, 60, 120]);
    assert_eq!(h.log().last_run, None);
}

#[tokio::test]
async fn unlinked_nothing_is_sent_and_once_linked_the_confirmed_logs_upload() {
    // CMP-R55: nothing is sent while not linked; CMP-R101: right after
    // linking, a run for every confirmed logId
    let h = Harness::unlinked(15).await;
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::NotStarted("not linked")]
    );
    h.uploads.trigger(LOG_A, Trigger::UploadNow);
    h.uploads.run_queued(&h.ctx()).await;
    assert!(h.server.received_requests().await.unwrap().is_empty());

    h.ctx()
        .account
        .store_link(Approval {
            credential: CREDENTIAL.into(),
            battle_tag: "Ada#1234".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        h.uploads.run_queued(&h.ctx()).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(h.pagle.pages(), [seen(1, 15, 15, "", h.as_of())]);
    for request in h.server.received_requests().await.unwrap() {
        assert_eq!(
            request.headers.get("authorization").unwrap(),
            &format!("Bearer {CREDENTIAL}")
        );
    }
}

#[tokio::test]
async fn switching_off_drops_the_queued_run_and_stops_the_running_one() {
    // CMP-R41: switching off drops a queued run of that logId, and a
    // running one stops after its current request, leaving lastRun as it
    // was (CMP-R50)
    let h = Harness::new(45_000).await;
    h.pagle
        .script_uploads(vec![Some(applied().set_delay(Duration::from_millis(300)))]);
    h.uploads.trigger(LOG_A, Trigger::Start);
    let ctx = h.ctx();
    let (ends, ()) = tokio::join!(h.uploads.run_queued(&ctx), async {
        until(|| h.pagle.pages().len() == 1).await;
        // A save during the run queues one more…
        h.uploads.trigger(LOG_A, Trigger::Change);
        // …which switching off drops.
        h.uploads.stop_runs(LOG_A);
    });
    assert_eq!(ends, [RunEnd::Interrupted]);
    assert_eq!(h.pagle.pages().len(), 1);
    let log = h.log();
    assert_eq!(log.last_run, None);
    // The whole send isn't finished: the next run starts from seq 1.
    assert!(log.whole_send_pending);
}

#[tokio::test]
async fn a_divergence_is_held_back_until_upload_now() {
    // CMP-R47: a different local entry ID at E ends the run as a
    // divergence without sending; CMP-R45: held back after it, except from
    // Upload now and linking
    let h = Harness::new(15).await;
    h.hold(&fork(12), 12);

    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Divergence)]
    );
    assert!(h.pagle.pages().is_empty());
    for trigger in [Trigger::Change, Trigger::Scan, Trigger::SwitchOn] {
        assert_eq!(h.run(trigger).await, [RunEnd::NotStarted("held back")]);
    }
    assert_eq!(h.pagle.cursor_reads(), 1);
    assert_eq!(
        h.run(Trigger::UploadNow).await,
        [RunEnd::Recorded(RunResult::Divergence)]
    );
    assert_eq!(h.pagle.cursor_reads(), 2);
}

#[tokio::test]
async fn a_later_scan_skips_a_file_unchanged_since_its_upload() {
    // CMP-R45: after each later scan, except one whose last run ended
    // uploaded or up to date and whose file is still that run's; CMP-R46:
    // every run reads its file when it starts
    let h = Harness::new(15).await;
    h.run(Trigger::Start).await;
    assert_eq!(
        h.run(Trigger::Scan).await,
        [RunEnd::NotStarted("unchanged since its last run")]
    );
    assert_eq!(h.pagle.cursor_reads(), 1);

    h.uploads.trigger(LOG_A, Trigger::Scan);
    h.save(17);
    assert_eq!(
        h.uploads.run_queued(&h.ctx()).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    let last = h.pagle.pages().pop().unwrap();
    assert_eq!((last.from, last.count, last.total), (16, 2, 17));
}

#[tokio::test]
async fn entry_cap_ends_the_run_failed_and_held_back_until_the_file_changes() {
    // CMP-R50: entry_cap ends the run failed, held back (CMP-R45), with the
    // refusal's code and message, and writes the reply to the local log
    // file; CMP UI: "Upload failed. Pagle Companion tries again when the file
    // changes."; CMP-R128
    let h = Harness::new(15).await;
    h.pagle
        .script_uploads(vec![Some(ResponseTemplate::new(422).set_body_json(
            json!({ "result": "refused", "reason": "entry_cap", "message": "Too long.",
                "logId": LOG_A, "end": { "seq": 0, "entryId": null }, "newestAsOf": null,
                "catchLog": null }),
        ))]);
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Failed)]
    );
    let run = h.log().last_run.unwrap();
    assert_eq!(run.code.as_deref(), Some("entry_cap"));
    assert_eq!(run.message.as_deref(), Some("Too long."));
    assert_eq!(
        h.line(),
        Some(Line::Failed {
            text: "Upload failed. Pagle Companion tries again when the file changes."
        })
    );
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::NotStarted("held back")]
    );
    // A file change lifts the hold.
    h.save(16);
    assert_eq!(
        h.run(Trigger::Change).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
}

#[tokio::test]
async fn a_log_no_longer_confirmed_or_on_disk_doesnt_run() {
    // CMP-R45: runs are for confirmed logIds; CMP-R46: a run reads its file
    // when it starts
    let mut h = Harness::new(15).await;
    h.confirmed.clear();
    assert_eq!(
        h.run(Trigger::UploadNow).await,
        [RunEnd::NotStarted("not confirmed")]
    );
    let h = Harness::new(15).await;
    std::fs::remove_file(h.folder.join("SavedVariables/Pagle.lua")).unwrap();
    assert_eq!(
        h.run(Trigger::UploadNow).await,
        [RunEnd::NotStarted("its file doesn't hold it")]
    );
    assert_eq!(h.pagle.cursor_reads(), 0);
}

#[tokio::test]
async fn the_row_says_uploaded_or_up_to_date() {
    // CMP UI, Catch logs: "Uploaded {relative time}" and "Up to date"
    let h = Harness::new(15).await;
    h.run(Trigger::Start).await;
    assert_eq!(
        catch_logs::run_line(&h.log(), NOW + 5 * 60),
        Some(Line::Uploaded {
            text: "Uploaded 5 minutes ago".into(),
            notes: vec![Note::Pending { text: FOREVER_API }],
        })
    );
    h.run(Trigger::UploadNow).await;
    assert_eq!(
        h.line(),
        Some(Line::UpToDate {
            text: "Up to date",
            notes: vec![Note::Pending { text: FOREVER_API }],
        })
    );
    let json = serde_json::to_value(h.line()).unwrap();
    assert_eq!(
        json,
        json!({ "kind": "up-to-date", "text": "Up to date",
                "notes": [{ "kind": "pending", "text": FOREVER_API }] })
    );
}

#[tokio::test]
async fn the_first_readings_update_is_the_start_and_later_scans_are_scans() {
    // CMP-R45: at start, then after each later scan
    let uploads = Uploads::new();
    assert_eq!(uploads.reading_trigger(false), Some(Trigger::Start));
    assert_eq!(uploads.reading_trigger(false), None);
    assert_eq!(uploads.reading_trigger(true), Some(Trigger::Scan));
}

// Every reply of CMP-R50, end to end.

#[tokio::test]
async fn a_first_gap_reads_the_cursor_again_and_restarts() {
    // CMP-R50: gap reads the cursor again and restarts the run once;
    // CMP-R120: from a fresh cursor, with no backoff
    let h = Harness::new(15).await;
    h.pagle.script_uploads(vec![Some(refusal(409, "gap"))]);
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(h.pagle.cursor_reads(), 2);
    assert_eq!(h.pagle.pages().len(), 2);
    assert!(h.env.sleeps().is_empty());
    assert_eq!(h.pagle.end(LOG_A), 15);
}

#[tokio::test]
async fn a_second_gap_ends_the_run_failed_and_not_held_back() {
    // CMP-R50: a second gap in the same run ends it failed, not held back,
    // so the next trigger tries again; CMP UI: "Upload failed. Pagle
    // Companion tries again later."; CMP Tunables: 1 gap restart per run
    let h = Harness::new(15).await;
    h.pagle
        .script_uploads(vec![Some(refusal(409, "gap")), Some(refusal(409, "gap"))]);
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Failed)]
    );
    assert_eq!(h.pagle.cursor_reads(), 2);
    let run = h.log().last_run.unwrap();
    assert_eq!(run.code.as_deref(), Some("gap"));
    assert_eq!(
        h.line(),
        Some(Line::Failed {
            text: "Upload failed. Pagle Companion tries again later."
        })
    );
    // Not held back: the same file runs again at the next trigger.
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
}

#[tokio::test]
async fn a_divergence_is_held_back_notifies_once_and_offers_replace() {
    // CMP-R52: the logId is held back, its row says "This Catch log differs
    // from the copy on Pagle.", a notification is raised (CMP-R111), and the
    // row offers Replace with its question; Upload now finds it again
    let h = Harness::new(15).await;
    h.hold(&fork(12), 12);
    h.run(Trigger::Start).await;
    assert_eq!(
        h.line(),
        Some(Line::Divergence {
            text: DIFFERS,
            question: "Pagle will hold this computer's copy of this Catch log. Entries only \
                       Pagle's copy has will be dropped from Pagle.",
        })
    );
    assert_eq!(h.shown(), [DIFFERS]);
    assert_eq!(
        h.run(Trigger::UploadNow).await,
        [RunEnd::Recorded(RunResult::Divergence)]
    );
    // CMP-R110: once per cause.
    assert_eq!(h.shown(), [DIFFERS]);
}

#[tokio::test]
async fn a_page_refused_as_divergence_ends_as_a_divergence() {
    // CMP-R50: divergence ends the run as a divergence (CMP-R52), held back
    let h = Harness::new(15).await;
    h.pagle
        .script_uploads(vec![Some(refusal(409, "divergence"))]);
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Divergence)]
    );
    h.save(16);
    assert_eq!(
        h.run(Trigger::Change).await,
        [RunEnd::NotStarted("held back")]
    );
    assert_eq!(h.shown(), [DIFFERS]);
}

#[tokio::test]
async fn replace_sends_the_whole_log_from_seq_1() {
    // CMP-R52: Replace sets the whole-send flag (CMP-R48), lifts the hold
    // and starts a run, which sends the whole log (ADR 0001); CMP-R110: the
    // divergence clears, so a new one notifies again
    let h = Harness::new(15).await;
    h.hold(&fork(12), 12);
    h.run(Trigger::Start).await;

    Uploads::replace(&h.state, LOG_A).unwrap();
    assert!(h.log().whole_send_pending);
    // The hold lifts with the divergence's lastRun.
    assert_eq!(h.log().last_run, None);
    assert_eq!(h.line(), None);
    assert_eq!(
        h.run(Trigger::Replace).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(h.pagle.pages(), [seen(1, 15, 15, "", h.as_of())]);
    assert!(!h.log().whole_send_pending);
    assert_eq!(h.pagle.end(LOG_A), 15);

    h.hold_at(&fork(12), 12, h.as_of());
    h.save(16);
    assert_eq!(
        h.run(Trigger::Change).await,
        [RunEnd::Recorded(RunResult::Divergence)]
    );
    assert_eq!(h.shown(), [DIFFERS, DIFFERS]);
}

#[tokio::test]
async fn a_replace_cut_off_sends_the_whole_log_at_the_next_run() {
    // CMP-R52: Replace sets the whole-send flag and lifts the hold, both kept
    // in state.json (CMP-R48, CMP-R106), so a Replace whose run never ran
    // still replaces at the next start
    let h = Harness::new(15).await;
    h.hold(&fork(12), 12);
    h.run(Trigger::Start).await;
    Uploads::replace(&h.state, LOG_A).unwrap();

    let h = h.restart();
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(h.pagle.pages(), [seen(1, 15, 15, "", h.as_of())]);
}

#[tokio::test]
async fn only_a_divergence_can_be_replaced() {
    // CMP-R52: the row offers Replace after a divergence only
    let h = Harness::new(15).await;
    assert!(Uploads::replace(&h.state, LOG_A).is_err());
    h.run(Trigger::Start).await;
    assert!(Uploads::replace(&h.state, LOG_A).is_err());
    assert!(!h.log().whole_send_pending);
}

#[tokio::test]
async fn replace_over_a_newer_copy_on_pagle_ends_stale_and_isnt_held_back() {
    // CMP-R52: the server applies it unless its as-of is older than Pagle's
    // newest (UPL-R35), in which case the run ends stale; CMP-R50: stale is
    // not held back; CMP-R134: the Companion shows the state and tries again
    // on the next trigger
    let h = Harness::new(15).await;
    h.hold_at(&fork(12), 12, h.as_of() + 100);
    h.run(Trigger::Start).await;
    Uploads::replace(&h.state, LOG_A).unwrap();
    assert_eq!(
        h.run(Trigger::Replace).await,
        [RunEnd::Recorded(RunResult::Stale)]
    );
    assert_eq!(
        h.line(),
        Some(Line::Stale {
            text: "Pagle already has a newer copy of this Catch log. Nothing changed."
        })
    );
    let reads = h.pagle.cursor_reads();
    assert_eq!(
        h.run(Trigger::Change).await,
        [RunEnd::Recorded(RunResult::Stale)]
    );
    assert_eq!(h.pagle.cursor_reads(), reads + 1);
    // Stale raises nothing; the divergence's cause has cleared.
    assert_eq!(h.shown(), [DIFFERS]);
}

#[tokio::test]
async fn deleted_is_retried_by_upload_now_but_not_by_a_file_change() {
    // CMP-R54: after deleted the row says "You deleted this Catch log on the
    // Pagle website. Allow it again there to upload it."; held back from
    // file changes only, so once allowed again the next start, scan, Upload
    // now or link uploads it from seq 1 (E = 0); CMP-R121
    let h = Harness::new(15).await;
    h.run(Trigger::Start).await;
    h.pagle.delete(LOG_A);
    h.save(16);
    assert_eq!(
        h.run(Trigger::Change).await,
        [RunEnd::Recorded(RunResult::Deleted)]
    );
    assert_eq!(
        h.line(),
        Some(Line::Deleted {
            text: "You deleted this Catch log on the Pagle website. Allow it again there to \
                   upload it."
        })
    );
    h.save(17);
    assert_eq!(
        h.run(Trigger::Change).await,
        [RunEnd::NotStarted("held back")]
    );
    assert_eq!(
        h.run(Trigger::UploadNow).await,
        [RunEnd::Recorded(RunResult::Deleted)]
    );

    h.pagle.allow(LOG_A);
    assert_eq!(
        h.run(Trigger::UploadNow).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(
        h.pagle.pages().pop().unwrap(),
        seen(1, 17, 17, "", h.as_of())
    );
    assert!(h.shown().is_empty());
}

#[tokio::test]
async fn a_dev_log_is_held_until_the_format_changes() {
    // CMP-R50: dev_log ends the run dev log, held back (CMP-R45) until the
    // file's format differs; CMP-R133: at L the wiped server answers E = 0,
    // the whole log goes, and it comes back dev_log
    let h = Harness::new(15).await;
    h.pagle.script_uploads(vec![Some(refusal(422, "dev_log"))]);
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::DevLog)]
    );
    assert_eq!(h.pagle.pages(), [seen(1, 15, 15, "", h.as_of())]);
    assert_eq!(
        h.line(),
        Some(Line::DevLog {
            text: "This Catch log is from a Pagle development version and can't be uploaded."
        })
    );
    h.save(16);
    for trigger in [Trigger::Change, Trigger::Start, Trigger::Scan] {
        assert_eq!(h.run(trigger).await, [RunEnd::NotStarted("held back")]);
    }
    h.save_format(17, 2);
    assert_eq!(
        h.run(Trigger::Change).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
}

#[tokio::test]
async fn entry_cap_is_held_until_the_companion_runs_another_version() {
    // CMP-R45: failed with a refusal, until its watched file changes or the
    // Companion runs a different version than the one that failed
    let mut h = Harness::new(15).await;
    h.pagle
        .script_uploads(vec![Some(refusal(422, "entry_cap"))]);
    h.run(Trigger::Start).await;
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::NotStarted("held back")]
    );
    h.version = "1.0.1";
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
}

#[tokio::test]
async fn unsupported_document_is_failed_held_back_and_asks_for_an_update() {
    // CMP-R50: unsupported_document ends the run failed, held back
    // (CMP-R45), and the row says "Update Pagle Companion."
    let mut h = Harness::new(15).await;
    h.pagle
        .script_uploads(vec![Some(refusal(422, "unsupported_document"))]);
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Failed)]
    );
    assert_eq!(
        h.line(),
        Some(Line::Failed {
            text: "Update Pagle Companion."
        })
    );
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::NotStarted("held back")]
    );
    h.version = "1.1.0";
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
}

#[tokio::test]
async fn unsupported_format_waits_for_pagle_and_isnt_held_back() {
    // CMP-R50: unsupported_format ends the run waiting for Pagle, not held
    // back; CMP UI: "The Pagle website can't read this version of the addon
    // yet. Pagle Companion tries again later."
    let h = Harness::new(15).await;
    h.pagle
        .script_uploads(vec![Some(refusal(422, "unsupported_format"))]);
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::WaitingForPagle)]
    );
    assert_eq!(
        h.line(),
        Some(Line::WaitingForPagle {
            text: "The Pagle website can't read this version of the addon yet. Pagle \
                   Companion tries again later."
        })
    );
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
}

#[tokio::test]
async fn a_4xx_without_a_code_is_failed_and_held_back() {
    // CMP-R50: a 4xx whose body isn't JSON, or has no code, counts as "any
    // other 4xx": failed, held back
    let h = Harness::new(15).await;
    h.pagle.script_cursor(vec![Some(
        ResponseTemplate::new(400).set_body_string("nope"),
    )]);
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Failed)]
    );
    assert_eq!(h.log().last_run.unwrap().code.as_deref(), Some("400"));
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::NotStarted("held back")]
    );
}

#[tokio::test]
async fn log_cap_notifies_with_its_message_and_isnt_held_back() {
    // CMP-R50: log_cap ends the run log cap, not held back, with a
    // notification (CMP-R111) of the refusal's message; CMP UI: the row
    // shows that message with a link to My Anglers; CMP-R128
    let h = Harness::new(15).await;
    h.pagle.script_uploads(vec![Some(refusal(422, "log_cap"))]);
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::LogCap)]
    );
    assert_eq!(
        h.line(),
        Some(Line::LogCap {
            text: LOG_CAP.into()
        })
    );
    assert_eq!(h.shown(), [LOG_CAP]);
    // Not held back; once it uploads, the cause has cleared.
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    h.pagle.script_uploads(vec![Some(refusal(422, "log_cap"))]);
    h.save(16);
    h.run(Trigger::Change).await;
    assert_eq!(h.shown(), [LOG_CAP, LOG_CAP]);
}

#[tokio::test]
async fn a_429_waits_retry_after_and_resends_the_same_request() {
    // CMP-R50: 429 on a page waits Retry-After seconds, then resends the
    // same request, and the whole queue waits (one request at a time,
    // CMP-R46); CMP-R128, OPS-R67
    let h = Harness::new(15).await;
    h.pagle.script_uploads(vec![Some(
        refusal(429, "rate_limited").insert_header("Retry-After", "7"),
    )]);
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(h.env.sleeps(), [7]);
    let page = seen(1, 15, 15, "", h.as_of());
    assert_eq!(h.pagle.pages(), [page.clone(), page]);
    assert_eq!(h.pagle.cursor_reads(), 1);
}

#[tokio::test]
async fn a_429_without_a_readable_retry_after_waits_the_backoff() {
    // CMP-R50: the retry backoff when Retry-After is missing or unreadable
    // (CMP-R51), on a cursor read too
    let h = Harness::new(15).await;
    h.pagle.script_cursor(vec![
        Some(refusal(429, "rate_limited")),
        Some(refusal(429, "rate_limited").insert_header("Retry-After", "soon")),
    ]);
    assert_eq!(
        h.run(Trigger::Start).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
    assert_eq!(h.env.sleeps(), [30, 60]);
    assert_eq!(h.pagle.cursor_reads(), 3);
}

#[tokio::test]
async fn unlink_during_a_429_wait_ends_the_run_leaving_last_run() {
    // CMP-R50: a run ended by Unlink leaves the logId's lastRun as it was
    let mut h = Harness::new(15).await;
    h.env.cancel_at_wait = Some((1, h.uploads.clone()));
    h.pagle.script_uploads(vec![Some(
        refusal(429, "rate_limited").insert_header("Retry-After", "7"),
    )]);
    assert_eq!(h.run(Trigger::Start).await, [RunEnd::Interrupted]);
    assert_eq!(h.log().last_run, None);
    assert_eq!(h.pagle.pages().len(), 1);
}

#[tokio::test]
async fn suspended_stops_every_run_until_the_next_link() {
    // CMP-R50: suspended (403) is a suspended stop: every request but update
    // checks, config reads, self-revoke and the device flow stops, with a
    // notification (CMP-R111) of the refusal's message, until the next
    // start or link; a run ended by a stop leaves lastRun as it was;
    // CMP-R124
    let h = Harness::new(15).await;
    h.pagle.script_cursor(vec![Some(refusal(403, "suspended"))]);
    assert_eq!(h.run(Trigger::Start).await, [RunEnd::Interrupted]);
    assert_eq!(h.log().last_run, None);
    assert_eq!(h.shown(), [SUSPENDED]);
    assert_eq!(h.stop().as_deref(), Some(SUSPENDED));

    for trigger in [Trigger::Change, Trigger::Start, Trigger::UploadNow] {
        assert_eq!(h.run(trigger).await, [RunEnd::NotStarted("stopped")]);
    }
    assert!(matches!(
        h.ctx()
            .account
            .send(reqwest::Method::GET, "in-game/anglers", |r| r)
            .await,
        Err(SendError::Stopped)
    ));
    assert_eq!(h.pagle.cursor_reads(), 1);
    assert_eq!(h.server.received_requests().await.unwrap().len(), 1);

    // The next link clears it, and its post-link runs upload.
    h.ctx()
        .account
        .store_link(Approval {
            credential: CREDENTIAL.into(),
            battle_tag: "Ada#1234".into(),
        })
        .await
        .unwrap();
    assert_eq!(h.stop(), None);
    assert_eq!(
        h.uploads.run_queued(&h.ctx()).await,
        [RunEnd::Recorded(RunResult::Uploaded)]
    );
}

#[tokio::test]
async fn a_stop_drops_the_queued_runs() {
    // CMP-R50: a stop stops every request: the queue's other runs end too
    let h = Harness::new(15).await;
    h.pagle.script_cursor(vec![Some(refusal(403, "suspended"))]);
    h.uploads.trigger(LOG_A, Trigger::Start);
    let ctx = h.ctx();
    let item = h.uploads.next().await;
    // A save during the run queues one more, which the stop drops.
    h.uploads.trigger(LOG_A, Trigger::Change);
    assert_eq!(h.uploads.run_item(&ctx, item).await, RunEnd::Interrupted);
    assert!(h.uploads.run_queued(&ctx).await.is_empty());
}

#[tokio::test]
async fn update_required_stops_runs_with_its_message() {
    // CMP-R50, DST-R61: update_required (403) ends the run and the Companion
    // enters update required, whose stop message is the refusal's message;
    // runs don't start while it holds, and lastRun stays as it was;
    // CMP-R127
    let h = Harness::new(15).await;
    h.pagle
        .script_uploads(vec![Some(refusal(403, "update_required"))]);
    assert_eq!(h.run(Trigger::Start).await, [RunEnd::Interrupted]);
    assert_eq!(h.log().last_run, None);
    assert_eq!(h.stop().as_deref(), Some(UPDATE_REQUIRED));
    assert_eq!(
        h.run(Trigger::UploadNow).await,
        [RunEnd::NotStarted("stopped")]
    );
    // DST-R57's notification comes with Track C.
    assert!(h.shown().is_empty());
}

#[tokio::test]
async fn an_applied_reply_pending_forever_api_shows_its_sentence_and_no_notification() {
    // CMP-R53: pending with a pendingReason shows the text for that reason
    // (accounts.md UI, Pending reasons), with no notification; CMP-R125:
    // with no Forever namespace every Catch log is Pending forever-api
    let h = Harness::new(15).await;
    h.run(Trigger::Start).await;
    assert_eq!(
        h.line(),
        Some(Line::Uploaded {
            text: "Uploaded just now".into(),
            notes: vec![Note::Pending { text: FOREVER_API }],
        })
    );
    assert!(h.shown().is_empty());
}

#[tokio::test]
async fn held_characters_ask_to_sign_in_and_notify_until_they_clear() {
    // CMP-R53: verified with heldCharacters > 0 shows "A new character needs
    // verifying. Sign in again to verify it." with a Sign in link, and a
    // notification whose cause is heldCharacters > 0 for that logId;
    // CMP-R123: the window shows the held count from the next reply;
    // CMP-R110: again only after it cleared
    let h = Harness::new(15).await;
    let verified = |held: u32| {
        json!({ "verification": "verified", "pendingReason": null,
                "heldCharacters": held, "leftOff": null })
    };
    h.pagle.set_catch_log(verified(1));
    h.run(Trigger::Start).await;
    assert_eq!(
        h.line(),
        Some(Line::Uploaded {
            text: "Uploaded just now".into(),
            notes: vec![Note::HeldCharacters { text: HELD }],
        })
    );
    assert_eq!(h.shown(), [HELD]);
    // Up to date keeps the last catchLog, and the cause: no new notification.
    h.run(Trigger::UploadNow).await;
    assert_eq!(
        h.line(),
        Some(Line::UpToDate {
            text: "Up to date",
            notes: vec![Note::HeldCharacters { text: HELD }],
        })
    );
    assert_eq!(h.shown(), [HELD]);

    h.pagle.set_catch_log(verified(0));
    h.save(16);
    h.run(Trigger::Change).await;
    assert_eq!(
        h.line(),
        Some(Line::Uploaded {
            text: "Uploaded just now".into(),
            notes: vec![],
        })
    );
    h.pagle.set_catch_log(verified(2));
    h.save(17);
    h.run(Trigger::Change).await;
    assert_eq!(h.shown(), [HELD, HELD]);
}

#[tokio::test]
async fn a_left_off_angler_shows_the_my_anglers_line_without_a_notification() {
    // CMP-R53: leftOff not null shows "This Angler is left off Leaderboards
    // and Region firsts. Your Angler page says why." with a link to My
    // Anglers, for flagged and excluded alike, and no notification;
    // CMP-R124; CMP UI: CMP-R53's lines in order, all that apply
    let h = Harness::new(15).await;
    for left_off in ["flagged", "excluded"] {
        h.pagle.set_catch_log(json!({ "verification": "verified",
            "pendingReason": null, "heldCharacters": 1, "leftOff": left_off }));
        h.run(Trigger::UploadNow).await;
        let Some(Line::Uploaded { notes, .. } | Line::UpToDate { notes, .. }) = h.line() else {
            panic!("not uploaded: {:?}", h.line());
        };
        assert_eq!(
            notes,
            [
                Note::HeldCharacters { text: HELD },
                Note::LeftOff { text: LEFT_OFF }
            ]
        );
        h.save(16 + u64::from(left_off == "excluded"));
    }
    assert_eq!(h.shown(), [HELD]);
}

#[tokio::test]
async fn the_state_line_comes_back_from_state_json_after_a_restart() {
    // CMP UI, Catch logs: every state line comes from lastRun, which
    // state.json keeps (CMP Data), so a restart shows it again
    let h = Harness::new(15).await;
    h.pagle.delete(LOG_A);
    h.run(Trigger::Start).await;
    let h = h.restart();
    assert!(matches!(h.line(), Some(Line::Deleted { .. })));
}

// The pure mapping of CMP-R50.

fn refused(status: u16, code: Option<&str>) -> Refusal {
    Refusal {
        status,
        code: code.map(str::to_owned),
        message: Some("m".into()),
        retry_after: None,
    }
}

fn ends(result: RunResult, code: &str) -> Step {
    Step::End {
        result,
        code: Some(code.into()),
        message: Some("m".into()),
    }
}

#[test]
fn every_refusal_code_has_its_step() {
    // CMP-R50: classified by HTTP status first, then by the code; UPL
    // Refusals' codes; held back or not is CMP-R45's, from the result
    let cases = [
        (409, Some("gap"), Step::Restart),
        (
            409,
            Some("divergence"),
            ends(RunResult::Divergence, "divergence"),
        ),
        (409, Some("stale"), ends(RunResult::Stale, "stale")),
        (409, Some("deleted"), ends(RunResult::Deleted, "deleted")),
        (422, Some("dev_log"), ends(RunResult::DevLog, "dev_log")),
        (
            422,
            Some("unsupported_format"),
            ends(RunResult::WaitingForPagle, "unsupported_format"),
        ),
        (
            422,
            Some("unsupported_document"),
            ends(RunResult::Failed, "unsupported_document"),
        ),
        (422, Some("log_cap"), ends(RunResult::LogCap, "log_cap")),
        (422, Some("entry_cap"), ends(RunResult::Failed, "entry_cap")),
        (413, Some("too_large"), ends(RunResult::Failed, "too_large")),
        (400, Some("malformed"), ends(RunResult::Failed, "malformed")),
        (415, Some("malformed"), ends(RunResult::Failed, "malformed")),
        (400, Some("checksum"), ends(RunResult::Failed, "checksum")),
        (
            422,
            Some("paste_too_long"),
            ends(RunResult::Failed, "paste_too_long"),
        ),
        (
            418,
            Some("something_new"),
            ends(RunResult::Failed, "something_new"),
        ),
        (404, None, ends(RunResult::Failed, "404")),
        (
            403,
            Some("suspended"),
            Step::Stop(Stop::Suspended {
                message: "m".into(),
            }),
        ),
        (
            403,
            Some("update_required"),
            Step::Stop(Stop::UpdateRequired {
                message: "m".into(),
            }),
        ),
        (401, Some("credential_invalid"), Step::Unlinked),
        // The status first: these codes on another status are unknown.
        (409, Some("suspended"), ends(RunResult::Failed, "suspended")),
        (
            401,
            Some("update_required"),
            ends(RunResult::Failed, "update_required"),
        ),
        (429, Some("rate_limited"), Step::Wait { retry_after: None }),
        (429, None, Step::Wait { retry_after: None }),
    ];
    for (status, code, step) in cases {
        assert_eq!(
            on_refusal(&refused(status, code)),
            step,
            "{status} {code:?}"
        );
    }
}

#[test]
fn a_429_carries_its_retry_after() {
    // CMP-R50: waits Retry-After seconds; a download uses it to stay
    // dropped (CMP-R81, B4)
    let mut refusal = refused(429, Some("rate_limited"));
    refusal.retry_after = Some(Duration::from_secs(7));
    assert_eq!(
        on_refusal(&refusal),
        Step::Wait {
            retry_after: Some(Duration::from_secs(7))
        }
    );
}

#[test]
fn a_stop_without_a_message_shows_the_known_text() {
    // CMP UI, Status: the stop message is the refusal's message; DST
    // Contracts: update_required's message
    let mut refusal = refused(403, Some("update_required"));
    refusal.message = None;
    assert_eq!(
        on_refusal(&refusal),
        Step::Stop(Stop::UpdateRequired {
            message: UPDATE_REQUIRED.into()
        })
    );
}

#[tokio::test]
async fn run_causes_raise_nothing_while_the_first_run_window_is_open() {
    // CMP-R100: no notification is raised while the first-run window is
    // open, divergence, held characters and log cap included (CMP-R111)
    let h = Harness::new(15).await;
    h.notifications.hold();
    h.hold(&fork(12), 12);
    h.run(Trigger::Start).await;
    assert!(h.shown().is_empty());

    h.notifications.release();
    assert_eq!(h.shown(), [DIFFERS]);
}

#[tokio::test]
async fn held_characters_and_log_cap_wait_for_the_first_run_window_to_close() {
    // CMP-R100: no notification while the first-run window is open; a cause
    // that cleared meanwhile never shows (CMP-R110)
    let h = Harness::new(15).await;
    h.notifications.hold();
    h.pagle.script_uploads(vec![Some(refusal(422, "log_cap"))]);
    h.run(Trigger::Start).await;
    h.pagle
        .set_catch_log(json!({ "verification": "verified", "pendingReason": null,
                                  "heldCharacters": 1, "leftOff": null }));
    h.run(Trigger::Start).await;
    assert!(h.shown().is_empty());

    h.notifications.release();
    // The log cap cleared with the upload; the held characters remain.
    assert_eq!(h.shown(), [HELD]);
}

#[tokio::test]
async fn post_link_runs_count_until_each_has_ended() {
    // CMP-R100: "Uploading…" shows until each post-link run (CMP-R101) has
    // ended
    let h = Harness::new(15).await;
    let count = h.uploads.post_link_runs();
    assert_eq!(*count.borrow(), 0);
    h.uploads.trigger(LOG_A, Trigger::PostLink);
    assert_eq!(*count.borrow(), 1);
    h.uploads.run_queued(&h.ctx()).await;
    assert_eq!(*count.borrow(), 0);

    // A cancelled one has ended too (CMP-R63 step 1).
    h.uploads.trigger(LOG_A, Trigger::PostLink);
    h.uploads.cancel();
    assert_eq!(*count.borrow(), 0);
}

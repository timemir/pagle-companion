//! Upload runs end to end, against a stub backend that keeps a cursor
//! ([`FakePagle`]): the queue, the cursor, the pages, the whole-send flag,
//! the backoff, and what local state records.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use wiremock::{MockServer, ResponseTemplate};

use super::fake_pagle::{FakePagle, Seen};
use super::queue::Trigger;
use super::{Ctx, Env, RunEnd, Uploads};
use crate::api::Api;
use crate::catch_logs::{self, Confirmed, Line};
use crate::credential_store::testing::MemoryStore;
use crate::credential_store::{CredentialStore as _, Keychain};
use crate::device_flow::Approval;
use crate::link::{Hooks, Link};
use crate::notify::{Notifications, Notifier};
use crate::pagle_log::{self, Found, LocalLog};
use crate::readings::fixture::{LOG_A, account_folder, pagle_lua_text, save};
use crate::readings::{Reading, read};
use crate::state::{self, Answer, Loaded, LogState, RunResult, State, Store};
use crate::stub_server;
use crate::upload_doc;

const CREDENTIAL: &str = "pgc_test_credential";
const NOW: i64 = 1_791_800_000;

struct Quiet;

impl Notifier for Quiet {
    fn show(&self, _: &str) {}
}

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
}

struct Harness {
    tmp: tempfile::TempDir,
    server: MockServer,
    pagle: FakePagle,
    api: Api,
    link: Link,
    state: Store,
    notifications: Notifications<Quiet>,
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
        Self {
            api: stub_server::api(&server),
            state: Store::new(tmp.path().join("state.json"), state),
            link: Link::new(Keychain::new(Box::new(os), None), hooks),
            notifications: Notifications::new(Quiet),
            env: TestEnv::default(),
            confirmed: [(LOG_A.to_owned(), folder_text)].into(),
            tmp,
            server,
            pagle,
            uploads,
            folder,
        }
    }

    fn ctx(&self) -> Ctx<'_, Quiet, TestEnv> {
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

    /// The stub holds the first `upto` entries of `log`.
    fn hold(&self, log: &LocalLog, upto: u64) {
        self.pagle
            .hold(&upload_doc::page(log, 1, upto, 1).unwrap().document);
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
        self.state = Store::new(self.tmp.path().join("state.json"), state);
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
async fn a_refusal_ends_the_run_failed_and_held_back() {
    // CMP-R50: any other 4xx ends the run failed, held back (CMP-R45), with
    // the refusal's code and message; #29 handles each code
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
            text: "Uploaded 5 minutes ago".into()
        })
    );
    h.run(Trigger::UploadNow).await;
    assert_eq!(
        catch_logs::run_line(&h.log(), NOW),
        Some(Line::UpToDate { text: "Up to date" })
    );
    let json = serde_json::to_value(catch_logs::run_line(&h.log(), NOW)).unwrap();
    assert_eq!(json, json!({ "kind": "up-to-date", "text": "Up to date" }));
}

#[tokio::test]
async fn the_first_readings_update_is_the_start_and_later_scans_are_scans() {
    // CMP-R45: at start, then after each later scan
    let uploads = Uploads::new();
    assert_eq!(uploads.reading_trigger(false), Some(Trigger::Start));
    assert_eq!(uploads.reading_trigger(false), None);
    assert_eq!(uploads.reading_trigger(true), Some(Trigger::Scan));
}

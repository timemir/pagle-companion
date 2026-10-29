//! Uploading (CMP-R45 to CMP-R51, CMP-R55, CMP-R101): the one queue, the
//! upload run that sends a confirmed Catch log from the server's cursor, the
//! content checkpoints, Recovery staging, and the queue's retry backoff.
//!
//! [`Uploads`] holds the queue ([`queue`]) and runs its items one at a time:
//! [`Uploads::trigger`] queues a run (CMP-R45's triggers call it; whether
//! one may queue at all, linked and outside a stop, is the caller's), and a
//! run ([`Uploads::run_item`]) reads its file, checks CMP-R45's holds and
//! skip, reads the cursor, decides what to send ([`plan`]) and sends it in
//! pages, recording `lastRun`, `lastUploadedAt` and the semantic snapshot in
//! local state. The tray-mode wiring is in [`window`].
//!
//! Replies (CMP-R50) are classified into a [`Reply`], and every refusal into
//! the [`Step`] a run takes next with [`on_refusal`]: restarting once after a
//! `gap`, waiting `Retry-After` and resending on a `429` (the whole queue
//! waits, since it sends one request at a time), entering a stop
//! (`suspended`, `update_required`, held by the link), or ending the run with
//! a result, recorded as `lastRun`. What each result's row says is
//! `catch_logs::run_line`'s; the notifications a run's end raises or clears
//! (divergence, held characters, log cap; CMP-R111) are [`causes`].
//!
//! The in-game downloads (CMP-R81) are the queue's other items: the timer's
//! checks queue the due ones ([`Uploads::in_game_check`]) and [`download`]
//! sends them, classifying their refusals with the same [`on_refusal`].

pub mod download;
pub mod plan;
pub mod queue;
pub mod window;

#[cfg(test)]
mod fake_pagle;
#[cfg(test)]
mod tests;

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, HeaderMap, RETRY_AFTER};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Notify, watch};

use crate::catch_logs::{self, Confirmed};
use crate::device_flow::UPDATE_REQUIRED_TEXT;
use crate::in_game::folders::{DataFile, Qualifying, WriteReport};
use crate::in_game::timer::Schedule;
use crate::link::{Account, CREDENTIAL_INVALID, SendError, Stop};
use crate::notify::{Cause, CauseKey, Notifications, Notifier};
use crate::readings::{LocalRead, Reading};
use crate::state::{Answer, CatchLogState, LastRun, LogState, RunResult, State, Store};
use crate::upload_doc;

use download::DownloadEnd;
use plan::{Backoff, Cursor, End, FileNow, PAGE_SIZE, Plan};
use queue::{Item, Queue, Run, Trigger};

/// The refusal code of a `gap` (UPL-R31); a run that fails on a second one
/// isn't held back (CMP-R50).
pub const GAP: &str = "gap";

/// What a pending Recovery's row says while its confirmation waits to be
/// retried (CMP-R51).
pub const CONFIRM_RETRY: &str =
    "Pagle couldn't check Recovery just now. Pagle Companion will upload and check again shortly.";

/// Why Recovery isn't prepared while another Catch log's package waits in
/// the same install.
pub const ANOTHER_RECOVERY: &str = "Another Catch log in this WoW installation has a Recovery waiting in Pagle_Recovery. Log in to that WoW account so Pagle applies it, then use Upload now to finish it. Then prepare Recovery here.";

/// Gap restarts per run (CMP Tunables).
pub const GAP_RESTARTS: u32 = 1;

/// The suspended stop's message when the refusal carries none (UPL
/// Refusals, without the contact address it names).
pub const SUSPENDED_TEXT: &str = "This Player is suspended.";

/// What a run needs from the rest of the Companion.
pub trait Env: Sync {
    /// Reads the account folder's `Pagle.lua` (CMP-R30), as a run does when
    /// it starts (CMP-R46). It may block for the read attempts, so it runs
    /// off the async runtime's threads.
    fn read(&self, folder: &str) -> impl Future<Output = Reading> + Send;
    /// Waits `wait` (CMP-R51's backoff).
    fn sleep(&self, wait: Duration) -> impl Future<Output = ()> + Send;
    /// Unix seconds now.
    fn now(&self) -> i64;
    /// Local state changed: the window and the tray show it again.
    fn changed(&self);
    /// Which installs qualify for the data addon folders now, and which of
    /// them lack each data file (CMP-R80).
    fn qualifying(&self) -> Qualifying;
    /// Whether `file` in every qualifying install hashes to `etag`, so the
    /// download may send `If-None-Match` (CMP-R81). It reads the files, so it
    /// runs off the async runtime's threads.
    fn data_files_hash_to(&self, file: DataFile, etag: &str) -> impl Future<Output = bool> + Send;
    /// Writes a body that passed CMP-R82 as `file`'s data file into every
    /// qualifying install (CMP-R83), off the async runtime's threads.
    fn write(&self, file: DataFile, body: Vec<u8>) -> impl Future<Output = WriteReport> + Send;
    /// Stages one private Recovery for the selected account's install.
    fn stage_recovery(
        &self,
        _log_id: &str,
        folder: &str,
        lua: Vec<u8>,
    ) -> impl Future<Output = Result<bool, String>> + Send {
        async move {
            crate::in_game::folders::stage_recovery(folder, &lua)
                .map_err(|e| format!("Couldn't stage Recovery: {e}"))
        }
    }
    /// Retires only the package staged by this Recovery, after confirmation.
    fn retire_recovery(
        &self,
        pending: &crate::recovery::Pending,
    ) -> impl Future<Output = Result<(), String>> + Send {
        let pending = pending.clone();
        async move {
            crate::in_game::folders::retire_recovery(&pending)
                .map_err(|e| format!("Couldn't retire completed Recovery; try Upload again: {e}"))
        }
    }
    /// Rewrites each data addon folder's TOC that `Pagle.toc` has moved on
    /// from (CMP-R83): at every in-game check.
    fn refresh_tocs(&self);
}

/// What one queue item works with.
pub struct Ctx<'a, N: Notifier, E: Env> {
    /// Requests that carry the credential (CMP-R49, CMP-R55).
    pub account: Account<'a, N>,
    pub state: &'a Store,
    /// The running Companion version, kept with each run (CMP-R45).
    pub version: &'a str,
    /// The confirmed `logId`s with their folders, as the item starts.
    pub confirmed: &'a Confirmed,
    pub env: &'a E,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    Recovery(Result<String, String>),
    /// It ended with a result, recorded as `lastRun` (CMP Data).
    Recorded(RunResult),
    /// Stopped (switched off, Unlink, **Link again…**, `credential_invalid`)
    /// or nothing could be sent: `lastRun` stays as it was (CMP-R50).
    Interrupted,
    /// It never read the cursor: not confirmed, not linked, the file doesn't
    /// hold the `logId`, held back or skipped (CMP-R45). The text is for the
    /// local log file.
    NotStarted(&'static str),
    /// The item was an in-game download, which ended so (CMP-R81).
    Download(DownloadEnd),
}

/// A reply other than a success or a retry: a refusal, by its HTTP status
/// and its code (CMP-R50).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    /// The body's `reason`, `code` or `error`, whichever is present; `None`
    /// for a body that isn't JSON or has no code ("any other `4xx`").
    pub code: Option<String>,
    pub message: Option<String>,
    /// `Retry-After`, in seconds, when present and readable (for `429`).
    pub retry_after: Option<Duration>,
}

/// What a reply to a cursor read or a page is (CMP-R50), classified by HTTP
/// status first, then by its code.
#[derive(Debug)]
pub enum Reply<T> {
    /// A `2xx` whose body reads as `T`.
    Ok(T),
    /// Any `5xx` (JSON or not), a timeout or no connection, or a `2xx`
    /// whose body doesn't read: wait the retry backoff, then restart the run
    /// from step 1 (CMP-R51). The text is for the local log file.
    Retry(String),
    /// Any other status: a refusal (CMP-R50's table, [`on_refusal`]).
    Refused(Refusal),
    /// Nothing was sent (not linked, the keyring doesn't answer, the store
    /// failed), or the credential was refused and CMP-R65 has run: the run
    /// ends and `lastRun` stays as it was.
    Ended(String),
}

/// What a run does after a refusal (CMP-R50): classified by its HTTP
/// status first (`429`; `401` and `403` for the codes the table gives
/// them), then by its code. Whether a result is held back is CMP-R45's
/// ([`plan::held_back`]); a refusal without a code, or with a code this
/// Companion doesn't know, is "any other `4xx`": **failed**, held back.
pub fn on_refusal(refusal: &Refusal) -> Step {
    let code = refusal.code.as_deref();
    let message = |known: &str| refusal.message.clone().unwrap_or_else(|| known.into());
    let end = |result| Step::End {
        result,
        code: Some(code.map_or_else(|| refusal.status.to_string(), str::to_owned)),
        message: refusal.message.clone(),
    };
    match (refusal.status, code) {
        (429, _) => Step::Wait {
            retry_after: refusal.retry_after,
        },
        (401, Some(CREDENTIAL_INVALID)) => Step::Unlinked,
        (403, Some("suspended")) => Step::Stop(Stop::Suspended {
            message: message(SUSPENDED_TEXT),
        }),
        (403, Some("update_required")) => Step::Stop(Stop::UpdateRequired {
            message: message(UPDATE_REQUIRED_TEXT),
        }),
        (_, Some(GAP | "invalid_base")) => Step::Restart,
        (_, Some("divergence" | "recovery_required")) => end(RunResult::Divergence),
        (_, Some("stale")) => end(RunResult::Stale),
        (_, Some("deleted")) => end(RunResult::Deleted),
        (_, Some("dev_log")) => end(RunResult::DevLog),
        (_, Some("unsupported_format")) => end(RunResult::WaitingForPagle),
        (_, Some("log_cap")) => end(RunResult::LogCap),
        // `unsupported_document`, `entry_cap`, `too_large`, `malformed`,
        // `checksum`, any other code or none.
        _ => end(RunResult::Failed),
    }
}

/// What a run does next after a refusal (CMP-R50).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// End the run with this result, recorded as `lastRun`.
    End {
        result: RunResult,
        code: Option<String>,
        message: Option<String>,
    },
    /// `gap`: read the cursor again and restart the run, once
    /// ([`GAP_RESTARTS`]); a second one ends it **failed**, not held back.
    Restart,
    /// `429`: wait `retry_after` (the retry backoff when it is missing or
    /// unreadable), then resend the same request; the whole queue waits. A
    /// download is dropped instead, not due again before it passes.
    Wait { retry_after: Option<Duration> },
    /// Enter the stop: the run ends, leaving `lastRun` as it was.
    Stop(Stop),
    /// `credential_invalid`: no longer linked (CMP-R65, which
    /// [`Account::send`] has run); the run ends, leaving `lastRun` as it
    /// was.
    Unlinked,
}

/// The body of a reply to a page (UPL Contracts, Reply), as far as a run
/// reads it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadReply {
    /// `applied` or `unchanged` on a `2xx`.
    pub result: String,
    #[serde(default)]
    pub end: Option<End>,
    #[serde(default)]
    pub catch_log: Option<CatchLogState>,
}

/// One pass of a run from step 1 (CMP-R47).
enum Attempt {
    Retry(String),
    /// A `gap` (CMP-R50).
    Gap(Refusal),
    End(RunEnd),
}

/// The process's queue and the runs it sends. Reach it with
/// `app.state::<Uploads>()`.
pub struct Uploads {
    inner: Mutex<Inner>,
    /// Woken when an item is queued.
    queued: Notify,
    /// Bumped when the item in progress is to stop, to end a backoff wait.
    interrupts: watch::Sender<u64>,
    backoff: Mutex<Backoff>,
    /// Whether the start's runs have been triggered (CMP-R45: "at start").
    started: AtomicBool,
    #[cfg(test)]
    recovery_test_key: bool,
    /// When the in-game downloads are due (CMP-R81).
    schedule: Mutex<Schedule>,
    /// How many post-link runs are queued or in progress (CMP-R101).
    post_link: watch::Sender<usize>,
}

#[derive(Default)]
struct Inner {
    queue: Queue,
    /// The item in progress stops after its current request.
    stop: bool,
}

impl Default for Uploads {
    fn default() -> Self {
        Self::new()
    }
}

impl Uploads {
    pub fn new() -> Self {
        Self {
            inner: Mutex::default(),
            queued: Notify::new(),
            interrupts: watch::channel(0).0,
            backoff: Mutex::default(),
            started: AtomicBool::new(false),
            #[cfg(test)]
            recovery_test_key: false,
            schedule: Mutex::default(),
            post_link: watch::channel(0).0,
        }
    }

    #[cfg(test)]
    fn with_recovery_test_key() -> Self {
        Self {
            recovery_test_key: true,
            ..Self::new()
        }
    }

    /// Queues a run of `log_id` for `trigger`, or merges it into the one
    /// queued (CMP-R46).
    pub fn trigger(&self, log_id: &str, trigger: Trigger) {
        let mut inner = self.lock();
        if inner.queue.push(log_id, trigger) {
            log::info!("queued an upload run for {log_id} ({trigger:?})");
        }
        self.count_post_link(&inner);
        drop(inner);
        self.queued.notify_one();
    }

    pub fn trigger_recovery(&self, log_id: &str) {
        self.lock().queue.push_recovery(log_id);
        self.queued.notify_one();
    }

    async fn recover<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        log_id: &str,
    ) -> Result<String, String> {
        use crate::recovery::{self, snapshot};
        use base64::Engine as _;
        let folder = ctx
            .confirmed
            .get(log_id)
            .ok_or("Choose the folder for this Catch log first")?;
        if !ctx.account.is_linked() || ctx.account.is_stopped() {
            return Err("Link Pagle Companion before preparing Recovery".into());
        }
        let source = recovery::read_source(folder)?;
        if source.log_id != log_id {
            return Err("The selected folder now holds another Catch log".into());
        }
        let bytes = snapshot::encode(&source);
        if bytes.len() > snapshot::CAP {
            return Err("Recovery snapshot too large".into());
        }
        // The addon loads exactly one package per install: never replace
        // another Catch log's pending one, which would still say it's ready.
        if let Some(staged) = crate::in_game::folders::staged_recovery_digest(folder)
            && ctx.state.read(|s| {
                s.logs.iter().any(|(other, log)| {
                    other != log_id
                        && log
                            .recovery
                            .as_ref()
                            .is_some_and(|p| p.package_digest.as_deref() == Some(staged.as_str()))
                })
            })
        {
            return Err(ANOTHER_RECOVERY.into());
        }
        let cursor_path = format!("catch-logs/{log_id}/cursor");
        let checkpoint = match self
            .request::<Cursor, _, _>(ctx, Method::GET, &cursor_path, |r| r)
            .await
        {
            Reply::Ok(cursor) => cursor.end,
            _ => return Err("Couldn't read the current checkpoint; try Recovery again".into()),
        };
        if self.stopping() {
            return Err("Recovery stopped".into());
        }
        let body = serde_json::json!({ "snapshot":base64::engine::general_purpose::STANDARD.encode(bytes), "checkpoint": {"seq":checkpoint.seq,"digest":checkpoint.digest,"revision":checkpoint.revision} });
        let path = format!("catch-logs/{log_id}/recovery");
        // Recovery has a separate cap: a complete private snapshot is much
        // larger than a public content download or an ordinary JSON reply.
        let signed = match self
            .request_capped::<recovery::Signed, _, _>(
                ctx,
                Method::POST,
                &path,
                recovery::REPLY_CAP,
                |r| r.json(&body),
            )
            .await
        {
            Reply::Ok(signed) => signed,
            Reply::Refused(refusal) => {
                return Err(refusal
                    .message
                    .unwrap_or_else(|| recovery::refusal_message(refusal.code.as_deref()).into()));
            }
            _ => return Err("Couldn't prepare Recovery; try again".into()),
        };
        #[cfg(test)]
        let verifier = if self.recovery_test_key {
            recovery::verify_test
        } else {
            recovery::verify
        };
        #[cfg(not(test))]
        let verifier = recovery::verify;
        let mut verified = verifier(signed, &source, &checkpoint)?;
        let same_selection = ctx.state.read(|s| {
            s.logs.get(log_id).is_some_and(|log| {
                crate::installs::paths::same_folder_key(
                    log.folder.as_deref().unwrap_or(&log.last_folder),
                ) == crate::installs::paths::same_folder_key(folder)
            })
        });
        if self.stopping()
            || !same_selection
            || !ctx.account.is_linked()
            || recovery::snapshot_digest(&recovery::read_source(folder)?)
                != recovery::snapshot_digest(&source)
        {
            return Err("Your full local Catch log changed. Prepare Recovery again.".into());
        }
        // Persist the identity before staging so a crash after the atomic
        // write still leads to a real Upload proof on restart.
        verified.pending.staged_folder = Some(folder.clone());
        verified.pending.instructions =
            "Recovery prepared; staging in the selected WoW folder.".into();
        ctx.state
            .update(|s| {
                if let Some(log) = s.logs.get_mut(log_id) {
                    log.recovery = Some(verified.pending.clone());
                    log.recovery_notice = None;
                }
            })
            .map_err(|_| "Couldn't save Recovery state")?;
        let new = ctx.env.stage_recovery(log_id, folder, verified.lua).await?;
        let instruction = if new {
            "Recovery is ready. Return to character selection so WoW discovers Pagle_Recovery, then log in. Your original branch is preserved. Reload or log out to save, then Upload again to check consistency."
        } else {
            "Recovery is ready. If WoW has already discovered Pagle_Recovery, use /reload; otherwise return to character selection, then log in. Your original branch is preserved. Reload or log out again to save, then Upload again to check consistency."
        };
        let instruction = format!(
            "Recovery restores {} accepted entries; {} local records remain staged for review. {instruction}",
            verified.pending.accepted_seq, verified.pending.staged
        );
        ctx.state
            .update(|s| {
                if let Some(log) = s.logs.get_mut(log_id)
                    && let Some(pending) = &mut log.recovery
                {
                    pending.instructions = instruction.clone();
                }
            })
            .map_err(|_| "Couldn't save Recovery instructions")?;
        ctx.env.changed();
        Ok(instruction)
    }

    /// How many post-link runs (CMP-R101) are queued or in progress, kept
    /// current: the first-run window shows "Uploading…" until it is 0
    /// (CMP-R100).
    pub fn post_link_runs(&self) -> watch::Receiver<usize> {
        self.post_link.subscribe()
    }

    fn count_post_link(&self, inner: &Inner) {
        let count = inner.queue.post_link_runs();
        self.post_link
            .send_if_modified(|n| std::mem::replace(n, count) != count);
    }

    /// The trigger a readings update makes for every confirmed `logId`
    /// (CMP-R45): the first update is the start's (no skip), each later
    /// scan's a scan's (with its skip); other updates make none.
    pub fn reading_trigger(&self, scan: bool) -> Option<Trigger> {
        if !self.started.swap(true, Ordering::SeqCst) {
            Some(Trigger::Start)
        } else if scan {
            Some(Trigger::Scan)
        } else {
            None
        }
    }

    /// **Upload** switched off (CMP-R41): drops the queued run of `log_id`,
    /// and stops a running one after its current request.
    pub fn stop_runs(&self, log_id: &str) {
        let mut inner = self.lock();
        let running = inner.queue.drop_runs(log_id);
        self.count_post_link(&inner);
        if running {
            inner.stop = true;
            drop(inner);
            self.interrupts.send_modify(|n| *n += 1);
        }
    }

    /// Cancels every queued and running item (CMP-R63 step 1, CMP-R64,
    /// CMP-R65): a running one stops after its current request.
    pub fn cancel(&self) {
        let mut inner = self.lock();
        let running = inner.queue.clear();
        self.count_post_link(&inner);
        if running {
            inner.stop = true;
            drop(inner);
            self.interrupts.send_modify(|n| *n += 1);
        }
        log::info!("cancelled the upload queue");
    }

    /// Whether an upload run is in progress or queued (DST-R76:
    /// the idle install waits for it).
    pub fn run_under_way(&self) -> bool {
        self.lock().queue.has_runs()
    }

    /// Waits for the next item and marks it in progress. Pass it to
    /// [`Uploads::run_item`].
    pub async fn next(&self) -> Item {
        loop {
            if let Some(item) = self.take() {
                return item;
            }
            self.queued.notified().await;
        }
    }

    fn take(&self) -> Option<Item> {
        let mut inner = self.lock();
        let item = inner.queue.pop()?;
        inner.stop = false;
        self.count_post_link(&inner);
        Some(item)
    }

    /// Runs an item from [`Uploads::next`] to its end.
    pub async fn run_item<N: Notifier, E: Env>(&self, ctx: &Ctx<'_, N, E>, item: Item) -> RunEnd {
        let end = match &item {
            Item::Run(run) => self.run(ctx, run).await,
            Item::Download(file) => RunEnd::Download(self.download(ctx, *file).await),
            Item::Recovery(log_id) => {
                self.save(ctx, log_id, |log| {
                    log.recovery_notice = Some("Preparing Recovery…".into())
                });
                ctx.env.changed();
                let result = self.recover(ctx, log_id).await;
                if let Err(message) = &result {
                    let message = message.clone();
                    self.save(ctx, log_id, |log| {
                        log.recovery_notice = Some(message.clone());
                        if let Some(pending) = &mut log.recovery {
                            pending.instructions = message.clone();
                        }
                        if let Some(run) = &mut log.last_run {
                            run.message = Some(message);
                        }
                    });
                    ctx.env.changed();
                }
                RunEnd::Recovery(result)
            }
        };
        let mut inner = self.lock();
        inner.queue.done();
        self.count_post_link(&inner);
        if let (Item::Download(file), RunEnd::Download(DownloadEnd::Retry)) = (&item, &end) {
            // CMP-R50, CMP-R51: after the backoff the download starts again,
            // behind any upload run that queued meanwhile (CMP-R46).
            inner.queue.push_download(*file);
            drop(inner);
            self.queued.notify_one();
        }
        end
    }

    /// Runs every queued item, in order, until the queue is empty.
    #[cfg(test)]
    pub async fn run_queued<N: Notifier, E: Env>(&self, ctx: &Ctx<'_, N, E>) -> Vec<RunEnd> {
        let mut ends = Vec::new();
        while let Some(item) = self.take() {
            ends.push(self.run_item(ctx, item).await);
        }
        ends
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stopping(&self) -> bool {
        self.lock().stop
    }

    /// An upload run (CMP-R46, CMP-R47): reads the file once, then works on
    /// that local log to its end, from step 1 again after each retry.
    async fn run<N: Notifier, E: Env>(&self, ctx: &Ctx<'_, N, E>, run: &Run) -> RunEnd {
        let log_id = run.log_id.as_str();
        let Some(folder) = ctx.confirmed.get(log_id) else {
            return not_started(log_id, "not confirmed");
        };
        if !ctx.account.is_linked() {
            // CMP-R55: nothing is sent while not linked.
            return not_started(log_id, "not linked");
        }
        if ctx.account.is_stopped() {
            // CMP-R45: runs start outside a stop only.
            return not_started(log_id, "stopped");
        }
        let read = match ctx.env.read(folder).await {
            Reading::Valid(read) if read.log.log_id == log_id && !read.log.is_empty() => read,
            _ => return not_started(log_id, "its file doesn't hold it"),
        };
        let last = ctx
            .state
            .read(|s| s.logs.get(log_id).and_then(|log| log.last_run.clone()));
        let now = FileNow {
            file: read.file,
            format: read.log.format,
            companion_version: ctx.version,
        };
        let semantic = crate::recovery::snapshot_digest(&read.log);
        let (changed, recovery) = ctx.state.read(|s| {
            s.logs.get(log_id).map_or((true, None), |log| {
                (
                    log.observed_snapshot.as_deref() != Some(semantic.as_str()),
                    log.recovery.clone(),
                )
            })
        });
        if recovery
            .as_ref()
            .and_then(|pending| pending.expected_digest.as_deref())
            == Some(semantic.as_str())
        {
            // The addon hasn't applied the package yet. Sending this original
            // branch again would be refused and move the backend's checkpoint
            // past the one the plan pins, so confirmation could never pass.
            return not_started(log_id, "Recovery isn't applied yet");
        }
        let recovering = recovery.is_some();
        let changed_refusal = changed
            && last
                .as_ref()
                .is_some_and(|run| matches!(run.result, RunResult::Failed | RunResult::TooLarge));
        if !recovering && !changed_refusal && plan::held_back(last.as_ref(), now, run.props) {
            return not_started(log_id, "held back");
        }
        log::info!(
            "upload run for {log_id}: {} entries as of {}",
            read.log.len(),
            read.as_of
        );
        let mut gaps = 0;
        loop {
            match self.attempt(ctx, run, &read).await {
                Attempt::End(end) => return end,
                Attempt::Retry(why) => {
                    if !self.wait_backoff(ctx, log_id, &why).await {
                        log::info!("upload run for {log_id} stopped");
                        return RunEnd::Interrupted;
                    }
                }
                Attempt::Gap(refusal) => {
                    gaps += 1;
                    if gaps > GAP_RESTARTS {
                        // CMP-R50: rechecking did not resolve the gap.
                        let message = refusal.message;
                        let code = Some("recovery_required".to_owned());
                        return self.record(
                            ctx,
                            log_id,
                            &read,
                            RunResult::Divergence,
                            code,
                            message,
                            None,
                        );
                    }
                    log::warn!("{log_id}: gap; reading the cursor again");
                }
            }
        }
    }

    /// CMP-R47 from step 1: the cursor, the plan, the pages.
    async fn attempt<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        run: &Run,
        read: &LocalRead,
    ) -> Attempt {
        let log_id = run.log_id.as_str();
        let path = format!("catch-logs/{log_id}/cursor");
        let cursor = match self
            .request::<Cursor, _, _>(ctx, Method::GET, &path, |r| r)
            .await
        {
            Reply::Ok(cursor) => cursor.end,
            other => return self.not_ok(ctx, log_id, read, other),
        };
        if self.stopping() {
            return self.interrupted(log_id);
        }
        let current = crate::recovery::snapshot_digest(&read.log);
        let changed = ctx.state.read(|s| {
            s.logs
                .get(log_id)
                .and_then(|log| log.uploaded_snapshot.as_deref())
                != Some(current.as_str())
        });
        let recovery = ctx
            .state
            .read(|s| s.logs.get(log_id).and_then(|log| log.recovery.clone()));
        let first = match plan::plan(
            &read.log,
            &cursor,
            run.props.resend || changed || recovery.is_some(),
        ) {
            Plan::UpToDate => {
                return Attempt::End(self.finish(ctx, log_id, read, RunResult::UpToDate, None));
            }
            Plan::Divergence => {
                let shorter = cursor.seq > read.log.len();
                return Attempt::End(self.record(ctx,log_id,read,RunResult::Divergence,
                    shorter.then(||"recovery_required".into()),
                    shorter.then(||"This computer has a shorter backup. Prepare Recovery to restore Accepted history.".into()),None));
            }
            Plan::Send { from } => from,
        };
        let first = if recovery.is_some() { 1 } else { first };
        let total = read.log.len();
        let as_of = u64::try_from(read.as_of).unwrap_or(0);
        let mut catch_log = None;
        let mut from = first;
        while from <= total {
            let page = match plan::page(&read.log, from, as_of, PAGE_SIZE) {
                Ok(page) => page,
                Err(too_large) => {
                    log::warn!("entry {} of {log_id} is over a cap", too_large.seq);
                    return Attempt::End(self.finish(ctx, log_id, read, RunResult::TooLarge, None));
                }
            };
            let body = page.body.clone();
            let sent = self
                .request::<UploadReply, _, _>(ctx, Method::POST, "uploads", |r| {
                    r.header(CONTENT_TYPE, upload_doc::CONTENT_TYPE)
                        .body(body.clone())
                })
                .await;
            let reply = match sent {
                Reply::Ok(reply) => reply,
                other => return self.not_ok(ctx, log_id, read, other),
            };
            let last = page.from + page.count - 1;
            log::info!(
                "{log_id}: entries {}–{last} of {total} {}",
                page.from,
                reply.result
            );
            let at = ctx.env.now();
            self.save(ctx, log_id, |log| {
                // CMP-R50: "last uploaded", per page.
                log.last_uploaded_at = Some(at);
            });
            ctx.env.changed();
            if reply.catch_log.is_some() {
                catch_log = reply.catch_log;
            }
            if self.stopping() {
                return self.interrupted(log_id);
            }
            // UPL-R33: the next page starts at f + count, never at the
            // reply's end.
            from = last + 1;
        }
        if let Some(pending) = recovery {
            let path = format!(
                "catch-logs/{log_id}/recovery/{}/confirm",
                pending.application_id
            );
            match self
                .request::<serde_json::Value, _, _>(ctx, Method::POST, &path, |r| r)
                .await
            {
                Reply::Ok(body)
                    if body.get("result").and_then(|v| v.as_str()) == Some("consistent") =>
                {
                    match ctx.env.retire_recovery(&pending).await {
                        Ok(()) => self.save(ctx, log_id, |log| {
                            log.recovery = None;
                            log.recovery_notice = None;
                        }),
                        Err(message) => self.save(ctx, log_id, |log| {
                            if let Some(pending) = &mut log.recovery {
                                pending.instructions = message;
                            }
                        }),
                    }
                }
                Reply::Ended(_) => return self.interrupted(log_id),
                Reply::Retry(why) => {
                    // CMP-R51: a busy (`503 validation_unavailable`) or
                    // unreachable confirmation waits the retry backoff, then
                    // the run starts again from step 1, so the proof Upload
                    // names the current revision. Recovery stays pending.
                    self.save(ctx, log_id, |log| {
                        if let Some(pending) = &mut log.recovery {
                            pending.instructions = CONFIRM_RETRY.into();
                        }
                    });
                    ctx.env.changed();
                    return Attempt::Retry(format!("Recovery confirmation: {why}"));
                }
                other => {
                    let code = match &other {
                        Reply::Refused(refusal) => refusal.code.as_deref(),
                        _ => None,
                    };
                    self.save(ctx, log_id, |log| {
                        if let Some(pending) = &mut log.recovery {
                            pending.instructions = crate::recovery::refusal_message(code).into();
                        }
                    });
                }
            }
        }
        self.save(ctx, log_id, |log| log.uploaded_snapshot = Some(current));
        Attempt::End(self.finish(ctx, log_id, read, RunResult::Uploaded, catch_log))
    }

    /// What a run does after a reply that isn't a success.
    fn not_ok<T, N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        log_id: &str,
        read: &LocalRead,
        reply: Reply<T>,
    ) -> Attempt {
        match reply {
            Reply::Ok(_) => unreachable!("handled by the caller"),
            Reply::Retry(why) => Attempt::Retry(why),
            Reply::Ended(why) => {
                log::info!("upload run for {log_id} ended: {why}");
                Attempt::End(RunEnd::Interrupted)
            }
            Reply::Refused(refusal) => {
                // CMP-R50: the reply goes to the local log file (refusal
                // codes and the backend's messages hold no character names,
                // CMP-R112).
                log::warn!(
                    "{log_id}: refused with {} {}: {}",
                    refusal.status,
                    refusal.code.as_deref().unwrap_or("(no code)"),
                    refusal.message.as_deref().unwrap_or("(no message)")
                );
                match on_refusal(&refusal) {
                    Step::End {
                        result,
                        code,
                        message,
                    } => {
                        let end = self.record(ctx, log_id, read, result, code, message, None);
                        Attempt::End(end)
                    }
                    Step::Restart => Attempt::Gap(refusal),
                    // [`Uploads::request`] waits these out itself.
                    Step::Wait { .. } => Attempt::Retry("429".into()),
                    Step::Stop(stop) => {
                        ctx.account.enter_stop(stop);
                        Attempt::End(RunEnd::Interrupted)
                    }
                    Step::Unlinked => Attempt::End(RunEnd::Interrupted),
                }
            }
        }
    }

    fn interrupted(&self, log_id: &str) -> Attempt {
        log::info!("upload run for {log_id} stopped after its request");
        Attempt::End(RunEnd::Interrupted)
    }

    /// Waits the queue's retry backoff (CMP-R51). `false` when the run was
    /// stopped meanwhile. A new trigger doesn't shorten it.
    async fn wait_backoff<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        log_id: &str,
        why: &str,
    ) -> bool {
        let wait = lock(&self.backoff).next_delay();
        log::warn!(
            "upload run for {log_id}: {why}; trying again in {} s",
            wait.as_secs()
        );
        self.wait(ctx, wait).await
    }

    /// Waits `wait`, with the whole queue: the item in progress holds it.
    /// `false` when the item was stopped meanwhile (switched off, Unlink,
    /// **Link again…**, a stop).
    async fn wait<N: Notifier, E: Env>(&self, ctx: &Ctx<'_, N, E>, wait: Duration) -> bool {
        let mut interrupts = self.interrupts.subscribe();
        if self.stopping() {
            return false;
        }
        let sleep = ctx.env.sleep(wait);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                () = &mut sleep => return !self.stopping(),
                changed = interrupts.changed() => {
                    if changed.is_err() || self.stopping() {
                        return false;
                    }
                }
            }
        }
    }

    /// Sends one request needing the credential and classifies its reply
    /// (CMP-R50, CMP-R51). A success resets the backoff. A `429` waits
    /// `Retry-After` (or the retry backoff) and sends the same request again
    /// (CMP-R50), so it never comes back.
    async fn request<T, N, E>(
        &self,
        ctx: &Ctx<'_, N, E>,
        method: Method,
        path: &str,
        build: impl Fn(RequestBuilder) -> RequestBuilder,
    ) -> Reply<T>
    where
        T: DeserializeOwned,
        N: Notifier,
        E: Env,
    {
        self.request_capped(ctx, method, path, crate::api::REPLY_CAP, build)
            .await
    }

    async fn request_capped<T, N, E>(
        &self,
        ctx: &Ctx<'_, N, E>,
        method: Method,
        path: &str,
        cap: usize,
        build: impl Fn(RequestBuilder) -> RequestBuilder,
    ) -> Reply<T>
    where
        T: DeserializeOwned,
        N: Notifier,
        E: Env,
    {
        loop {
            let reply = self
                .request_once(ctx, method.clone(), path, cap, &build)
                .await;
            let Reply::Refused(refusal) = &reply else {
                return reply;
            };
            let retry_after = match on_refusal(refusal) {
                Step::Stop(stop) => {
                    ctx.account.enter_stop(stop);
                    return Reply::Ended("entered a required stop".into());
                }
                Step::Wait { retry_after } => retry_after,
                _ => return reply,
            };
            let wait = retry_after.unwrap_or_else(|| lock(&self.backoff).next_delay());
            log::warn!(
                "{path}: 429; the queue waits {} s and sends it again",
                wait.as_secs()
            );
            if !self.wait(ctx, wait).await {
                return Reply::Ended("stopped while waiting out a 429".into());
            }
        }
    }

    /// One request of [`Uploads::request`].
    async fn request_once<T, N, E>(
        &self,
        ctx: &Ctx<'_, N, E>,
        method: Method,
        path: &str,
        cap: usize,
        build: &impl Fn(RequestBuilder) -> RequestBuilder,
    ) -> Reply<T>
    where
        T: DeserializeOwned,
        N: Notifier,
        E: Env,
    {
        let reply = match ctx.account.send(method, path, build).await {
            Ok(reply) => reply,
            Err(SendError::Http(e)) => return Reply::Retry(e.to_string()),
            Err(e) => return Reply::Ended(e.to_string()),
        };
        let status = reply.status();
        if status.is_server_error() {
            return Reply::Retry(status.to_string());
        }
        let retry_after = retry_after(reply.headers());
        let body = match crate::api::read_capped(reply, cap).await {
            Ok(body) => body,
            Err(e) => return Reply::Retry(e.to_string()),
        };
        if status.is_success() {
            return match serde_json::from_slice::<T>(&body) {
                Ok(value) => {
                    lock(&self.backoff).reset();
                    Reply::Ok(value)
                }
                Err(e) => Reply::Retry(format!("{status} with a body that doesn't read: {e}")),
            };
        }
        Reply::Refused(refusal(status, retry_after, &body))
    }

    /// Ends a run with `result` and no refusal.
    fn finish<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        log_id: &str,
        read: &LocalRead,
        result: RunResult,
        catch_log: Option<CatchLogState>,
    ) -> RunEnd {
        self.record(ctx, log_id, read, result, None, None, catch_log)
    }

    /// Records the run's end as `lastRun` (CMP Data): the file as read, its
    /// `format`, this Companion's version, and the reply's `catchLog`, or the
    /// one kept when the run got none (CMP UI: a run that sends nothing
    /// keeps the previous one).
    #[allow(clippy::too_many_arguments)]
    fn record<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        log_id: &str,
        read: &LocalRead,
        result: RunResult,
        code: Option<String>,
        message: Option<String>,
        catch_log: Option<CatchLogState>,
    ) -> RunEnd {
        let at = ctx.env.now();
        let version = ctx.version.to_owned();
        self.save(ctx, log_id, |log| {
            log.observed_snapshot = Some(crate::recovery::snapshot_digest(&read.log));
            let kept = log.last_run.take().and_then(|run| run.catch_log);
            log.last_run = Some(LastRun {
                result,
                at,
                file_mtime_ms: read.file.mtime_ms,
                file_size: read.file.size,
                format: i64::try_from(read.log.format).unwrap_or(i64::MAX),
                companion_version: version,
                code,
                message,
                catch_log: catch_log.or(kept),
            });
        });
        log::info!("upload run for {log_id} ended {result:?}");
        notify(ctx.state, ctx.account.notifications());
        ctx.env.changed();
        RunEnd::Recorded(result)
    }

    /// Changes the run's `logId` in local state.
    fn save<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        log_id: &str,
        change: impl FnOnce(&mut LogState),
    ) {
        let saved = ctx.state.update(|s| {
            if let Some(log) = s.logs.get_mut(log_id) {
                change(log);
            }
        });
        if let Err(e) = saved {
            log::error!("couldn't write state.json: {e}");
        }
    }
}

/// The notifications runs' ends raise (CMP-R111), from each `logId`'s
/// `lastRun`: `Some` when the cause is present, `None` when it has cleared.
/// Only `logId`s answered **Upload** raise them.
/// - a divergence (CMP-R52), until a run ends otherwise;
/// - `heldCharacters > 0` in the `catchLog` kept (CMP-R53);
/// - `log_cap` (CMP-R50), one cause for the Player, with the refusal's
///   message.
pub fn causes(state: &State) -> Vec<(CauseKey, Option<Cause>)> {
    let mut causes = Vec::new();
    let mut log_cap = None;
    for (log_id, log) in &state.logs {
        let run = log
            .last_run
            .as_ref()
            .filter(|_| log.answer == Some(Answer::Upload));
        let result = run.map(|run| run.result);
        causes.push((
            CauseKey::Divergence(log_id.clone()),
            (result == Some(RunResult::Divergence)).then(|| Cause::Divergence {
                log_id: log_id.clone(),
            }),
        ));
        let held = run
            .and_then(|run| run.catch_log.as_ref())
            .is_some_and(catch_logs::has_held_characters);
        causes.push((
            CauseKey::HeldCharacters(log_id.clone()),
            held.then(|| Cause::HeldCharacters {
                log_id: log_id.clone(),
            }),
        ));
        if let Some(run) = run.filter(|run| run.result == RunResult::LogCap) {
            log_cap.get_or_insert_with(|| catch_logs::log_cap_text(run));
        }
    }
    causes.push((
        CauseKey::LogCap,
        log_cap.map(|message| Cause::LogCap { message }),
    ));
    causes
}

/// Raises and clears [`causes`] (CMP-R110: once per cause, again only after
/// it cleared).
pub fn notify<N: Notifier>(state: &Store, notifications: &Notifications<N>) {
    for (key, cause) in state.read(causes) {
        match cause {
            Some(cause) => {
                if notifications.raise(&cause) {
                    log::info!("notified: {key:?}");
                }
            }
            None => notifications.clear(&key),
        }
    }
}

/// A reply's `Retry-After`, in seconds, when present and readable (CMP-R50).
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// A refusal of `status` with `body` (CMP-R50): its code is the body's
/// `reason`, `code` or `error`, whichever is present.
fn refusal(status: StatusCode, retry_after: Option<Duration>, body: &[u8]) -> Refusal {
    let json = serde_json::from_slice::<serde_json::Value>(body).ok();
    let text = |key: &str| {
        json.as_ref()
            .and_then(|j| j.get(key)?.as_str().map(str::to_owned))
    };
    Refusal {
        status: status.as_u16(),
        code: ["reason", "code", "error"].iter().find_map(|key| text(key)),
        message: text("message"),
        retry_after,
    }
}

fn not_started(log_id: &str, why: &'static str) -> RunEnd {
    log::info!("no upload run for {log_id}: {why}");
    RunEnd::NotStarted(why)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

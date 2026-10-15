//! Uploading (CMP-R45 to CMP-R51, CMP-R55, CMP-R101): the one queue, the
//! upload run that sends a confirmed Catch log from the server's cursor, the
//! whole-send flag, and the queue's retry backoff.
//!
//! [`Uploads`] holds the queue ([`queue`]) and runs its items one at a time:
//! [`Uploads::trigger`] queues a run (CMP-R45's triggers call it; whether
//! one may queue at all, linked and outside a stop, is the caller's), and a
//! run ([`Uploads::run_item`]) reads its file, checks CMP-R45's holds and
//! skip, reads the cursor, decides what to send ([`plan`]) and sends it in
//! pages, recording `lastRun`, `lastUploadedAt` and `wholeSendPending` in
//! local state. The tray-mode wiring is in [`window`].
//!
//! Replies (CMP-R50) are classified into a [`Reply`]; this slice acts on
//! successes and on CMP-R51's retries, and ends a run at every refusal with
//! [`on_refusal`], which `b1-companion-replies` (#29) fills in row by row:
//! restarting after `gap`, waiting `Retry-After`, the stops, each result's
//! text.

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

use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Method, RequestBuilder};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Notify, watch};

use crate::catch_logs::Confirmed;
use crate::link::{Account, SendError};
use crate::notify::Notifier;
use crate::readings::{LocalRead, Reading};
use crate::state::{CatchLogState, LastRun, LogState, RunResult, Store};
use crate::upload_doc;

use plan::{Backoff, Cursor, End, FileNow, PAGE_SIZE, Plan};
use queue::{Item, Queue, Run, Trigger};

/// The refusal code of a `gap` (UPL-R31); a run that fails on a second one
/// isn't held back (CMP-R50).
pub const GAP: &str = "gap";

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
    /// It ended with a result, recorded as `lastRun` (CMP Data).
    Recorded(RunResult),
    /// Stopped (switched off, Unlink, **Link again…**, `credential_invalid`)
    /// or nothing could be sent: `lastRun` stays as it was (CMP-R50).
    Interrupted,
    /// It never read the cursor: not confirmed, not linked, the file doesn't
    /// hold the `logId`, held back or skipped (CMP-R45). The text is for the
    /// local log file.
    NotStarted(&'static str),
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

/// What a run does after a refusal (CMP-R50). This slice treats every
/// refusal as "any other `4xx`": the run ends **failed**, held back
/// (CMP-R45), with the refusal's code and message. #29 fills in the table's
/// other rows here (`gap`'s restart, `429`'s wait, the stops, each result).
pub fn on_refusal(refusal: &Refusal) -> Step {
    Step::End {
        result: RunResult::Failed,
        code: refusal
            .code
            .clone()
            .or_else(|| Some(refusal.status.to_string())),
        message: refusal.message.clone(),
    }
}

/// What a run does next after a reply that isn't a success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// End the run with this result, recorded as `lastRun`.
    End {
        result: RunResult,
        code: Option<String>,
        message: Option<String>,
    },
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
        }
    }

    /// Queues a run of `log_id` for `trigger`, or merges it into the one
    /// queued (CMP-R46).
    pub fn trigger(&self, log_id: &str, trigger: Trigger) {
        if self.lock().queue.push(log_id, trigger) {
            log::info!("queued an upload run for {log_id} ({trigger:?})");
        }
        self.queued.notify_one();
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
        if inner.queue.drop_runs(log_id) {
            inner.stop = true;
            drop(inner);
            self.interrupts.send_modify(|n| *n += 1);
        }
    }

    /// Cancels every queued and running item (CMP-R63 step 1, CMP-R64,
    /// CMP-R65): a running one stops after its current request.
    pub fn cancel(&self) {
        let mut inner = self.lock();
        if inner.queue.clear() {
            inner.stop = true;
            drop(inner);
            self.interrupts.send_modify(|n| *n += 1);
        }
        log::info!("cancelled the upload queue");
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
        Some(item)
    }

    /// Runs an item from [`Uploads::next`] to its end.
    pub async fn run_item<N: Notifier, E: Env>(&self, ctx: &Ctx<'_, N, E>, item: Item) -> RunEnd {
        let end = match &item {
            Item::Run(run) => self.run(ctx, run).await,
        };
        self.lock().queue.done();
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
        if plan::held_back(last.as_ref(), now, run.props) {
            return not_started(log_id, "held back");
        }
        if plan::skipped(last.as_ref(), read.file, run.props) {
            return not_started(log_id, "unchanged since its last run");
        }
        log::info!(
            "upload run for {log_id}: {} entries as of {}",
            read.log.len(),
            read.as_of
        );
        loop {
            match self.attempt(ctx, run, &read).await {
                Attempt::End(end) => return end,
                Attempt::Retry(why) => {
                    if !self.wait_backoff(ctx, log_id, &why).await {
                        log::info!("upload run for {log_id} stopped");
                        return RunEnd::Interrupted;
                    }
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
        let flag = ctx
            .state
            .read(|s| s.logs.get(log_id).is_some_and(|log| log.whole_send_pending));
        let first = match plan::plan(&read.log, &cursor, flag, run.props.resend) {
            Plan::UpToDate => {
                return Attempt::End(self.finish(ctx, log_id, read, RunResult::UpToDate, None));
            }
            Plan::Divergence => {
                return Attempt::End(self.finish(ctx, log_id, read, RunResult::Divergence, None));
            }
            Plan::Send { from, whole } => {
                if whole && !flag {
                    // CMP-R48: set when a whole send starts.
                    self.save(ctx, log_id, |log| log.whole_send_pending = true);
                }
                from
            }
        };
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
                    r.header(CONTENT_TYPE, upload_doc::CONTENT_TYPE).body(body)
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
                if last == total {
                    // CMP-R48: cleared when the page holding seq L is
                    // applied or unchanged.
                    log.whole_send_pending = false;
                }
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
                log::warn!(
                    "{log_id}: refused with {} {}",
                    refusal.status,
                    refusal.code.as_deref().unwrap_or("(no code)")
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
        let mut interrupts = self.interrupts.subscribe();
        if self.stopping() {
            return false;
        }
        let wait = lock(&self.backoff).next_delay();
        log::warn!(
            "upload run for {log_id}: {why}; trying again in {} s",
            wait.as_secs()
        );
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
    /// (CMP-R50, CMP-R51). A success resets the backoff.
    async fn request<T, N, E>(
        &self,
        ctx: &Ctx<'_, N, E>,
        method: Method,
        path: &str,
        build: impl FnOnce(RequestBuilder) -> RequestBuilder,
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
        let retry_after = reply
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = match reply.bytes().await {
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
        let json = serde_json::from_slice::<serde_json::Value>(&body).ok();
        let text = |key: &str| {
            json.as_ref()
                .and_then(|j| j.get(key)?.as_str().map(str::to_owned))
        };
        Reply::Refused(Refusal {
            status: status.as_u16(),
            code: ["reason", "code", "error"].iter().find_map(|key| text(key)),
            message: text("message"),
            retry_after,
        })
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

fn not_started(log_id: &str, why: &'static str) -> RunEnd {
    log::info!("no upload run for {log_id}: {why}");
    RunEnd::NotStarted(why)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

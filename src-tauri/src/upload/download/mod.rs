//! The in-game downloads (CMP-R81 to CMP-R84; IGV-R30 to IGV-R35, IGV-R53;
//! HEV-R37, HEV-R38): `GET /api/v1/in-game/following`,
//! `GET /api/v1/in-game/anglers` and `GET /api/v1/in-game/content` as the
//! queue's items.
//!
//! [`Uploads::in_game_check`] is the timer's check: it refreshes the TOCs and,
//! while linked and outside a stop, queues the downloads the [`Schedule`]
//! says are due. A download ([`Uploads::download`]) sends the credential, the
//! version header, `Accept-Encoding: gzip` and, for Anglers and Content,
//! `If-None-Match` when every qualifying install's data file hashes to the
//! kept `ETag`. It reads the decoded body up to [`DECODED_CAP`], checks it
//! (CMP-R82), writes it (CMP-R83), and keeps the time and the `ETag` of a
//! success in `inGame`. How it ended goes back into the schedule:
//! - a `200` written in at least one install, or a `304`: successful;
//! - a body that fails CMP-R82, or is written nowhere: held one following
//!   interval;
//! - a `429`, or a `503 content_unavailable` (HEV-R37): dropped, held until
//!   `Retry-After` has passed (the queue doesn't wait);
//! - a `5xx`, a timeout or no connection: the queue's backoff (CMP-R51), then
//!   queued again;
//! - a stop, `credential_invalid` or any other refusal: due again at the next
//!   check, which a stop or a lost link keeps from sending anything.
//!
//! Log lines name the download and its result, never what the body holds
//! (CMP-R112).

#[cfg(test)]
mod tests;

use reqwest::header::{ACCEPT_ENCODING, ETAG, IF_NONE_MATCH};

use super::*;
use crate::in_game::check::{self, ANGLERS_CAP};
use crate::in_game::folders::etag_of;
use crate::in_game::timer::FOLLOWING_INTERVAL;

/// The most decoded bytes a download reads: the Anglers cap (CMP Tunables).
/// No valid `Pagle_Anglers` body is longer, and the followed cap keeps any
/// `Pagle_Following` body far shorter, so a body that would pass it fails
/// CMP-R82 without being inflated further.
pub const DECODED_CAP: usize = ANGLERS_CAP;

/// The refusal code of a `503` before the backend's first content build has
/// stored a file (HEV-R37, HEV Contracts, Endpoints).
pub const CONTENT_UNAVAILABLE: &str = "content_unavailable";

/// How a download ended (CMP-R81).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadEnd {
    /// A `200` whose body passed CMP-R82, written in this many installs (at
    /// least one): successful.
    Written(usize),
    /// A `304`: successful, nothing written.
    NotModified,
    /// A `200` whose body failed CMP-R82: nothing written, not tried again
    /// before one following interval.
    Rejected,
    /// A `200` whose body passed CMP-R82 but couldn't be written to any
    /// install: not tried again before one following interval.
    Unwritten,
    /// `429`: dropped, not due again before `Retry-After` has passed.
    RateLimited,
    /// `503 content_unavailable` (HEV-R37): dropped like a `429`, not due
    /// again before `Retry-After` has passed.
    Unavailable,
    /// A refusal that ends the download: due again at the next check.
    Failed,
    /// A `5xx`, a timeout or no connection: the backoff was waited, and the
    /// queue takes the download again (CMP-R51).
    Retry,
    /// Stopped (Unlink, **Link again…**, a stop, `credential_invalid`) or
    /// nothing could be sent: nothing written.
    Interrupted,
    /// Never sent: not linked, a stop holds, or no install qualifies.
    NotStarted(&'static str),
}

/// A download's reply that isn't a refusal.
enum Fetched {
    /// A `200`'s decoded body, and its `ETag`.
    Body { body: Vec<u8>, etag: Option<String> },
    /// A `304`, and its `ETag`.
    NotModified { etag: Option<String> },
    /// A `200` whose decoded body is longer than [`DECODED_CAP`].
    TooLarge,
}

/// The endpoint of `file`'s download, under `/api/v1/` (IGV Contracts).
pub fn endpoint(file: DataFile) -> &'static str {
    match file {
        DataFile::Following => "in-game/following",
        DataFile::Anglers => "in-game/anglers",
        DataFile::Content => "in-game/content",
    }
}

/// The checker's name for `file`.
fn checked(file: DataFile) -> check::DataFile {
    match file {
        DataFile::Following => check::DataFile::Following,
        DataFile::Anglers => check::DataFile::Anglers,
        DataFile::Content => check::DataFile::Content,
    }
}

impl Uploads {
    /// Right after linking (CMP-R101): the in-game downloads are owed again.
    /// The caller then makes an in-game check.
    pub fn owe_downloads(&self) {
        lock(&self.schedule).owe_all();
    }

    /// The timer's check, at start and every in-game check interval
    /// (CMP-R81): refreshes the TOCs (CMP-R83) and, while linked and outside
    /// a stop, queues every download due. Returns the downloads queued.
    pub fn in_game_check<N: Notifier, E: Env>(&self, ctx: &Ctx<'_, N, E>) -> Vec<DataFile> {
        ctx.env.refresh_tocs();
        if !ctx.account.is_linked() || ctx.account.is_stopped() {
            return Vec::new();
        }
        let in_game = ctx.state.read(|s| s.in_game.clone());
        let due = lock(&self.schedule).due(ctx.env.now(), &in_game, &ctx.env.qualifying());
        let queued: Vec<DataFile> = {
            let mut inner = self.lock();
            due.into_iter()
                .filter(|&file| inner.queue.push_download(file))
                .collect()
        };
        for file in &queued {
            log::info!("queued the in-game download {}", file.folder());
        }
        if !queued.is_empty() {
            self.queued.notify_one();
        }
        queued
    }

    /// One in-game download (CMP-R81 to CMP-R84), and what its end does to
    /// the schedule.
    pub(super) async fn download<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        file: DataFile,
    ) -> DownloadEnd {
        let end = self.fetch_and_write(ctx, file).await;
        let now = ctx.env.now();
        let mut schedule = lock(&self.schedule);
        match &end {
            DownloadEnd::Written(_) | DownloadEnd::NotModified => schedule.succeeded(file),
            DownloadEnd::Rejected | DownloadEnd::Unwritten => {
                schedule.hold(file, now + FOLLOWING_INTERVAL);
            }
            // Held where the reply was read.
            DownloadEnd::RateLimited | DownloadEnd::Unavailable => {}
            DownloadEnd::Failed
            | DownloadEnd::Retry
            | DownloadEnd::Interrupted
            | DownloadEnd::NotStarted(_) => schedule.failed(file),
        }
        end
    }

    async fn fetch_and_write<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        file: DataFile,
    ) -> DownloadEnd {
        let name = file.folder();
        if !ctx.account.is_linked() {
            // IGV-R53: an unlinked Companion downloads nothing.
            return not_downloaded(name, "not linked");
        }
        if ctx.account.is_stopped() {
            // CMP-R81: only outside a stop.
            return not_downloaded(name, "stopped");
        }
        if !ctx.env.qualifying().any() {
            // CMP-R80: no in-game download while no install qualifies.
            return not_downloaded(name, "no install has the Pagle addon");
        }
        let if_none_match = match file {
            // IGV-R31: every Following answer is new, with no ETag.
            DataFile::Following => None,
            // IGV-R51, HEV-R37: the ETag is the data file's SHA-256.
            DataFile::Anglers | DataFile::Content => self.if_none_match(ctx, file).await,
        };
        log::info!(
            "in-game download {name}{}",
            if if_none_match.is_some() {
                " with If-None-Match"
            } else {
                ""
            }
        );
        let fetched = match self.fetch(ctx, file, if_none_match.as_deref()).await {
            Reply::Ok(fetched) => fetched,
            Reply::Retry(why) => return self.retry(ctx, name, &why).await,
            Reply::Ended(why) => {
                log::info!("in-game download {name} ended: {why}");
                return DownloadEnd::Interrupted;
            }
            Reply::Refused(refusal) => return self.refused(ctx, file, &refusal),
        };
        if self.stopping() || !ctx.account.is_linked() {
            // CMP-R63, CMP-R64: stopped after its request; nothing is written
            // into folders an Unlink deletes.
            log::info!("in-game download {name} stopped after its request");
            return DownloadEnd::Interrupted;
        }
        match fetched {
            Fetched::NotModified { etag } => {
                let Some(sent) = if_none_match else {
                    log::warn!("in-game download {name}: 304 to a request without If-None-Match");
                    return DownloadEnd::Failed;
                };
                // CMP-R81: a 304 is successful and writes no data file.
                self.downloaded(ctx, file, Some(etag.unwrap_or(sent)));
                log::info!("in-game download {name}: not modified");
                DownloadEnd::NotModified
            }
            Fetched::TooLarge => {
                log::warn!(
                    "in-game download {name}: the decoded body is over {DECODED_CAP} bytes \
                     (IGV-R41, CMP-R82); not tried again for one following interval"
                );
                DownloadEnd::Rejected
            }
            Fetched::Body { body, etag } => self.write(ctx, file, body, etag).await,
        }
    }

    /// The kept `ETag` of `file`, when every qualifying install's data file
    /// hashes to it (CMP-R81): `Anglers.lua`'s, or `Content.lua`'s "as the
    /// Anglers download does".
    async fn if_none_match<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        file: DataFile,
    ) -> Option<String> {
        let kept = ctx.state.read(|s| match file {
            DataFile::Following => None,
            DataFile::Anglers => s.in_game.anglers_etag.clone(),
            DataFile::Content => s.in_game.content_etag.clone(),
        })?;
        if ctx.env.data_files_hash_to(file, &kept).await {
            Some(kept)
        } else {
            None
        }
    }

    /// Sends the download and reads its reply (CMP-R50), the body decoded
    /// and read up to [`DECODED_CAP`].
    async fn fetch<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        file: DataFile,
        if_none_match: Option<&str>,
    ) -> Reply<Fetched> {
        let sent = ctx
            .account
            .send(Method::GET, endpoint(file), |r| {
                let r = r.header(ACCEPT_ENCODING, "gzip");
                match if_none_match {
                    Some(etag) => r.header(IF_NONE_MATCH, etag),
                    None => r,
                }
            })
            .await;
        let mut reply = match sent {
            Ok(reply) => reply,
            Err(SendError::Http(e)) => return Reply::Retry(e.to_string()),
            Err(e) => return Reply::Ended(e.to_string()),
        };
        let status = reply.status();
        if status == StatusCode::SERVICE_UNAVAILABLE {
            // HEV-R37: `503 content_unavailable` with `Retry-After` before
            // the first content build; any other 503 is a 5xx (CMP-R50).
            let retry_after = retry_after(reply.headers());
            return match reply.bytes().await {
                Ok(body) => {
                    let refused = refusal(status, retry_after, &body);
                    if refused.code.as_deref() == Some(CONTENT_UNAVAILABLE) {
                        Reply::Refused(refused)
                    } else {
                        Reply::Retry(status.to_string())
                    }
                }
                Err(_) => Reply::Retry(status.to_string()),
            };
        }
        if status.is_server_error() {
            return Reply::Retry(status.to_string());
        }
        let etag = reply
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if status == StatusCode::NOT_MODIFIED {
            lock(&self.backoff).reset();
            return Reply::Ok(Fetched::NotModified { etag });
        }
        if !status.is_success() {
            let retry_after = retry_after(reply.headers());
            return match reply.bytes().await {
                Ok(body) => Reply::Refused(refusal(status, retry_after, &body)),
                Err(e) => Reply::Retry(e.to_string()),
            };
        }
        // The client decodes a gzip body as it arrives, so stopping at the
        // cap inflates at most one chunk past it.
        let mut body = Vec::new();
        loop {
            match reply.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > DECODED_CAP {
                        lock(&self.backoff).reset();
                        return Reply::Ok(Fetched::TooLarge);
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                // A connection lost mid-body, or a body that doesn't decode.
                Err(e) => return Reply::Retry(format!("{status} whose body didn't read: {e}")),
            }
        }
        lock(&self.backoff).reset();
        Reply::Ok(Fetched::Body { body, etag })
    }

    /// CMP-R82, CMP-R83: checks the body, then writes it.
    async fn write<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        file: DataFile,
        body: Vec<u8>,
        etag: Option<String>,
    ) -> DownloadEnd {
        let name = file.folder();
        if let Err(rejection) = check::check(checked(file), &body) {
            // CMP-R82: never written; the failure goes to the local log file.
            log::warn!(
                "in-game download {name}: {rejection}; not tried again for one following interval"
            );
            return DownloadEnd::Rejected;
        }
        // The ETag is the body's SHA-256 (IGV-R32); without one, the kept
        // ETag is what the files written hash to.
        let etag = etag.unwrap_or_else(|| etag_of(&body));
        let report = ctx.env.write(file, body).await;
        if !report.succeeded() {
            // CMP-R81, CMP-R83: written nowhere, so not successful, and held
            // like a body that fails CMP-R82.
            log::warn!(
                "in-game download {name}: written to no install ({} failed); \
                 not tried again for one following interval",
                report.failed.len()
            );
            return DownloadEnd::Unwritten;
        }
        self.downloaded(ctx, file, Some(etag));
        log::info!(
            "in-game download {name}: written to {} install(s){}",
            report.written.len(),
            if report.failed.is_empty() {
                String::new()
            } else {
                format!(", failed in {}", report.failed.len())
            }
        );
        DownloadEnd::Written(report.written.len())
    }

    /// A successful download (CMP-R81): its time, and for Anglers and
    /// Content its `ETag`, go into `inGame`.
    fn downloaded<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        file: DataFile,
        etag: Option<String>,
    ) {
        let at = ctx.env.now();
        let saved = ctx.state.update(|s| match file {
            DataFile::Following => s.in_game.following_at = Some(at),
            DataFile::Anglers => {
                s.in_game.anglers_at = Some(at);
                s.in_game.anglers_etag = etag;
            }
            DataFile::Content => {
                s.in_game.content_at = Some(at);
                s.in_game.content_etag = etag;
            }
        });
        if let Err(e) = saved {
            log::error!("couldn't write state.json: {e}");
        }
    }

    /// A `5xx`, a timeout or no connection (CMP-R50, CMP-R84): waits the
    /// queue's backoff (CMP-R51), then the queue takes the download again.
    async fn retry<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        name: &str,
        why: &str,
    ) -> DownloadEnd {
        let wait = lock(&self.backoff).next_delay();
        log::warn!(
            "in-game download {name}: {why}; trying again in {} s",
            wait.as_secs()
        );
        if self.wait(ctx, wait).await {
            DownloadEnd::Retry
        } else {
            log::info!("in-game download {name} stopped");
            DownloadEnd::Interrupted
        }
    }

    /// A refusal (CMP-R50, CMP-R84).
    fn refused<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        file: DataFile,
        refusal: &Refusal,
    ) -> DownloadEnd {
        let name = file.folder();
        // Refusal codes and the backend's messages hold no names (CMP-R112).
        log::warn!(
            "in-game download {name}: refused with {} {}: {}",
            refusal.status,
            refusal.code.as_deref().unwrap_or("(no code)"),
            refusal.message.as_deref().unwrap_or("(no message)")
        );
        if refusal.status == StatusCode::SERVICE_UNAVAILABLE.as_u16()
            && refusal.code.as_deref() == Some(CONTENT_UNAVAILABLE)
        {
            // HEV-R37: no content file yet. The request cost a rate-limit
            // token (HEV-R38), so it waits Retry-After to the second, as a
            // 429 does; the queue doesn't wait.
            self.hold_off(ctx, file, refusal.retry_after);
            return DownloadEnd::Unavailable;
        }
        match on_refusal(refusal) {
            Step::Wait { retry_after } => {
                // CMP-R50: dropped, and not due again before Retry-After has
                // passed (the retry backoff when it is missing); the queue
                // doesn't wait.
                self.hold_off(ctx, file, retry_after);
                DownloadEnd::RateLimited
            }
            Step::Stop(stop) => {
                // CMP-R84: stops it with everything else; the files stay.
                ctx.account.enter_stop(stop);
                DownloadEnd::Interrupted
            }
            Step::Unlinked => DownloadEnd::Interrupted,
            // CMP-R50: for a download, "ends the run" drops it until the
            // timer's next check.
            Step::End { .. } | Step::Restart => DownloadEnd::Failed,
        }
    }
}

impl Uploads {
    /// Drops `file`'s download and holds it off until `retry_after` has
    /// passed, or the retry backoff when it is missing or unreadable
    /// (CMP-R50's `429`, HEV-R37's `503`).
    fn hold_off<N: Notifier, E: Env>(
        &self,
        ctx: &Ctx<'_, N, E>,
        file: DataFile,
        retry_after: Option<Duration>,
    ) {
        let wait = retry_after.unwrap_or_else(|| lock(&self.backoff).next_delay());
        let until = ctx.env.now() + i64::try_from(wait.as_secs()).unwrap_or(i64::MAX / 2);
        lock(&self.schedule).hold(file, until);
        log::info!(
            "in-game download {}: not due again for {} s",
            file.folder(),
            wait.as_secs()
        );
    }
}

fn not_downloaded(name: &str, why: &'static str) -> DownloadEnd {
    log::info!("no in-game download {name}: {why}");
    DownloadEnd::NotStarted(why)
}

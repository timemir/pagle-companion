//! The pure parts of an upload run: what to send given the server's cursor
//! and the local log (CMP-R47 step 2), its pages (steps 3 and 4), whether a
//! run starts at all (CMP-R45's holds and skip), and the queue's retry
//! backoff (CMP-R51).

use std::time::Duration;

use serde::Deserialize;

use crate::pagle_log::LocalLog;
use crate::readings::FileStamp;
use crate::state::{LastRun, RunResult};
use crate::upload_doc::{self, Page};

use super::queue::Props;

/// Page size (CMP Tunables): entries per page, halved while over a cap.
pub const PAGE_SIZE: u64 = 20_000;

/// Retry delay (CMP Tunables): the first wait after a `5xx`, a timeout or no
/// connection, doubling up to the longest.
pub const FIRST_RETRY_DELAY: Duration = Duration::from_secs(30);
pub const LONGEST_RETRY_DELAY: Duration = Duration::from_secs(15 * 60);

/// A Catch log's end on the server (UPL-R23, UPL Contracts): `seq` 0 with no
/// entry ID when the Player holds no such Catch log.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct End {
    pub seq: u64,
    #[serde(default)]
    pub entry_id: Option<String>,
}

/// The cursor endpoint's success body (UPL-R26).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cursor {
    pub end: End,
}

/// What a run does after reading the cursor (CMP-R47 step 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Send pages from `seq from` to `L`. `whole` is a whole send, which
    /// sets the whole-send flag (CMP-R48).
    Send { from: u64, whole: bool },
    /// `E = L`: nothing to send; the run ends up to date without an Upload.
    UpToDate,
    /// The local entry ID at `E` differs from the cursor's (CMP-R52): the run
    /// ends without sending.
    Divergence,
}

/// CMP-R47 step 2 for a local log, the server's end `cursor`, the `logId`'s
/// whole-send flag, and whether the run resends its last entry when up to
/// date (a post-link run, CMP-R101). `log` has at least one entry
/// (CMP-R32).
pub fn plan(log: &LocalLog, cursor: &End, whole_send_flag: bool, resend: bool) -> Plan {
    let local_end = log.len();
    let e = cursor.seq;
    if e == 0 || e > local_end || whole_send_flag {
        return Plan::Send {
            from: 1,
            whole: true,
        };
    }
    if log.entry_id(e) != cursor.entry_id {
        return Plan::Divergence;
    }
    if e < local_end {
        return Plan::Send {
            from: e + 1,
            whole: false,
        };
    }
    if resend {
        // CMP-R101: the last entry again, as one page, so the receiving
        // page sees an Upload. Linking sets no whole-send flag.
        return Plan::Send {
            from: local_end,
            whole: false,
        };
    }
    Plan::UpToDate
}

/// A page that can't be built within the caps: one entry is over a cap
/// (CMP-R47 step 4), and the run ends **too large**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLarge {
    pub seq: u64,
}

/// The page from `seq from` (CMP-R47 steps 3 and 4): the smaller of
/// `page_size` and `L − from + 1` entries, rebuilt with half the count,
/// rounded down, while it is over a cap. A post-link resend (`from = L`)
/// holds one entry (CMP-R101). `from` is within `1 … L`.
pub fn page(log: &LocalLog, from: u64, as_of: u64, page_size: u64) -> Result<Page, TooLarge> {
    let mut count = page_size.min(log.len() + 1 - from).max(1);
    loop {
        let page =
            upload_doc::page(log, from, count, as_of).expect("a page within 1 … L always builds");
        if page.fits_caps() {
            return Ok(page);
        }
        if count == 1 {
            return Err(TooLarge { seq: from });
        }
        count /= 2;
    }
}

/// The watched file as a run read it: what holds and skips compare with the
/// last run (CMP-R45).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileNow<'a> {
    pub file: FileStamp,
    pub format: u64,
    pub companion_version: &'a str,
}

/// Whether the `logId` is **held back** from a run with `props` (CMP-R45),
/// given its last run and the file the run read:
/// - a divergence, until the Player picks **Replace** (CMP-R52);
/// - dev log, until the file's `format` differs from that run's;
/// - failed with a refusal, or too large, until the file changes or the
///   Companion runs another version; a run that failed after a second `gap`
///   isn't held back (CMP-R50);
/// - deleted, from file changes only (CMP-R54).
///
/// **Upload now** and post-link runs ignore holds.
pub fn held_back(last: Option<&LastRun>, now: FileNow, props: Props) -> bool {
    let Some(last) = last else {
        return false;
    };
    if !props.holds {
        return false;
    }
    let same_file = || same_file(last, now.file);
    let same_version = || last.companion_version == now.companion_version;
    match last.result {
        RunResult::Divergence => true,
        RunResult::DevLog => i64::try_from(now.format).ok() == Some(last.format),
        RunResult::Failed => {
            last.code.as_deref() != Some(super::GAP) && same_file() && same_version()
        }
        RunResult::TooLarge => same_file() && same_version(),
        RunResult::Deleted => props.file_change_only,
        RunResult::Uploaded
        | RunResult::UpToDate
        | RunResult::Stale
        | RunResult::WaitingForPagle
        | RunResult::LogCap => false,
    }
}

/// Whether a later scan's run is skipped (CMP-R45): its last run ended
/// uploaded or up to date and the watched file's modification time and size
/// are still that run's.
pub fn skipped(last: Option<&LastRun>, file: FileStamp, props: Props) -> bool {
    props.skip
        && last.is_some_and(|last| {
            matches!(last.result, RunResult::Uploaded | RunResult::UpToDate)
                && same_file(last, file)
        })
}

fn same_file(last: &LastRun, file: FileStamp) -> bool {
    last.file_mtime_ms == file.mtime_ms && last.file_size == file.size
}

/// The queue's retry backoff (CMP-R51): the first retry delay, doubling up
/// to the longest; the queue's next success resets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            next: FIRST_RETRY_DELAY,
        }
    }
}

impl Backoff {
    /// The wait before the next retry, doubling the one after it.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (delay * 2).min(LONGEST_RETRY_DELAY);
        delay
    }

    /// A success: the next retry waits the first retry delay again.
    pub fn reset(&mut self) {
        self.next = FIRST_RETRY_DELAY;
    }
}

/// A relative time as the window and the tray show it, like "5 minutes ago"
/// (CMP UI).
pub fn relative_time(now: i64, at: i64) -> String {
    let seconds = (now - at).max(0);
    let (n, unit) = match seconds {
        0..60 => return "just now".into(),
        60..3_600 => (seconds / 60, "minute"),
        3_600..86_400 => (seconds / 3_600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    if n == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{n} {unit}s ago")
    }
}

#[cfg(test)]
mod tests {
    use super::super::queue::Trigger;
    use super::*;
    use crate::pagle_log::{self, Found};
    use crate::readings::fixture::{LOG_A, pagle_lua_text};

    fn local(entries: u64) -> LocalLog {
        match pagle_log::parse(pagle_lua_text(LOG_A, entries).as_bytes()).unwrap() {
            Found::Valid(log) => log,
            other => panic!("not valid: {other:?}"),
        }
    }

    fn end(log: &LocalLog, seq: u64) -> End {
        End {
            seq,
            entry_id: log.entry_id(seq),
        }
    }

    #[test]
    fn an_empty_cursor_is_a_whole_send() {
        // CMP-R47 step 2: f = 1 when E = 0
        let log = local(15);
        let cursor = End {
            seq: 0,
            entry_id: None,
        };
        assert_eq!(
            plan(&log, &cursor, false, false),
            Plan::Send {
                from: 1,
                whole: true
            }
        );
    }

    #[test]
    fn a_server_ahead_of_the_disk_is_a_whole_send() {
        // CMP-R47 step 2: f = 1 when E > L (a restored backup, ADR 0001)
        let log = local(15);
        let cursor = End {
            seq: 20,
            entry_id: Some("Player-1-F-1-0".into()),
        };
        assert_eq!(
            plan(&log, &cursor, false, false),
            Plan::Send {
                from: 1,
                whole: true
            }
        );
    }

    #[test]
    fn the_whole_send_flag_is_a_whole_send() {
        // CMP-R47 step 2, CMP-R48: f = 1 when the whole-send flag is set
        let log = local(15);
        assert_eq!(
            plan(&log, &end(&log, 12), true, false),
            Plan::Send {
                from: 1,
                whole: true
            }
        );
    }

    #[test]
    fn a_matching_cursor_appends_from_the_next_entry() {
        // CMP-R47 step 2: otherwise f = E + 1
        let log = local(15);
        assert_eq!(
            plan(&log, &end(&log, 12), false, false),
            Plan::Send {
                from: 13,
                whole: false
            }
        );
    }

    #[test]
    fn a_different_entry_id_at_the_cursor_is_a_divergence() {
        // CMP-R47 step 2: the local entry ID at E differs from the cursor's
        let log = local(15);
        let cursor = End {
            seq: 12,
            entry_id: Some("Player-4618-0A1B2C3D-1-0".into()),
        };
        assert_eq!(plan(&log, &cursor, false, false), Plan::Divergence);
        // Even when E = L, and even right after linking.
        let cursor = End {
            seq: 15,
            entry_id: None,
        };
        assert_eq!(plan(&log, &cursor, false, true), Plan::Divergence);
    }

    #[test]
    fn a_cursor_at_the_last_entry_is_up_to_date() {
        // CMP-R47 step 2: when E = L the run ends up to date without an
        // Upload
        let log = local(15);
        assert_eq!(plan(&log, &end(&log, 15), false, false), Plan::UpToDate);
    }

    #[test]
    fn right_after_linking_up_to_date_resends_the_last_entry() {
        // CMP-R101: a post-link run that would end up to date sends its
        // last entry again as one page (from = L); linking sets no
        // whole-send flag
        let log = local(15);
        assert_eq!(
            plan(&log, &end(&log, 15), false, true),
            Plan::Send {
                from: 15,
                whole: false
            }
        );
        let page = page(&log, 15, 1_791_735_090, 1).unwrap();
        assert_eq!((page.from, page.count), (15, 1));
    }

    #[test]
    fn pages_hold_at_most_the_page_size() {
        // CMP-R47 step 3: count = the smaller of the page size and L − f + 1
        let log = local(45);
        let first = page(&log, 1, 1, 20).unwrap();
        assert_eq!((first.from, first.count), (1, 20));
        let last = page(&log, 41, 1, 20).unwrap();
        assert_eq!((last.from, last.count), (41, 5));
        assert_eq!(PAGE_SIZE, 20_000);
    }

    /// A local log whose entries carry `chars` characters that hardly
    /// compress each.
    fn incompressible(entries: u64, chars: usize) -> LocalLog {
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let alphabet = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let mut text = String::from(
            "PagleLog = { format = 1, logId = \"3f9a2c1e7b0d4a65\", characters = { { guid = \
             \"Player-4618-0A1B2C3D\", name = \"Ada\", classID = 1, raceID = 1, faction = \
             \"Alliance\", realmID = 4618, firstSeen = 1, lastSeen = 2 } }, entries = { ",
        );
        for i in 0..entries {
            text.push_str(&format!("\"S1|1|{}|0|", 1_791_735_050 + i));
            for _ in 0..chars {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                text.push(alphabet[(seed % 62) as usize] as char);
            }
            text.push_str("\", ");
        }
        text.push_str("} }");
        match pagle_log::parse(text.as_bytes()).unwrap() {
            Found::Valid(log) => log,
            other => panic!("not valid: {other:?}"),
        }
    }

    #[test]
    fn a_page_over_the_compressed_cap_is_rebuilt_at_half_the_count() {
        // CMP-R47 step 4: a page whose compressed body is over 2 MiB is
        // rebuilt with half the count, until it fits; the next page starts
        // again from the page size
        let log = incompressible(20_000, 150);
        let full = upload_doc::page(&log, 1, 20_000, 1).unwrap();
        assert!(full.body.len() > upload_doc::COMPRESSED_CAP);
        assert!(full.document.len() <= upload_doc::DECOMPRESSED_CAP);

        let first = page(&log, 1, 1, PAGE_SIZE).unwrap();
        assert_eq!((first.from, first.count), (1, 10_000));
        assert!(first.fits_caps());
        let next = page(&log, 10_001, 1, PAGE_SIZE).unwrap();
        assert_eq!((next.from, next.count), (10_001, 10_000));
    }

    #[test]
    fn one_entry_over_a_cap_is_too_large() {
        // CMP-R47 step 4: a single entry over a cap ends the run as too
        // large
        let log = incompressible(2, 3 * 1024 * 1024);
        assert_eq!(page(&log, 1, 1, PAGE_SIZE), Err(TooLarge { seq: 1 }));
    }

    fn last(result: RunResult) -> LastRun {
        LastRun {
            result,
            at: 1_791_735_100,
            file_mtime_ms: 1_791_735_090_123,
            file_size: 812_345,
            format: 1,
            companion_version: "1.0.0".into(),
            code: None,
            message: None,
            catch_log: None,
        }
    }

    const SAME: FileStamp = FileStamp {
        mtime_ms: 1_791_735_090_123,
        size: 812_345,
    };
    const CHANGED: FileStamp = FileStamp {
        mtime_ms: 1_791_735_190_000,
        size: 812_400,
    };

    fn now(file: FileStamp) -> FileNow<'static> {
        FileNow {
            file,
            format: 1,
            companion_version: "1.0.0",
        }
    }

    #[test]
    fn a_divergence_holds_until_replace_but_not_for_upload_now_or_linking() {
        // CMP-R45: held back after a divergence until Replace; Upload now
        // and linking run it held back or not (CMP-R52)
        let run = last(RunResult::Divergence);
        for trigger in [
            Trigger::Change,
            Trigger::Start,
            Trigger::Scan,
            Trigger::SwitchOn,
        ] {
            assert!(held_back(Some(&run), now(CHANGED), trigger.props()));
        }
        assert!(!held_back(
            Some(&run),
            now(SAME),
            Trigger::UploadNow.props()
        ));
        assert!(!held_back(Some(&run), now(SAME), Trigger::PostLink.props()));
    }

    #[test]
    fn a_dev_log_holds_until_the_format_changes() {
        // CMP-R45: dev log, until the watched file's format differs from the
        // one recorded with that run
        let run = last(RunResult::DevLog);
        let props = Trigger::Change.props();
        assert!(held_back(Some(&run), now(CHANGED), props));
        let newer = FileNow {
            format: 2,
            ..now(CHANGED)
        };
        assert!(!held_back(Some(&run), newer, props));
    }

    #[test]
    fn failed_and_too_large_hold_until_the_file_or_the_version_changes() {
        // CMP-R45: failed with a refusal, or too large, until its watched
        // file changes or the Companion runs a different version
        for result in [RunResult::Failed, RunResult::TooLarge] {
            let mut run = last(result);
            run.code = (result == RunResult::Failed).then(|| "entry_cap".into());
            let props = Trigger::Start.props();
            assert!(held_back(Some(&run), now(SAME), props));
            assert!(!held_back(Some(&run), now(CHANGED), props));
            let updated = FileNow {
                companion_version: "1.0.1",
                ..now(SAME)
            };
            assert!(!held_back(Some(&run), updated, props));
        }
    }

    #[test]
    fn failed_after_a_second_gap_is_not_held_back() {
        // CMP-R50: a second gap ends the run failed, not held back
        let mut run = last(RunResult::Failed);
        run.code = Some("gap".into());
        assert!(!held_back(Some(&run), now(SAME), Trigger::Start.props()));
    }

    #[test]
    fn deleted_is_held_back_from_file_changes_only() {
        // CMP-R45, CMP-R54: start, scans, Upload now and linking still try
        let run = last(RunResult::Deleted);
        assert!(held_back(Some(&run), now(CHANGED), Trigger::Change.props()));
        for trigger in [
            Trigger::Start,
            Trigger::Scan,
            Trigger::UploadNow,
            Trigger::PostLink,
        ] {
            assert!(!held_back(Some(&run), now(CHANGED), trigger.props()));
        }
        // A change merged with a start into one queued run tries too.
        let merged = Trigger::Change.props().merge(Trigger::Start.props());
        assert!(!held_back(Some(&run), now(CHANGED), merged));
    }

    #[test]
    fn other_results_and_no_run_hold_nothing() {
        // CMP-R45: only the results it names hold a logId back
        assert!(!held_back(None, now(SAME), Trigger::Change.props()));
        for result in [
            RunResult::Uploaded,
            RunResult::UpToDate,
            RunResult::Stale,
            RunResult::WaitingForPagle,
            RunResult::LogCap,
        ] {
            assert!(!held_back(
                Some(&last(result)),
                now(SAME),
                Trigger::Change.props()
            ));
        }
    }

    #[test]
    fn a_later_scan_skips_an_unchanged_file_after_uploaded_or_up_to_date() {
        // CMP-R45: after each later scan, except one whose last run ended
        // uploaded or up to date and whose watched file's modification time
        // and size are still those of that run
        let scan = Trigger::Scan.props();
        for result in [RunResult::Uploaded, RunResult::UpToDate] {
            assert!(skipped(Some(&last(result)), SAME, scan));
            assert!(!skipped(Some(&last(result)), CHANGED, scan));
        }
        assert!(!skipped(Some(&last(RunResult::Stale)), SAME, scan));
        assert!(!skipped(None, SAME, scan));
        // Only scans skip (CMP-R46: Upload now has no skip).
        let merged = scan.merge(Trigger::UploadNow.props());
        assert!(!skipped(Some(&last(RunResult::Uploaded)), SAME, merged));
        assert!(!skipped(
            Some(&last(RunResult::Uploaded)),
            SAME,
            Trigger::Start.props()
        ));
    }

    #[test]
    fn the_backoff_doubles_from_30_seconds_to_15_minutes_and_resets() {
        // CMP-R51, CMP Tunables: retry delay 30 s first, doubling, at most
        // 15 min; the queue's next success resets it
        let mut backoff = Backoff::default();
        let waits: Vec<u64> = (0..7).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(waits, [30, 60, 120, 240, 480, 900, 900]);
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_secs(30));
    }

    #[test]
    fn relative_times_read_like_5_minutes_ago() {
        // CMP UI: relative times read like "5 minutes ago"
        let at = 1_791_735_100;
        assert_eq!(relative_time(at + 5, at), "just now");
        assert_eq!(relative_time(at + 60, at), "1 minute ago");
        assert_eq!(relative_time(at + 5 * 60 + 59, at), "5 minutes ago");
        assert_eq!(relative_time(at + 3_600, at), "1 hour ago");
        assert_eq!(relative_time(at + 7_300, at), "2 hours ago");
        assert_eq!(relative_time(at + 86_400 * 3, at), "3 days ago");
        // A clock set back never reads as the future.
        assert_eq!(relative_time(at - 100, at), "just now");
    }
}

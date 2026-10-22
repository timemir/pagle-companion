//! The in-game download timer (CMP-R80, CMP-R81; IGV-R50, IGV-R51; HEV
//! Tunables): when the Following, Anglers and Content downloads are due.
//!
//! The [`Schedule`] is what the timer keeps in memory between its checks:
//! whether each download is owed (at start, right after linking, after one
//! that wasn't successful) and whether it is held off until some time (a body
//! that failed CMP-R82 or couldn't be written anywhere, a `429`). The times of
//! the last successful downloads are local state's `inGame` (CMP Data), so
//! [`Schedule::due`] reads them from there. Whether the Companion may download
//! at all (linked, outside a stop) is the caller's; the queue and the download
//! itself are `upload`'s, and the app's timer is in [`window`].

pub mod window;

use std::time::Duration;

use super::folders::{DataFile, Qualifying};
use crate::state::InGame;

/// The in-game check interval (CMP Tunables): the timer is checked at start
/// and this often.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The following interval (CMP Tunables, IGV-R50), in seconds: the Following
/// download is due this long after its last success, and a download whose
/// body failed CMP-R82 or was written nowhere waits this long.
pub const FOLLOWING_INTERVAL: i64 = 60 * 60;

/// The Anglers re-fetch age (CMP Tunables, IGV-R51), in seconds: the Anglers
/// download is due once its last success (`200` or `304`) is this old.
pub const ANGLERS_REFETCH_AGE: i64 = 12 * 60 * 60;

/// The content poll interval (CMP Tunables, HEV Tunables), in seconds: the
/// Content download is due this long after its last success (`200` or
/// `304`). The backend builds a new file every content interval (HEV-R30), so
/// a running Companion's `Content.lua` is never near the addon's not-running
/// age (PUSH-R15) or its `expires` (HEV-R33).
pub const CONTENT_POLL_INTERVAL: i64 = 60 * 60;

/// One download's part of the schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    /// Due at the next check whatever its last success: at start, right
    /// after linking, and after a download that wasn't successful.
    owed: bool,
    /// Not due before this Unix second, whatever else says it is.
    not_before: Option<i64>,
}

impl Default for Slot {
    /// A new process owes every download: "at start" (CMP-R81).
    fn default() -> Self {
        Self {
            owed: true,
            not_before: None,
        }
    }
}

/// When the three downloads are due (CMP-R81). A new one owes them all, as
/// at start.
#[derive(Debug, Default)]
pub struct Schedule {
    following: Slot,
    anglers: Slot,
    content: Slot,
}

impl Schedule {
    fn slot(&mut self, file: DataFile) -> &mut Slot {
        match file {
            DataFile::Following => &mut self.following,
            DataFile::Anglers => &mut self.anglers,
            DataFile::Content => &mut self.content,
        }
    }

    fn get(&self, file: DataFile) -> Slot {
        match file {
            DataFile::Following => self.following,
            DataFile::Anglers => self.anglers,
            DataFile::Content => self.content,
        }
    }

    /// Right after linking (CMP-R101): every download is owed again, and
    /// what held them off belonged to the old link.
    pub fn owe_all(&mut self) {
        *self = Self::default();
    }

    /// The downloads due at `now` (CMP-R80, CMP-R81), Following, Anglers,
    /// then Content:
    /// - none while no install qualifies;
    /// - none held off until after `now`;
    /// - an owed one;
    /// - one a qualifying install lacks the data file of;
    /// - Following once its last success is the following interval old,
    ///   Anglers once its last success is the Anglers re-fetch age old,
    ///   Content once its last success is the content poll interval old, or
    ///   any without one (or with one in the future, as after the clock was
    ///   set back).
    pub fn due(&self, now: i64, in_game: &InGame, qualifying: &Qualifying) -> Vec<DataFile> {
        if !qualifying.any() {
            return Vec::new();
        }
        DataFile::ALL
            .into_iter()
            .filter(|&file| {
                let slot = self.get(file);
                if slot.not_before.is_some_and(|t| now < t) {
                    return false;
                }
                slot.owed || !qualifying.lacking(file).is_empty() || aged(file, in_game, now)
            })
            .collect()
    }

    /// A successful download (CMP-R81): due again by its age.
    pub fn succeeded(&mut self, file: DataFile) {
        *self.slot(file) = Slot {
            owed: false,
            not_before: None,
        };
    }

    /// A download that wasn't successful: due again at the next check
    /// (CMP-R81).
    pub fn failed(&mut self, file: DataFile) {
        self.slot(file).owed = true;
    }

    /// A download that wasn't successful and isn't tried again before
    /// `until` (CMP-R50's `429`, HEV-R37's `503 content_unavailable`,
    /// CMP-R81's bad or unwritable body).
    pub fn hold(&mut self, file: DataFile, until: i64) {
        *self.slot(file) = Slot {
            owed: true,
            not_before: Some(until),
        };
    }
}

/// Whether `file`'s last success is its interval old (CMP-R81).
fn aged(file: DataFile, in_game: &InGame, now: i64) -> bool {
    let (last, interval) = match file {
        DataFile::Following => (in_game.following_at, FOLLOWING_INTERVAL),
        DataFile::Anglers => (in_game.anglers_at, ANGLERS_REFETCH_AGE),
        DataFile::Content => (in_game.content_at, CONTENT_POLL_INTERVAL),
    };
    last.is_none_or(|at| at > now || now - at >= interval)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_791_800_000;

    fn one_install() -> Qualifying {
        Qualifying {
            installs: vec!["C:/WoW/_classic_".into()],
            ..Qualifying::default()
        }
    }

    fn lacking(file: DataFile) -> Qualifying {
        let mut qualifying = one_install();
        match file {
            DataFile::Following => qualifying.lacking_following = qualifying.installs.clone(),
            DataFile::Anglers => qualifying.lacking_anglers = qualifying.installs.clone(),
            DataFile::Content => qualifying.lacking_content = qualifying.installs.clone(),
        }
        qualifying
    }

    /// Following and Anglers last done at the times given, Content a minute
    /// ago.
    fn done_at(following: i64, anglers: i64) -> InGame {
        InGame {
            following_at: Some(following),
            anglers_at: Some(anglers),
            anglers_etag: Some("\"ab\"".into()),
            content_at: Some(NOW - 60),
            content_etag: Some("\"cd\"".into()),
        }
    }

    /// Following and Anglers a minute ago, Content at `content`.
    fn content_done_at(content: i64) -> InGame {
        InGame {
            content_at: Some(content),
            ..done_at(NOW - 60, NOW - 60)
        }
    }

    /// A schedule whose start downloads all succeeded.
    fn settled() -> Schedule {
        let mut schedule = Schedule::default();
        for file in DataFile::ALL {
            schedule.succeeded(file);
        }
        schedule
    }

    #[test]
    fn every_download_is_due_at_start_whatever_its_last_success() {
        // CMP-R81: the Following, Anglers and Content downloads at start
        let schedule = Schedule::default();
        assert_eq!(
            schedule.due(NOW, &done_at(NOW - 60, NOW - 60), &one_install()),
            DataFile::ALL
        );
    }

    #[test]
    fn content_is_due_every_content_poll_interval() {
        // CMP-R81, HEV Tunables: the Content download every content poll
        // interval (1 h) after its last success, 200 or 304 alike
        let schedule = settled();
        let at = NOW - CONTENT_POLL_INTERVAL;
        assert_eq!(
            schedule.due(NOW - 1, &content_done_at(at), &one_install()),
            []
        );
        assert_eq!(
            schedule.due(NOW, &content_done_at(at), &one_install()),
            [DataFile::Content]
        );
        assert_eq!(CONTENT_POLL_INTERVAL, 3_600);
    }

    #[test]
    fn content_is_held_off_until_retry_after_and_then_due() {
        // HEV-R37: 503 content_unavailable with Retry-After holds the Content
        // download off as a 429 does (CMP-R50), whatever else would make it
        // due
        let mut schedule = settled();
        schedule.hold(DataFile::Content, NOW + 60);
        let qualifying = lacking(DataFile::Content);
        assert_eq!(
            schedule.due(NOW + 59, &InGame::default(), &qualifying),
            [DataFile::Following, DataFile::Anglers]
        );
        assert_eq!(
            schedule.due(NOW + 60, &content_done_at(NOW + 59), &qualifying),
            [DataFile::Content]
        );
    }

    #[test]
    fn nothing_is_due_while_no_install_qualifies() {
        // CMP-R80: while no install qualifies, the Companion makes no in-game
        // download, owed or not
        let schedule = Schedule::default();
        assert!(
            schedule
                .due(NOW, &InGame::default(), &Qualifying::default())
                .is_empty()
        );
    }

    #[test]
    fn following_is_due_every_following_interval() {
        // CMP-R81, IGV-R50: the Following download every following interval
        // (1 h) after its last success
        let schedule = settled();
        let at = NOW - FOLLOWING_INTERVAL;
        assert_eq!(
            schedule.due(NOW - 1, &done_at(at, NOW - 60), &one_install()),
            []
        );
        assert_eq!(
            schedule.due(NOW, &done_at(at, NOW - 60), &one_install()),
            [DataFile::Following]
        );
        assert_eq!(FOLLOWING_INTERVAL, 3_600);
    }

    #[test]
    fn anglers_is_due_once_its_last_success_is_the_refetch_age_old() {
        // CMP-R81, IGV-R51: the Anglers download whenever its last
        // successful download is the Anglers re-fetch age (12 h) old
        let schedule = settled();
        let at = NOW - ANGLERS_REFETCH_AGE;
        assert_eq!(
            schedule.due(NOW - 1, &done_at(NOW - 1, at), &one_install()),
            []
        );
        assert_eq!(
            schedule.due(NOW, &done_at(NOW, at), &one_install()),
            [DataFile::Anglers]
        );
        assert_eq!(ANGLERS_REFETCH_AGE, 12 * 3_600);
    }

    #[test]
    fn a_download_never_successful_is_due() {
        // CMP-R81: no last success to be young
        let schedule = settled();
        assert_eq!(
            schedule.due(NOW, &InGame::default(), &one_install()),
            DataFile::ALL
        );
    }

    #[test]
    fn an_install_lacking_a_data_file_makes_that_download_due_at_once() {
        // CMP-R80: an install that qualifies but lacks a data file makes that
        // download due at once
        let schedule = settled();
        let recent = done_at(NOW - 60, NOW - 60);
        assert_eq!(
            schedule.due(NOW, &recent, &lacking(DataFile::Anglers)),
            [DataFile::Anglers]
        );
        assert_eq!(
            schedule.due(NOW, &recent, &lacking(DataFile::Following)),
            [DataFile::Following]
        );
        assert_eq!(
            schedule.due(NOW, &recent, &lacking(DataFile::Content)),
            [DataFile::Content]
        );
    }

    #[test]
    fn a_download_that_failed_is_due_at_the_next_check() {
        // CMP-R81: a download that isn't successful is due again at the
        // timer's next check, even when its last success is recent
        let mut schedule = settled();
        schedule.failed(DataFile::Following);
        assert_eq!(
            schedule.due(NOW, &done_at(NOW - 60, NOW - 60), &one_install()),
            [DataFile::Following]
        );
        let mut schedule = settled();
        schedule.failed(DataFile::Content);
        assert_eq!(
            schedule.due(NOW, &done_at(NOW - 60, NOW - 60), &one_install()),
            [DataFile::Content]
        );
    }

    #[test]
    fn a_held_download_waits_until_its_hold_passes() {
        // CMP-R81, CMP-R82: a body that fails its check or can't be written
        // anywhere isn't tried again before one following interval has
        // passed, even for an install lacking its file; CMP-R50: a 429'd
        // download isn't due before Retry-After has passed
        let mut schedule = settled();
        schedule.hold(DataFile::Anglers, NOW + FOLLOWING_INTERVAL);
        let qualifying = lacking(DataFile::Anglers);
        assert_eq!(
            schedule.due(NOW, &InGame::default(), &qualifying),
            [DataFile::Following, DataFile::Content]
        );
        assert_eq!(
            schedule.due(
                NOW + FOLLOWING_INTERVAL - 1,
                &InGame::default(),
                &qualifying
            ),
            [DataFile::Following, DataFile::Content]
        );
        assert_eq!(
            schedule.due(NOW + FOLLOWING_INTERVAL, &InGame::default(), &qualifying),
            DataFile::ALL
        );
    }

    #[test]
    fn a_success_clears_what_was_owed() {
        // CMP-R81: once successful, the download is due by its age again
        let mut schedule = Schedule::default();
        schedule.succeeded(DataFile::Following);
        assert_eq!(
            schedule.due(NOW, &done_at(NOW, NOW - 60), &one_install()),
            [DataFile::Anglers, DataFile::Content]
        );
        schedule.succeeded(DataFile::Content);
        assert_eq!(
            schedule.due(NOW, &done_at(NOW, NOW - 60), &one_install()),
            [DataFile::Anglers]
        );
    }

    #[test]
    fn linking_owes_every_download_and_forgets_the_holds() {
        // CMP-R101: right after any link, the in-game downloads (CMP-R81:
        // Content too, "right after linking")
        let mut schedule = settled();
        schedule.hold(DataFile::Following, NOW + FOLLOWING_INTERVAL);
        schedule.hold(DataFile::Content, NOW + 60);
        schedule.owe_all();
        assert_eq!(
            schedule.due(NOW, &done_at(NOW, NOW), &one_install()),
            DataFile::ALL
        );
    }

    #[test]
    fn a_last_success_in_the_future_counts_as_old() {
        // CMP-R81: a clock set back can't keep a download from being due for
        // as long as it went back
        let schedule = settled();
        let future = InGame {
            content_at: Some(NOW + 60),
            ..done_at(NOW + 3_600, NOW + 60)
        };
        assert_eq!(schedule.due(NOW, &future, &one_install()), DataFile::ALL);
    }

    #[test]
    fn the_check_interval_is_5_minutes() {
        // CMP Tunables: in-game check interval 5 min
        assert_eq!(CHECK_INTERVAL, Duration::from_secs(300));
    }
}

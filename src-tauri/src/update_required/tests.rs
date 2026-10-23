//! The decisions of update required against a recording [`Env`]: no
//! backend, no update server, no clock but the times passed in.

use std::cell::{Cell, RefCell};
use std::time::{Duration, SystemTime};

use semver::Version;

use super::*;
use crate::updater::Checked;

const MINUTE: Duration = Duration::from_secs(60);

fn t0() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Read,
    Kept(String),
    Entered,
    Left,
    Shown(Text),
    CheckSoon,
    StopLinking,
    Install,
}

/// A Companion whose backend answers `config`.
struct Fake {
    enforcer: Enforcer,
    config: RefCell<Option<Config>>,
    required: Cell<bool>,
    waiting: Cell<bool>,
    failed_verifications: Cell<u32>,
    /// Whether the install at once starts the new version.
    installs: Cell<bool>,
    events: RefCell<Vec<Event>>,
}

impl Fake {
    fn running(version: &str) -> Self {
        Self {
            enforcer: Enforcer::new(version),
            config: RefCell::new(None),
            required: Cell::new(false),
            waiting: Cell::new(false),
            failed_verifications: Cell::new(0),
            installs: Cell::new(true),
            events: RefCell::default(),
        }
    }

    fn new() -> Self {
        Self::running("1.0.0")
    }

    /// The backend answers this minimum and the beta's uid.
    fn answers(&self, min: &str) {
        *self.config.borrow_mut() = Some(config(min, "wow_classic_beta"));
    }

    fn unreachable(&self) {
        *self.config.borrow_mut() = None;
    }

    fn after_check(&self, checked: Checked, now: SystemTime) {
        self.enforcer.after_check(self, &checked, now);
    }

    fn tick(&self, now: SystemTime) {
        self.enforcer.tick(self, now);
    }

    /// The events since the last call.
    fn take(&self) -> Vec<Event> {
        self.events.take()
    }

    fn push(&self, event: Event) {
        self.events.borrow_mut().push(event);
    }
}

impl Env for Fake {
    fn read_config(&self) -> Option<Config> {
        self.push(Event::Read);
        self.config.borrow().clone()
    }

    fn keep_config(&self, config: &Config) {
        self.push(Event::Kept(config.forever_product_uid.clone()));
    }

    fn update_required(&self) -> bool {
        self.required.get()
    }

    fn enter(&self) {
        self.required.set(true);
        self.push(Event::Entered);
        // As the link's hook does.
        self.enforcer.entered(self);
    }

    fn leave(&self) {
        self.required.set(false);
        self.push(Event::Left);
    }

    fn update_waiting(&self) -> bool {
        self.waiting.get()
    }

    fn failed_verifications(&self) -> u32 {
        self.failed_verifications.get()
    }

    fn show(&self, text: Text) {
        self.push(Event::Shown(text));
    }

    fn check_soon(&self) {
        self.push(Event::CheckSoon);
    }

    fn stop_linking(&self) {
        self.push(Event::StopLinking);
    }

    fn install_now(&self) -> bool {
        self.push(Event::Install);
        self.installs.get()
    }
}

fn config(min: &str, uid: &str) -> Config {
    Config {
        min_companion_version: Version::parse(min).unwrap(),
        recommended_companion_version: Version::new(0, 0, 0),
        forever_product_uid: uid.into(),
    }
}

#[test]
fn a_config_read_follows_every_check() {
    // DST-R51: every update check is followed by a config read, a failed
    // check too; CMP-R12: its Forever product uid is kept
    let fake = Fake::new();
    fake.answers("0.0.0");
    fake.after_check(Checked::UpToDate, t0());
    fake.after_check(Checked::Failed, t0() + 6 * 60 * MINUTE);
    assert_eq!(
        fake.take(),
        [
            Event::Read,
            Event::Kept("wow_classic_beta".into()),
            Event::Read,
            Event::Kept("wow_classic_beta".into()),
        ]
    );
}

#[test]
fn a_failed_read_changes_nothing() {
    // DST-R99: config reads fail and retry at the next check or required-state
    // interval; the Companion keeps its update-required state as it was
    let fake = Fake::new();
    fake.unreachable();
    fake.after_check(Checked::UpToDate, t0());
    assert_eq!(fake.take(), [Event::Read]);
    assert!(!fake.required.get());

    fake.required.set(true);
    fake.after_check(Checked::Failed, t0() + MINUTE);
    assert_eq!(fake.take(), [Event::Read]);
    assert!(fake.required.get());
}

#[test]
fn a_minimum_above_the_running_version_enters_update_required() {
    // DST-R56: entered when a config read shows minCompanionVersion above
    // its own version; #18: the check that read followed counts as the
    // update check at once, so none is forced; linking stops
    let fake = Fake::new();
    fake.answers("9.0.0");
    fake.after_check(Checked::Failed, t0());
    assert_eq!(
        fake.take(),
        [
            Event::Read,
            Event::Kept("wow_classic_beta".into()),
            Event::Entered,
            Event::StopLinking,
        ]
    );
    assert!(fake.required.get());
}

#[test]
fn below_and_not_below_go_by_semver_precedence() {
    // DST-R61: below by SemVer 2.0.0 precedence, build metadata ignored, so
    // 1.2.0-rc.1 is below 1.2.0
    let below = |running: &str, min: &str| {
        let fake = Fake::running(running);
        fake.answers(min);
        fake.after_check(Checked::Failed, t0());
        fake.required.get()
    };
    assert!(below("1.2.0-rc.1", "1.2.0"));
    assert!(below("0.0.1-test.7", "0.1.0"));
    assert!(!below("1.2.0+build.5", "1.2.0"));
    assert!(!below("1.2.0", "1.2.0"));
    assert!(!below("1.3.0", "1.2.0"));
}

#[test]
fn a_refusal_runs_an_update_check_at_once() {
    // DST-R56: entered by a request refused update_required, it runs an
    // update check at once; #18: device-flow polling under way stops
    let fake = Fake::new();
    fake.enforcer.entered(&fake);
    assert_eq!(fake.take(), [Event::StopLinking, Event::CheckSoon]);
}

#[test]
fn config_is_read_every_fifteen_minutes_while_update_required() {
    // DST-R56: it reads config every required-state config interval, besides
    // the reads after update checks; #18: driven by the 1-min update tick, by
    // wall clock
    let fake = Fake::new();
    fake.answers("9.0.0");
    fake.after_check(Checked::Failed, t0());
    fake.take();

    let mut now = t0();
    let mut reads = Vec::new();
    while now <= t0() + 31 * MINUTE {
        now += MINUTE;
        fake.tick(now);
        if fake.take().contains(&Event::Read) {
            reads.push(now);
        }
    }
    assert_eq!(reads, [t0() + 15 * MINUTE, t0() + 30 * MINUTE]);

    // A tick after sleep reads at once.
    fake.tick(t0() + 120 * MINUTE);
    assert!(fake.take().contains(&Event::Read));
}

#[test]
fn outside_update_required_ticks_read_nothing() {
    // DST-R51: outside update required, config is read only after checks
    let fake = Fake::new();
    fake.answers("0.0.0");
    fake.after_check(Checked::UpToDate, t0());
    fake.take();
    for minute in 1..=60 {
        fake.tick(t0() + minute * MINUTE);
    }
    assert!(fake.take().is_empty());
}

#[test]
fn a_read_below_the_minimum_no_more_leaves_the_state() {
    // DST-R56: it leaves the state when a config read shows its version is
    // no longer below the minimum, a required-state read too
    let fake = Fake::new();
    fake.answers("9.0.0");
    fake.after_check(Checked::Failed, t0());
    fake.answers("0.0.0");
    fake.take();
    fake.tick(t0() + 15 * MINUTE);
    assert_eq!(
        fake.take(),
        [
            Event::Read,
            Event::Kept("wow_classic_beta".into()),
            Event::Left
        ]
    );
    assert!(!fake.required.get());

    // And a refusal's state, by the read after a check.
    fake.required.set(true);
    fake.after_check(Checked::UpToDate, t0() + 16 * MINUTE);
    assert!(fake.take().contains(&Event::Left));
}

#[test]
fn an_update_waiting_picks_the_restart_text() {
    // DST-R57: an update is waiting: "…Choose Restart to update in its tray
    // menu."
    assert_eq!(
        pick(&Checked::Waiting(Version::new(2, 0, 0)), true, 0),
        Some(Text::RestartToUpdate)
    );
    // Waiting wins, whatever this check found.
    assert_eq!(pick(&Checked::Failed, true, 3), Some(Text::RestartToUpdate));
}

#[test]
fn no_higher_version_or_failed_verifications_pick_the_download_text() {
    // DST-R57: a successful check finds no version above its own, the
    // download has failed verification 3 times in a row, or DST-R54's Linux
    // case applies: "…Download the latest version from pagle.app/download."
    assert_eq!(pick(&Checked::UpToDate, false, 0), Some(Text::Download));
    assert_eq!(
        pick(&Checked::NotVerified { in_a_row: 3 }, false, 3),
        Some(Text::Download)
    );
    assert_eq!(pick(&Checked::NotVerified { in_a_row: 2 }, false, 2), None);
    assert_eq!(pick(&Checked::CantDownload, false, 0), Some(Text::Download));
    // The count is updater.json's, so it holds across a download that
    // didn't complete.
    assert_eq!(
        pick(&Checked::NotDownloaded, false, 3),
        Some(Text::Download)
    );
    assert_eq!(pick(&Checked::NotDownloaded, false, 1), None);
}

#[test]
fn while_checks_fail_outright_no_text_is_picked() {
    // DST-R57: while update checks fail outright (offline, GitHub down), it
    // waits
    assert_eq!(pick(&Checked::Failed, false, 0), None);
    assert_eq!(pick(&Checked::Failed, false, 3), None);
}

#[test]
fn the_texts_are_dst_r57s() {
    // DST-R57, DST UI: the update-required notification's two texts
    assert_eq!(
        Text::RestartToUpdate.text(),
        "Pagle Companion needs an update to keep uploading. Choose Restart to update in its tray menu."
    );
    assert_eq!(
        Text::Download.text(),
        "Pagle Companion needs an update to keep uploading. Download the latest version from pagle.app/download."
    );
}

#[test]
fn a_text_is_shown_after_each_check_only_while_update_required() {
    // DST-R57: shown after update required is entered, as soon as one of its
    // conditions holds; CMP UI, Status: the text whose condition holds now
    let fake = Fake::new();
    fake.answers("0.0.0");
    fake.after_check(Checked::UpToDate, t0());
    assert!(!fake.take().iter().any(|e| matches!(e, Event::Shown(_))));

    fake.answers("9.0.0");
    fake.after_check(Checked::UpToDate, t0() + MINUTE);
    assert_eq!(fake.take().last(), Some(&Event::Shown(Text::Download)));

    // It moves to the first text once an update is waiting that the
    // install at once couldn't start (DST-R56).
    fake.waiting.set(true);
    fake.installs.set(false);
    fake.after_check(Checked::Waiting(Version::new(9, 0, 0)), t0() + 2 * MINUTE);
    let events = fake.take();
    assert!(events.contains(&Event::Install));
    assert_eq!(events.last(), Some(&Event::Shown(Text::RestartToUpdate)));
}

#[test]
fn a_waiting_update_installs_at_once_while_update_required() {
    // DST-R56: while update required, a waiting update installs at once,
    // whatever the setting or idle state (Enforcer asks neither); headless,
    // that is DST-R58's exit with status 0. DST-R59: with something it can
    // install, DST-R57's text isn't shown
    let fake = Fake::new();
    fake.answers("9.0.0");
    fake.waiting.set(true);
    fake.after_check(Checked::Waiting(Version::new(9, 0, 0)), t0());
    let events = fake.take();
    assert_eq!(events.last(), Some(&Event::Install));
    assert!(!events.iter().any(|e| matches!(e, Event::Shown(_))));

    // Nothing waiting: nothing to install, and DST-R57's download text.
    let fake = Fake::new();
    fake.answers("9.0.0");
    fake.after_check(Checked::UpToDate, t0());
    let events = fake.take();
    assert!(!events.contains(&Event::Install));
    assert_eq!(events.last(), Some(&Event::Shown(Text::Download)));
}

#[test]
fn outside_update_required_a_waiting_update_doesnt_install_after_the_check() {
    // DST-R54: while the version is allowed, a waiting update installs from
    // Restart to update, the idle install (update_policy) or the next start
    let fake = Fake::new();
    fake.answers("0.0.0");
    fake.waiting.set(true);
    fake.after_check(Checked::Waiting(Version::new(2, 0, 0)), t0());
    assert!(!fake.take().contains(&Event::Install));
}

#[test]
fn the_required_state_config_interval_is_fifteen_minutes() {
    // DST Tunables: required-state config interval
    assert_eq!(REQUIRED_READ_INTERVAL, 15 * MINUTE);
}

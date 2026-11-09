//! The update policy's decisions against a recording [`Env`]: no updater, no
//! window, no tray, no notifications.

use std::cell::{Cell, RefCell};

use semver::Version;

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    /// An install the Player didn't choose, quiet.
    Install,
    /// The install Check for updates found, passive.
    Chosen,
    Shown(Nudge),
    Notified,
    Line(CheckLine),
}

/// A Companion with an update waiting or not, and what keeps it busy.
struct Fake {
    policy: Policy,
    waiting: Cell<bool>,
    required: Cell<bool>,
    automatically: Cell<bool>,
    busy: Cell<Busy>,
    /// What an install comes to.
    installs: Cell<Outcome>,
    events: RefCell<Vec<Event>>,
}

#[derive(Debug, Clone, Copy)]
enum Outcome {
    Started,
    CouldntStart,
    NotVerified,
}

impl Fake {
    /// Running 1.0.0, idle, the setting on as by default, nothing waiting.
    fn new() -> Self {
        Self::running("1.0.0")
    }

    fn running(version: &str) -> Self {
        Self {
            policy: Policy::new(version),
            waiting: Cell::new(false),
            required: Cell::new(false),
            automatically: Cell::new(true),
            busy: Cell::new(Busy::default()),
            installs: Cell::new(Outcome::Started),
            events: RefCell::default(),
        }
    }

    fn recommend(&self, version: &str) {
        self.policy.recommend(&Version::parse(version).unwrap());
    }

    fn tick(&self) {
        self.policy.tick(self);
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
    fn update_waiting(&self) -> bool {
        self.waiting.get()
    }

    fn update_required(&self) -> bool {
        self.required.get()
    }

    fn installs_automatically(&self) -> bool {
        self.automatically.get()
    }

    fn busy(&self) -> Busy {
        self.busy.get()
    }

    fn install(&self, why: Install) -> Installed {
        self.push(match why {
            Install::Chosen => Event::Chosen,
            Install::Forced | Install::Idle => Event::Install,
        });
        match self.installs.get() {
            Outcome::Started => Installed::Started(Version::new(2, 0, 0)),
            Outcome::CouldntStart => Installed::CouldntStart,
            Outcome::NotVerified => {
                // The installer is deleted: nothing waits any more.
                self.waiting.set(false);
                Installed::NotVerified { in_a_row: 1 }
            }
        }
    }

    fn show(&self, nudge: Nudge) {
        self.push(Event::Shown(nudge));
    }

    fn notify(&self) {
        self.push(Event::Notified);
    }

    fn show_check(&self, line: &CheckLine) {
        self.push(Event::Line(line.clone()));
    }
}

fn v(version: &str) -> Version {
    Version::parse(version).unwrap()
}

const IDLE: Busy = Busy {
    upload_run: false,
    linking: false,
    window_open: false,
};

#[test]
fn idle_is_no_upload_run_no_linking_and_the_window_closed() {
    // DST-R76: idle means no upload run in progress, no linking in progress
    // and the window closed; each alone keeps it busy
    assert!(IDLE.is_idle());
    for busy in [
        Busy {
            upload_run: true,
            ..IDLE
        },
        Busy {
            linking: true,
            ..IDLE
        },
        Busy {
            window_open: true,
            ..IDLE
        },
    ] {
        assert!(!busy.is_idle(), "{busy:?}");
    }
}

#[test]
fn a_waiting_update_installs_when_idle_with_the_setting_on() {
    // DST-R76, DST-R77: with "Install updates automatically" on, a waiting
    // update installs as soon as the Companion is idle
    assert_eq!(decide(true, false, true, IDLE), Some(Install::Idle));
    assert_eq!(decide(false, false, true, IDLE), None, "nothing waiting");
}

#[test]
fn each_busy_condition_holds_the_idle_install_back() {
    // DST-R76: no install during an upload run, during linking, or while
    // the window is open; there is no save timer
    for busy in [
        Busy {
            upload_run: true,
            ..IDLE
        },
        Busy {
            linking: true,
            ..IDLE
        },
        Busy {
            window_open: true,
            ..IDLE
        },
        Busy {
            upload_run: true,
            linking: true,
            window_open: true,
        },
    ] {
        assert_eq!(decide(true, false, true, busy), None, "{busy:?}");
    }
}

#[test]
fn with_the_setting_off_nothing_installs_by_itself() {
    // DST-R77: off, a waiting update installs only through Restart to
    // update or at the next start (DST-R54), unless update required
    assert_eq!(decide(true, false, false, IDLE), None);
    assert_eq!(
        decide(
            true,
            false,
            false,
            Busy {
                window_open: true,
                ..IDLE
            }
        ),
        None
    );
}

#[test]
fn update_required_installs_at_once_whatever_the_setting_or_idle_state() {
    // DST-R56, DST-R59: under update required a waiting update installs at
    // once, whether or not idle and whatever DST-R77's setting
    let busy = Busy {
        upload_run: true,
        linking: true,
        window_open: true,
    };
    for automatically in [true, false] {
        for state in [IDLE, busy] {
            assert_eq!(
                decide(true, true, automatically, state),
                Some(Install::Forced),
                "{automatically} {state:?}"
            );
        }
    }
    assert_eq!(decide(false, true, true, IDLE), None, "nothing waiting");
}

#[test]
fn a_tick_installs_once_idle() {
    // DST-R76: tested at every update tick, so it installs within a tick of
    // becoming idle; an install that starts shows nothing more
    let fake = Fake::new();
    fake.waiting.set(true);
    fake.busy.set(Busy {
        window_open: true,
        ..IDLE
    });
    fake.tick();
    assert!(!fake.take().contains(&Event::Install));

    fake.busy.set(IDLE);
    fake.tick();
    assert_eq!(fake.take(), [Event::Install]);
}

#[test]
fn a_tick_doesnt_install_with_the_setting_off_or_while_busy() {
    // DST-R76, DST-R77
    let fake = Fake::new();
    fake.waiting.set(true);
    fake.automatically.set(false);
    fake.tick();
    assert!(!fake.take().contains(&Event::Install));

    fake.automatically.set(true);
    for busy in [
        Busy {
            upload_run: true,
            ..IDLE
        },
        Busy {
            linking: true,
            ..IDLE
        },
    ] {
        fake.busy.set(busy);
        fake.tick();
        assert!(!fake.take().contains(&Event::Install), "{busy:?}");
    }
}

#[test]
fn the_forced_install_is_the_checks_not_the_ticks() {
    // DST-R56: the install at once follows the update check
    // (update_required); a tick under update required doesn't repeat it
    let fake = Fake::new();
    fake.waiting.set(true);
    fake.required.set(true);
    fake.tick();
    assert!(!fake.take().contains(&Event::Install));
}

#[test]
fn the_forced_install_runs_through_the_modes_installer() {
    // DST-R56, DST-R58: tray mode and headless mode each set what an
    // install without the Player does; without one nothing happens
    let policy = Policy::new("1.0.0");
    assert_eq!(policy.install(Install::Forced), Installed::NothingWaiting);
    policy.install_with(|_| Installed::Started(Version::new(2, 0, 0)));
    assert_eq!(
        policy.install(Install::Forced),
        Installed::Started(Version::new(2, 0, 0))
    );
    assert_eq!(
        policy.install(Install::Idle),
        Installed::Started(Version::new(2, 0, 0))
    );
}

#[test]
fn an_idle_install_that_couldnt_start_waits_for_the_next_check() {
    // DST-R54: an installer that couldn't start stays waiting; it isn't
    // started again every tick, only after the next update check
    let fake = Fake::new();
    fake.waiting.set(true);
    fake.installs.set(Outcome::CouldntStart);
    fake.tick();
    assert!(fake.take().contains(&Event::Install));
    fake.tick();
    assert!(!fake.take().contains(&Event::Install));

    fake.policy.checked(&fake, &Checked::UpToDate);
    fake.installs.set(Outcome::Started);
    fake.tick();
    assert_eq!(fake.take(), [Event::Install]);
}

#[test]
fn an_idle_install_that_fails_verification_shows_nothing_waiting() {
    // DST-R54: a kept installer that fails its second verification is
    // deleted; the window's line goes away with it
    let fake = Fake::new();
    fake.waiting.set(true);
    fake.automatically.set(false);
    fake.tick();
    assert_eq!(
        fake.take(),
        [Event::Shown(Nudge::Important), Event::Notified]
    );

    fake.automatically.set(true);
    fake.installs.set(Outcome::NotVerified);
    fake.tick();
    assert_eq!(fake.take(), [Event::Install, Event::Shown(Nudge::None)]);
}

#[test]
fn every_waiting_update_nudges() {
    // DST-R59, DST-R78: an update is waiting: the important-update texts,
    // whatever the recommended version
    assert_eq!(nudge(true, false), Nudge::Important);
}

#[test]
fn with_nothing_waiting_nothing_shows() {
    // DST-R78: nothing waiting (still downloading, or DST-R54's Linux case):
    // none of the update texts shows
    assert_eq!(nudge(false, false), Nudge::None);
}

#[test]
fn under_update_required_the_update_texts_give_way() {
    // DST-R59, DST-R78: while update required, DST-R57's notification and
    // the stop message take the place of the update texts
    assert_eq!(nudge(true, true), Nudge::None);
    assert_eq!(nudge(false, true), Nudge::None);
}

#[test]
fn the_texts_are_dst_r78s() {
    // DST-R78: the four texts
    assert_eq!(
        Nudge::Important.window_text(),
        Some("An important update is ready.")
    );
    assert_eq!(Nudge::Important.tone(), Some("act"));
    assert_eq!(
        Nudge::Important.tray_line(),
        Some("Update ready: restart to update")
    );
    assert_eq!(
        crate::notify::Cause::UpdateRecommended.text(),
        "An important update for Pagle Companion is ready. Choose Restart to update in its tray menu."
    );
    assert_eq!(SETTING_TEXT, "Install updates automatically");
}

#[test]
fn which_texts_show_in_which_state() {
    // DST-R78: the act notice, the tray line and the notification while an
    // update is waiting; nothing without one
    assert_eq!(Nudge::None.window_text(), None);
    assert_eq!(Nudge::None.tray_line(), None);
    assert!(!Nudge::None.notifies());
    assert!(Nudge::Important.notifies());

    assert_eq!(
        serde_json::to_value(UpdateView::from(Nudge::Important)).unwrap(),
        serde_json::json!({ "text": "An important update is ready.", "tone": "act" })
    );
    assert_eq!(
        serde_json::to_value(UpdateView::from(Nudge::None)).unwrap(),
        serde_json::json!({ "text": null, "tone": null })
    );
}

#[test]
fn the_nudge_notifies_exactly_once_per_run() {
    // DST-R78: the notification shows once per process run, however often
    // the update goes and comes or the recommended version moves
    let fake = Fake::new();
    fake.automatically.set(false);
    fake.recommend("1.1.0");
    fake.waiting.set(true);
    fake.tick();
    assert_eq!(
        fake.take(),
        [Event::Shown(Nudge::Important), Event::Notified]
    );
    fake.tick();
    assert!(fake.take().is_empty());

    // Installed elsewhere, waiting again, recommended raised: shown again,
    // never notified again.
    fake.waiting.set(false);
    fake.tick();
    fake.recommend("1.2.0");
    fake.waiting.set(true);
    fake.tick();
    let events = fake.take();
    assert_eq!(
        events,
        [Event::Shown(Nudge::None), Event::Shown(Nudge::Important)]
    );
}

#[test]
fn the_recommended_version_decides_nothing() {
    // DST-R59, DST-R67: still read, and every waiting update nudges whether
    // the Companion is below, at or above it, or nothing was read
    for recommended in [None, Some("0.0.0"), Some("1.0.0"), Some("2.0.0")] {
        let fake = Fake::new();
        fake.automatically.set(false);
        if let Some(version) = recommended {
            fake.recommend(version);
        }
        fake.waiting.set(true);
        fake.tick();
        assert_eq!(
            fake.take(),
            [Event::Shown(Nudge::Important), Event::Notified],
            "{recommended:?}"
        );
    }
}

#[test]
fn under_update_required_the_nudge_doesnt_notify_until_it_is_left() {
    // DST-R59: while update required, DST-R57 speaks instead; once left,
    // the nudge notifies as usual
    let fake = Fake::new();
    fake.recommend("2.0.0");
    fake.waiting.set(true);
    fake.required.set(true);
    fake.tick();
    assert!(fake.take().is_empty());
    assert_eq!(fake.policy.shown(), Nudge::None);

    fake.required.set(false);
    fake.automatically.set(false);
    fake.tick();
    assert_eq!(
        fake.take(),
        [Event::Shown(Nudge::Important), Event::Notified]
    );
}

#[test]
fn an_idle_install_that_starts_shows_and_notifies_nothing() {
    // DST-R76: no window and no notification afterwards
    let fake = Fake::new();
    fake.recommend("2.0.0");
    fake.waiting.set(true);
    fake.tick();
    assert_eq!(fake.take(), [Event::Install]);
}

#[test]
fn a_busy_companion_below_recommended_nudges_until_idle() {
    // DST-R76, DST-R78: the window open holds the install back and shows
    // the act notice; once closed, the update installs
    let fake = Fake::new();
    fake.recommend("2.0.0");
    fake.waiting.set(true);
    fake.busy.set(Busy {
        window_open: true,
        ..IDLE
    });
    fake.tick();
    assert_eq!(
        fake.take(),
        [Event::Shown(Nudge::Important), Event::Notified]
    );
    fake.busy.set(IDLE);
    fake.tick();
    assert_eq!(fake.take(), [Event::Install]);
}

// Check for updates (DST-R102).

/// A Companion whose Player just pressed Check for updates, the check not
/// yet run.
fn asked() -> Fake {
    let fake = Fake::new();
    assert!(fake.policy.ask_check(&fake));
    assert_eq!(fake.take(), [Event::Line(CheckLine::Checking)]);
    fake
}

#[test]
fn check_for_updates_runs_one_check_at_a_time() {
    // DST-R102: the button is disabled while a check or an install it asked
    // for is under way; a second press asks nothing
    let fake = asked();
    assert!(fake.policy.check_line().busy());
    assert!(!fake.policy.ask_check(&fake));
    assert!(fake.take().is_empty());
}

#[test]
fn up_to_date_says_so_with_the_running_version() {
    // DST-R102: no higher version: "Pagle Companion is up to date (…)."
    let fake = asked();
    fake.policy.checked(&fake, &Checked::UpToDate);
    let line = CheckLine::UpToDate(v("1.0.0"));
    assert_eq!(fake.take(), [Event::Line(line.clone())]);
    assert_eq!(
        line.text().as_deref(),
        Some("Pagle Companion is up to date (1.0.0).")
    );
    assert!(!line.busy() && !line.problem());
    // And the button can be pressed again.
    assert!(fake.policy.ask_check(&fake));
}

#[test]
fn a_failed_check_says_so_without_a_notification() {
    // DST-R102: offline or GitHub down, a download that didn't complete, a
    // failed verification, an AppImage that can't be replaced: a line each,
    // no notification (DST-R57, DST-R78 keep theirs)
    for (checked, line) in [
        (Checked::Failed, CheckLine::Failed),
        (Checked::NotDownloaded, CheckLine::Failed),
        (Checked::NotVerified { in_a_row: 1 }, CheckLine::NotVerified),
        (Checked::CantDownload, CheckLine::CantDownload),
    ] {
        let fake = asked();
        fake.policy.checked(&fake, &checked);
        assert_eq!(fake.take(), [Event::Line(line.clone())], "{checked:?}");
        assert!(line.problem() && !line.busy(), "{checked:?}");
    }
}

#[test]
fn a_scheduled_check_shows_nothing_beside_the_button() {
    // DST-R102: only a check the Player asked for has a line
    let fake = Fake::new();
    fake.policy.checked(&fake, &Checked::Failed);
    fake.policy.checked(&fake, &Checked::Waiting(v("2.0.0")));
    assert!(fake.take().is_empty());
    assert_eq!(fake.policy.check_line(), CheckLine::None);
}

#[test]
fn an_update_found_installs_at_once_passive_whatever_the_setting_or_window() {
    // DST-R102: a higher version, downloaded and verified, installs as
    // Restart to update does, though the setting is off and the window open
    let fake = asked();
    fake.automatically.set(false);
    fake.busy.set(Busy {
        window_open: true,
        ..IDLE
    });
    fake.waiting.set(true);
    fake.policy.checked(&fake, &Checked::Waiting(v("2.0.0")));
    assert_eq!(
        fake.take(),
        [Event::Line(CheckLine::Installing(v("2.0.0")))]
    );
    fake.tick();
    assert_eq!(fake.take(), [Event::Chosen]);
}

#[test]
fn an_upload_run_or_linking_holds_the_chosen_install_back_until_it_ends() {
    // DST-R102: an upload run or linking in progress is let finish, then
    // the update installs
    for busy in [
        Busy {
            upload_run: true,
            ..IDLE
        },
        Busy {
            linking: true,
            ..IDLE
        },
    ] {
        let fake = asked();
        fake.waiting.set(true);
        fake.policy.checked(&fake, &Checked::Waiting(v("2.0.0")));
        fake.take();
        fake.busy.set(busy);
        fake.tick();
        let events = fake.take();
        assert!(!events.contains(&Event::Chosen), "{busy:?}");
        assert!(
            events.contains(&Event::Line(CheckLine::InstallsAfter(v("2.0.0")))),
            "{busy:?}"
        );
        assert!(fake.policy.check_line().busy(), "{busy:?}");

        fake.busy.set(IDLE);
        fake.tick();
        assert_eq!(
            fake.take(),
            [
                Event::Line(CheckLine::Installing(v("2.0.0"))),
                Event::Chosen
            ],
            "{busy:?}"
        );
    }
}

#[test]
fn a_chosen_install_that_fails_says_so_and_frees_the_button() {
    // DST-R102, DST-R54: an installer that couldn't start stays waiting; one
    // that fails its second verification is deleted
    for (outcome, line) in [
        (Outcome::CouldntStart, CheckLine::CouldntStart),
        (Outcome::NotVerified, CheckLine::NotVerified),
    ] {
        let fake = asked();
        fake.automatically.set(false);
        fake.waiting.set(true);
        fake.installs.set(outcome);
        fake.policy.checked(&fake, &Checked::Waiting(v("2.0.0")));
        fake.take();
        fake.tick();
        let events = fake.take();
        assert_eq!(events[0], Event::Chosen, "{outcome:?}");
        assert!(events.contains(&Event::Line(line.clone())), "{outcome:?}");
        assert!(!fake.policy.check_line().busy(), "{outcome:?}");
    }
}

#[test]
fn an_update_gone_before_the_chosen_install_clears_the_line() {
    // DST-R102: installed or discarded some other way: nothing to install
    let fake = asked();
    fake.policy.checked(&fake, &Checked::Waiting(v("2.0.0")));
    fake.take();
    fake.tick();
    assert_eq!(fake.take(), [Event::Line(CheckLine::None)]);
    assert!(!fake.policy.check_line().busy());
}

#[test]
fn the_check_for_updates_texts() {
    // DST-R102: the button and its lines
    assert_eq!(CHECK_BUTTON, "Check for updates");
    let version = v("0.3.1");
    for (line, text) in [
        (CheckLine::None, None),
        (CheckLine::Checking, Some("Checking for updates…")),
        (
            CheckLine::UpToDate(version.clone()),
            Some("Pagle Companion is up to date (0.3.1)."),
        ),
        (
            CheckLine::Installing(version.clone()),
            Some("Installing Pagle Companion 0.3.1…"),
        ),
        (
            CheckLine::InstallsAfter(version.clone()),
            Some("Pagle Companion 0.3.1 installs once the upload or linking in progress finishes."),
        ),
        (
            CheckLine::Failed,
            Some("Couldn't check for updates. Check your internet connection, then try again."),
        ),
        (
            CheckLine::NotVerified,
            Some("The update couldn't be verified, so it wasn't installed. Try again later."),
        ),
        (
            CheckLine::CantDownload,
            Some(
                "A newer Pagle Companion is out, but this copy can't update itself. Download the latest version from pagle.app/download.",
            ),
        ),
        (
            CheckLine::CouldntStart,
            Some("The update couldn't start. Try again, or restart Pagle Companion."),
        ),
    ] {
        assert_eq!(line.text().as_deref(), text, "{line:?}");
    }
    assert_eq!(
        serde_json::to_value(CheckView::from(&CheckLine::Checking)).unwrap(),
        serde_json::json!({ "busy": true, "text": "Checking for updates…", "problem": false })
    );
}

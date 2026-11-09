//! What the Companion does with a waiting update (DST-R53) besides
//! **Restart to update** and the install at the next start (DST-R54): the
//! update tiers (DST-R59), read from `latest.json` and the config the
//! Companion already polls (DST-R63).
//!
//! - **Latest:** with "Install updates automatically" on (DST-R77: local
//!   state, `settings.installUpdatesAutomatically`, on by default, read in
//!   headless mode too), a waiting update installs once the Companion is
//!   idle (DST-R76, [`decide`], [`Busy`]): no upload run in progress, no
//!   linking in progress and the window closed. There is no save timer:
//!   WoW writes `Pagle.lua` only on `/reload` or logout, and the disk is the
//!   queue. It installs quietly ([`crate::updater::install::Ui::Quiet`]): on
//!   Windows NSIS `/S`, on Linux a restart into the AppImage already in
//!   place (DST-R54), each with the running process's arguments plus
//!   `--autostart`, so the Companion comes back in the tray with no window;
//!   headless exits with status 0 so the unit starts the new version
//!   (DST-R58).
//! - **The nudge:** every waiting update nudges the Player with DST-R78's
//!   important-update texts ([`Nudge::Important`]): the window's `act`
//!   notice, the tray's status line and one notification per process run.
//!   Pagle releases only to fix things, and the idle install takes the
//!   update quietly wherever it can, so the nudge reaches only the installs
//!   that haven't updated (DST-R59). `recommendedCompanionVersion`
//!   (DST-R67) is still read and logged, and decides nothing: it nudges only
//!   versions before 0.3.1, which compare against it.
//! - **Minimum:** under update required (DST-R56) a waiting update installs
//!   at once, quietly too, whatever the setting or idle state
//!   ([`Install::Forced`]). That install follows the update check, so it is
//!   `update_required`'s (`Enforcer::after_check`); with nothing it can
//!   install, DST-R57's notification shows as before, and the nudge gives
//!   way to it and the stop message (DST-R59, DST-R78).
//!
//! - **Check for updates** (DST-R102): the Player's button at the bottom
//!   of Settings runs an update check at once, the same check as the
//!   scheduled one. A waiting update, found then or already, installs as
//!   soon as no upload run and no linking is in progress, passive as
//!   **Restart to update** is ([`Install::Chosen`]), whatever the setting
//!   and whether the window is open. A line beside the button says how it
//!   went ([`CheckLine`]); it raises no notification.
//!
//! [`Policy`] makes the decisions at every update tick (DST-R51), right after
//! any check, and acts through an [`Env`]; `window` is its glue in tray and
//! headless mode.

pub mod window;

#[cfg(test)]
mod tests;

use std::sync::{Mutex, MutexGuard, OnceLock};

use semver::Version;
use serde::Serialize;

use crate::updater::{Checked, Installed};

/// The window's notice while an update is waiting (`act` tone).
pub const IMPORTANT_TEXT: &str = "An important update is ready.";
/// The tray's status line while an update is waiting.
pub const TRAY_LINE: &str = "Update ready: restart to update";
/// The Settings switch, under "Start at login".
pub const SETTING_TEXT: &str = "Install updates automatically";
/// The button at the bottom of Settings (DST-R102).
pub const CHECK_BUTTON: &str = "Check for updates";
/// Its line while the check runs.
pub const CHECKING_TEXT: &str = "Checking for updates…";
/// Its line when no higher version exists; `{version}` is the running one.
pub const UP_TO_DATE_TEXT: &str = "Pagle Companion is up to date ({version}).";
/// Its line while the update it found installs.
pub const INSTALLING_TEXT: &str = "Installing Pagle Companion {version}…";
/// Its line while an upload run or linking holds the install back.
pub const INSTALLS_AFTER_TEXT: &str =
    "Pagle Companion {version} installs once the upload or linking in progress finishes.";
/// Its line when the check failed: offline, GitHub down, a bad document, a
/// download that didn't complete.
pub const CHECK_FAILED_TEXT: &str =
    "Couldn't check for updates. Check your internet connection, then try again.";
/// Its line when the update failed verification.
pub const NOT_VERIFIED_TEXT: &str =
    "The update couldn't be verified, so it wasn't installed. Try again later.";
/// Its line when a higher version exists but this AppImage can't be
/// replaced (DST-R54).
pub const CANT_DOWNLOAD_TEXT: &str = "A newer Pagle Companion is out, but this copy can't update itself. Download the latest version from pagle.app/download.";
/// Its line when the installer or the new AppImage couldn't start.
pub const COULDNT_START_TEXT: &str =
    "The update couldn't start. Try again, or restart Pagle Companion.";

/// What keeps the Companion from being idle (DST-R76).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Busy {
    /// An upload run is in progress or queued (CMP-R46).
    pub upload_run: bool,
    /// A device flow is starting or waiting for approval (CMP-R60).
    pub linking: bool,
    /// The window is open. Headless mode has none.
    pub window_open: bool,
}

impl Busy {
    pub fn is_idle(self) -> bool {
        !self.upload_run && !self.linking && !self.window_open
    }
}

/// Why the policy installs a waiting update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// Update required: at once, whatever the setting or idle state; quiet.
    Forced,
    /// "Install updates automatically" is on and the Companion is idle;
    /// quiet.
    Idle,
    /// The Player chose **Check for updates** (DST-R102): passive, with the
    /// process's arguments unchanged, as **Restart to update** (DST-R54).
    Chosen,
}

/// Whether a waiting update installs now, and why (DST-R56, DST-R76).
pub fn decide(waiting: bool, required: bool, automatically: bool, busy: Busy) -> Option<Install> {
    if !waiting {
        None
    } else if required {
        Some(Install::Forced)
    } else if automatically && busy.is_idle() {
        Some(Install::Idle)
    } else {
        None
    }
}

/// What the window and the tray show of a waiting update.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Nudge {
    /// No update is waiting.
    #[default]
    None,
    /// An update is waiting.
    Important,
}

impl Nudge {
    /// The window's notice in Status, with **Restart to update**.
    pub fn window_text(self) -> Option<&'static str> {
        match self {
            Nudge::None => None,
            Nudge::Important => Some(IMPORTANT_TEXT),
        }
    }

    /// The notice's tone (`ui/style.css`, Notices).
    pub fn tone(self) -> Option<&'static str> {
        match self {
            Nudge::None => None,
            Nudge::Important => Some("act"),
        }
    }

    /// The tray's status line, when the nudge has one.
    pub fn tray_line(self) -> Option<&'static str> {
        (self == Nudge::Important).then_some(TRAY_LINE)
    }

    /// Whether it raises the nudge's notification, once per process run.
    pub fn notifies(self) -> bool {
        self == Nudge::Important
    }
}

/// The nudge with an update `waiting` or not, and update `required` or not
/// (DST-R59): every waiting update is important. Nothing without a waiting
/// update, and nothing while update required: DST-R57 and the stop message
/// take its place (DST-R78, DST-R59).
pub fn nudge(waiting: bool, required: bool) -> Nudge {
    if waiting && !required {
        Nudge::Important
    } else {
        Nudge::None
    }
}

/// The line beside **Check for updates** (DST-R102).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CheckLine {
    /// Not asked in this process run, or nothing more to say.
    #[default]
    None,
    /// The check the Player asked for hasn't run yet.
    Checking,
    /// No higher version than the running one.
    UpToDate(Version),
    /// The update found installs at the next tick.
    Installing(Version),
    /// An upload run or linking holds the install back.
    InstallsAfter(Version),
    /// The check failed or the download didn't complete.
    Failed,
    /// The update failed verification.
    NotVerified,
    /// A higher version exists, and this AppImage can't be replaced.
    CantDownload,
    /// The new version couldn't be started; the update stays waiting.
    CouldntStart,
}

impl CheckLine {
    /// The line after the check the Player asked for came to `checked`,
    /// running `running`.
    pub fn after(checked: &Checked, running: &Version) -> Self {
        match checked {
            Checked::UpToDate => CheckLine::UpToDate(running.clone()),
            Checked::Waiting(version) => CheckLine::Installing(version.clone()),
            Checked::Failed | Checked::NotDownloaded => CheckLine::Failed,
            Checked::NotVerified { .. } => CheckLine::NotVerified,
            Checked::CantDownload => CheckLine::CantDownload,
        }
    }

    /// Whether a check or an install is under way: the button is disabled.
    pub fn busy(&self) -> bool {
        matches!(
            self,
            CheckLine::Checking | CheckLine::Installing(_) | CheckLine::InstallsAfter(_)
        )
    }

    /// The version an install is under way for.
    fn installing(&self) -> Option<&Version> {
        match self {
            CheckLine::Installing(version) | CheckLine::InstallsAfter(version) => Some(version),
            _ => None,
        }
    }

    pub fn text(&self) -> Option<String> {
        let with = |text: &str, version: &Version| text.replace("{version}", &version.to_string());
        Some(match self {
            CheckLine::None => return None,
            CheckLine::Checking => CHECKING_TEXT.into(),
            CheckLine::UpToDate(version) => with(UP_TO_DATE_TEXT, version),
            CheckLine::Installing(version) => with(INSTALLING_TEXT, version),
            CheckLine::InstallsAfter(version) => with(INSTALLS_AFTER_TEXT, version),
            CheckLine::Failed => CHECK_FAILED_TEXT.into(),
            CheckLine::NotVerified => NOT_VERIFIED_TEXT.into(),
            CheckLine::CantDownload => CANT_DOWNLOAD_TEXT.into(),
            CheckLine::CouldntStart => COULDNT_START_TEXT.into(),
        })
    }

    /// Whether the line reports a problem, shown as one.
    pub fn problem(&self) -> bool {
        matches!(
            self,
            CheckLine::Failed
                | CheckLine::NotVerified
                | CheckLine::CantDownload
                | CheckLine::CouldntStart
        )
    }
}

/// The window's view of **Check for updates**: the `update-check` event and
/// the `update_check_view` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckView {
    /// The button is disabled while a check or an install is under way.
    pub busy: bool,
    pub text: Option<String>,
    pub problem: bool,
}

impl From<&CheckLine> for CheckView {
    fn from(line: &CheckLine) -> Self {
        Self {
            busy: line.busy(),
            text: line.text(),
            problem: line.problem(),
        }
    }
}

/// The window's view of the nudge: the `update` event and the `update_view`
/// command. Both are `null` while no update waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct UpdateView {
    pub text: Option<&'static str>,
    pub tone: Option<&'static str>,
}

impl From<Nudge> for UpdateView {
    fn from(nudge: Nudge) -> Self {
        Self {
            text: nudge.window_text(),
            tone: nudge.tone(),
        }
    }
}

/// What the decisions act on: the updater, local state, the parts that keep
/// the Companion busy, the window, the tray and the notifications.
pub trait Env {
    /// Whether an update is waiting (DST-R53).
    fn update_waiting(&self) -> bool;
    /// Whether update required holds (DST-R56).
    fn update_required(&self) -> bool;
    /// "Install updates automatically", from local state.
    fn installs_automatically(&self) -> bool;
    /// What keeps the Companion from being idle now.
    fn busy(&self) -> Busy;
    /// Installs the waiting update for `why`: quietly for an install the
    /// Player didn't choose, where tray mode runs the installer with `/S` or
    /// restarts into the AppImage and exits, and headless mode exits with
    /// status 0 (DST-R58); passive for [`Install::Chosen`].
    fn install(&self, why: Install) -> Installed;
    /// Shows `nudge` in the window and the tray.
    fn show(&self, nudge: Nudge);
    /// Shows the line beside **Check for updates**.
    fn show_check(&self, line: &CheckLine);
    /// Raises the nudge's notification (held while the first-run window is
    /// open, CMP-R100).
    fn notify(&self);
}

/// The process's update policy.
pub struct Policy {
    running: Version,
    inner: Mutex<Inner>,
    /// What an install does in this mode, for each reason: set once by tray
    /// or headless mode at start.
    installer: OnceLock<Box<dyn Fn(Install) -> Installed + Send + Sync>>,
}

struct Inner {
    /// The config's `recommendedCompanionVersion`, `0.0.0` until read: only
    /// logged when it changes (DST-R67).
    recommended: Version,
    /// The nudge as last shown.
    shown: Nudge,
    /// Whether the nudge has notified in this process run.
    notified: bool,
    /// An idle install couldn't start: no other until the next check.
    held_back: bool,
    /// The line beside **Check for updates**.
    check: CheckLine,
}

impl Policy {
    /// The policy for the running `version`.
    pub fn new(version: &str) -> Self {
        let running = Version::parse(version).unwrap_or_else(|e| {
            log::error!("the running version {version:?} isn't SemVer ({e}); taking it as 0.0.0");
            Version::new(0, 0, 0)
        });
        Self {
            running,
            inner: Mutex::new(Inner {
                recommended: Version::new(0, 0, 0),
                shown: Nudge::None,
                notified: false,
                held_back: false,
                check: CheckLine::None,
            }),
            installer: OnceLock::new(),
        }
    }

    /// What an install does in this process, for each reason. A second
    /// call changes nothing.
    pub fn install_with(&self, installer: impl Fn(Install) -> Installed + Send + Sync + 'static) {
        let _ = self.installer.set(Box::new(installer));
    }

    /// Installs the waiting update for `why`, as this mode does
    /// ([`Policy::install_with`]). Nothing without an installer.
    pub fn install(&self, why: Install) -> Installed {
        let Some(installer) = self.installer.get() else {
            return Installed::NothingWaiting;
        };
        log::info!(
            "installing the waiting update {}",
            match why {
                Install::Forced => "quietly: update required",
                Install::Idle => "quietly: the Companion is idle",
                Install::Chosen => "passive: the Player chose Check for updates",
            }
        );
        installer(why)
    }

    /// **Check for updates** (DST-R102): whether a check should run now.
    /// Not while a check or an install it asked for is under way.
    pub fn ask_check<E: Env>(&self, env: &E) -> bool {
        if self.lock().check.busy() {
            return false;
        }
        self.set_check(env, CheckLine::Checking);
        true
    }

    /// The line beside **Check for updates**, as last shown.
    pub fn check_line(&self) -> CheckLine {
        self.lock().check.clone()
    }

    /// A config read's `recommendedCompanionVersion` (DST-R63).
    pub fn recommend(&self, version: &Version) {
        let mut inner = self.lock();
        if inner.recommended != *version {
            log::info!("the recommended Companion version is now {version}");
            inner.recommended = version.clone();
        }
    }

    /// The nudge as last shown.
    pub fn shown(&self) -> Nudge {
        self.lock().shown
    }

    /// After every update check: an idle install held back by one that
    /// couldn't start may be tried again, and a check the Player asked for
    /// shows how it went (DST-R102). The first check after the click
    /// answers it, even one that was already running.
    pub fn checked<E: Env>(&self, env: &E, checked: &Checked) {
        let asked = {
            let mut inner = self.lock();
            inner.held_back = false;
            inner.check == CheckLine::Checking
        };
        if asked {
            self.set_check(env, CheckLine::after(checked, &self.running));
        }
    }

    /// At every update tick (DST-R51), right after any check: installs a
    /// waiting update if the setting is on and the Companion is idle, else
    /// shows the nudge. The forced install is the check's
    /// (`update_required`), so a tick never repeats it.
    pub fn tick<E: Env>(&self, env: &E) {
        if self.chosen_install(env) {
            return;
        }
        let waiting = env.update_waiting();
        let why = decide(
            waiting,
            env.update_required(),
            env.installs_automatically(),
            env.busy(),
        );
        if why == Some(Install::Idle) && !self.lock().held_back {
            match env.install(Install::Idle) {
                Installed::Started(_) => return,
                Installed::CouldntStart => self.lock().held_back = true,
                Installed::NothingWaiting | Installed::NotVerified { .. } => {}
            }
        }
        self.show(env);
    }

    /// Shows the nudge for the update waiting now, if it changed, and
    /// raises its notification the first time it is [`Nudge::Important`]
    /// in this process run (DST-R78).
    pub fn show<E: Env>(&self, env: &E) {
        let waiting = env.update_waiting();
        let required = env.update_required();
        let mut inner = self.lock();
        let nudge = nudge(waiting, required);
        let changed = std::mem::replace(&mut inner.shown, nudge) != nudge;
        let notify = nudge.notifies() && !inner.notified;
        if notify {
            inner.notified = true;
        }
        drop(inner);
        if changed {
            env.show(nudge);
        }
        if notify {
            env.notify();
        }
    }

    /// The install **Check for updates** found an update for (DST-R102):
    /// once no upload run and no linking is in progress, whatever the
    /// setting and the window. Returns whether the process is exiting.
    fn chosen_install<E: Env>(&self, env: &E) -> bool {
        let Some(version) = self.lock().check.installing().cloned() else {
            return false;
        };
        if !env.update_waiting() {
            // Installed or discarded some other way.
            self.set_check(env, CheckLine::None);
            return false;
        }
        let busy = env.busy();
        if busy.upload_run || busy.linking {
            self.set_check(env, CheckLine::InstallsAfter(version));
            return false;
        }
        self.set_check(env, CheckLine::Installing(version));
        let line = match env.install(Install::Chosen) {
            Installed::Started(_) => return true,
            Installed::CouldntStart => CheckLine::CouldntStart,
            Installed::NotVerified { .. } => CheckLine::NotVerified,
            Installed::NothingWaiting => CheckLine::None,
        };
        self.set_check(env, line);
        false
    }

    /// Sets the line beside **Check for updates**, shown if it changed. The
    /// lock is let go first: showing reaches the tray, which reads the
    /// policy.
    fn set_check<E: Env>(&self, env: &E, line: CheckLine) {
        let changed = {
            let mut inner = self.lock();
            std::mem::replace(&mut inner.check, line.clone()) != line
        };
        if changed {
            env.show_check(&line);
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

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
//!   Windows NSIS `/S` with the running process's arguments, on Linux a
//!   restart into the AppImage already in place (DST-R54), and headless
//!   exits with status 0 so the unit starts the new version (DST-R58).
//! - **Recommended:** below `recommendedCompanionVersion` (DST-R67), a
//!   waiting update nudges the Player with DST-R78's important-update texts
//!   ([`Nudge::Important`]): the window's `act` notice, the tray's status
//!   line and one notification per process run. Any other waiting update
//!   shows only the window's `info` line ([`Nudge::Ready`], DST-R55).
//! - **Minimum:** under update required (DST-R56) a waiting update installs
//!   at once, quietly too, whatever the setting or idle state
//!   ([`Install::Forced`]). That install follows the update check, so it is
//!   `update_required`'s (`Enforcer::after_check`); with nothing it can
//!   install, DST-R57's notification shows as before, and the nudge gives
//!   way to it and the stop message (DST-R59, DST-R78).
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

use crate::updater::Installed;

/// The window's notice below the recommended version (`act` tone).
pub const IMPORTANT_TEXT: &str = "An important update is ready.";
/// The window's line for any other waiting update (`info` tone).
pub const READY_TEXT: &str = "An update is ready.";
/// The tray's status line below the recommended version.
pub const TRAY_LINE: &str = "Update ready: restart to update";
/// The Settings switch, under "Start at login".
pub const SETTING_TEXT: &str = "Install updates automatically";

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

/// Why a waiting update installs without the Player choosing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// Update required: at once, whatever the setting or idle state.
    Forced,
    /// "Install updates automatically" is on and the Companion is idle.
    Idle,
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
    /// An update is waiting, and the running version isn't below the
    /// recommended one.
    Ready,
    /// An update is waiting and the running version is below the
    /// recommended one.
    Important,
}

impl Nudge {
    /// The window's notice in Status, with **Restart to update**.
    pub fn window_text(self) -> Option<&'static str> {
        match self {
            Nudge::None => None,
            Nudge::Ready => Some(READY_TEXT),
            Nudge::Important => Some(IMPORTANT_TEXT),
        }
    }

    /// The notice's tone (`ui/style.css`, Notices).
    pub fn tone(self) -> Option<&'static str> {
        match self {
            Nudge::None => None,
            Nudge::Ready => Some("info"),
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

/// The nudge for the `running` version, with `recommended` as the last
/// successful config read gave it (`0.0.0` before any; a failed read keeps
/// it, DST-R99), an update `waiting` or not, and update `required` or not.
/// Below by SemVer precedence (DST-R59). Nothing without a waiting update,
/// and nothing while update required: DST-R57 and the stop message take its
/// place (DST-R78, DST-R59).
pub fn nudge(running: &Version, recommended: &Version, waiting: bool, required: bool) -> Nudge {
    if !waiting || required {
        Nudge::None
    } else if running.cmp_precedence(recommended).is_lt() {
        Nudge::Important
    } else {
        Nudge::Ready
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
    /// Installs the waiting update quietly: tray mode runs the installer
    /// with `/S` or restarts into the AppImage and exits; headless mode
    /// exits with status 0 (DST-R58).
    fn install(&self) -> Installed;
    /// Shows `nudge` in the window and the tray.
    fn show(&self, nudge: Nudge);
    /// Raises the nudge's notification (held while the first-run window is
    /// open, CMP-R100).
    fn notify(&self);
}

/// The process's update policy.
pub struct Policy {
    running: Version,
    inner: Mutex<Inner>,
    /// What an install without the Player does in this mode: set once by
    /// tray or headless mode at start.
    installer: OnceLock<Box<dyn Fn() -> Installed + Send + Sync>>,
}

struct Inner {
    /// The config's `recommendedCompanionVersion`, `0.0.0` until read.
    recommended: Version,
    /// The nudge as last shown.
    shown: Nudge,
    /// Whether the nudge has notified in this process run.
    notified: bool,
    /// An idle install couldn't start: no other until the next check.
    held_back: bool,
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
            }),
            installer: OnceLock::new(),
        }
    }

    /// What an install without the Player does in this process. A second
    /// call changes nothing.
    pub fn install_with(&self, installer: impl Fn() -> Installed + Send + Sync + 'static) {
        let _ = self.installer.set(Box::new(installer));
    }

    /// Installs the waiting update without the Player choosing it, as this
    /// mode does ([`Policy::install_with`]). Nothing without an installer.
    pub fn install(&self, why: Install) -> Installed {
        let Some(installer) = self.installer.get() else {
            return Installed::NothingWaiting;
        };
        log::info!(
            "installing the waiting update quietly: {}",
            match why {
                Install::Forced => "update required",
                Install::Idle => "the Companion is idle",
            }
        );
        installer()
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
    /// couldn't start may be tried again.
    pub fn checked(&self) {
        self.lock().held_back = false;
    }

    /// At every update tick (DST-R51), right after any check: installs a
    /// waiting update if the setting is on and the Companion is idle, else
    /// shows the nudge. The forced install is the check's
    /// (`update_required`), so a tick never repeats it.
    pub fn tick<E: Env>(&self, env: &E) {
        let waiting = env.update_waiting();
        let why = decide(
            waiting,
            env.update_required(),
            env.installs_automatically(),
            env.busy(),
        );
        if why == Some(Install::Idle) && !self.lock().held_back {
            match env.install() {
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
        let nudge = nudge(&self.running, &inner.recommended, waiting, required);
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

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

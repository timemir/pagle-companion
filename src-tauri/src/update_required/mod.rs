//! Update required (DST-R56 to DST-R58) and the config reads that drive it.
//! After every update check comes one config read ([`config`], DST-R51,
//! DST-R63). Its Forever product uid is kept in local state (CMP-R12), and
//! its `minCompanionVersion` enters or leaves update required. While update
//! required, config is read every required-state config interval too, and
//! each check picks DST-R57's text, shown as the stop message and raised as
//! the notification once per run.
//!
//! [`Enforcer`] makes the decisions and acts through an [`Env`]; `window` is
//! its glue in tray and headless mode. The updater's hooks call it after each
//! check and at each update tick, and the link's hook calls it when a refusal
//! enters the state. Neither config reads nor update checks go through the
//! upload queue, so a `429` wait or a backoff doesn't delay them (#18).

pub mod config;
pub mod window;

#[cfg(test)]
mod tests;

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use semver::Version;

use crate::updater::Checked;
use crate::updater::record::FAILED_VERIFICATION_COUNT;
use crate::updater::schedule::Schedule;
pub use config::Config;

/// DST Tunables: required-state config interval.
pub const REQUIRED_READ_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// DST-R57's two texts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Text {
    /// An update is waiting.
    RestartToUpdate,
    /// No higher version, verification failed too often, or this AppImage
    /// can't be replaced.
    Download,
}

impl Text {
    pub fn text(self) -> &'static str {
        match self {
            Text::RestartToUpdate => {
                "Pagle Companion needs an update to keep uploading. Choose Restart to update in its tray menu."
            }
            Text::Download => {
                "Pagle Companion needs an update to keep uploading. Download the latest version from pagle.app/download."
            }
        }
    }
}

/// The text whose condition holds after a check that came to `checked`, with
/// an update `waiting` or not and `updater.json`'s failed-verification count
/// (DST-R57). `None` while checks fail outright: the text shown stays.
pub fn pick(checked: &Checked, waiting: bool, failed_verifications: u32) -> Option<Text> {
    if waiting {
        return Some(Text::RestartToUpdate);
    }
    match checked {
        Checked::Failed => None,
        Checked::UpToDate | Checked::CantDownload => Some(Text::Download),
        _ if failed_verifications >= FAILED_VERIFICATION_COUNT => Some(Text::Download),
        Checked::NotVerified { in_a_row } if *in_a_row >= FAILED_VERIFICATION_COUNT => {
            Some(Text::Download)
        }
        _ => None,
    }
}

/// What the decisions act on: the backend, local state, the link, the
/// updater, the window and the process.
pub trait Env {
    /// One config read (DST-R63); `None` when it failed (DST-R99).
    fn read_config(&self) -> Option<Config>;
    /// Keeps the read's Forever product uid in local state; a change starts
    /// a scan (CMP-R12, CMP-R14).
    fn keep_config(&self, config: &Config);
    /// Whether update required holds.
    fn update_required(&self) -> bool;
    /// Enters update required as a stop (CMP-R50), with the `update_required`
    /// message. The link's hook then calls [`Enforcer::entered`].
    fn enter(&self);
    /// Leaves update required and starts what a start would: a run for every
    /// confirmed `logId` and the in-game downloads that are due (CMP-R45,
    /// CMP-R81).
    fn leave(&self);
    /// Whether an update is waiting (DST-R53).
    fn update_waiting(&self) -> bool;
    /// `updater.json`'s failed-verification count (DST-R57).
    fn failed_verifications(&self) -> u32;
    /// Shows `text` as the stop message and raises DST-R57's notification,
    /// at most once per process run.
    fn show(&self, text: Text);
    /// An update check at the next tick (DST-R56).
    fn check_soon(&self);
    /// Stops device-flow polling under way (CMP-R60).
    fn stop_linking(&self);
    /// Headless: exits with status 0 so the unit starts the new version
    /// (DST-R58). Tray mode does nothing: **Restart to update** shows.
    fn exit_for_update(&self);
}

/// The process's update-required decisions.
pub struct Enforcer {
    running: Version,
    inner: Mutex<Inner>,
}

struct Inner {
    /// When config was last read: the required-state reads come one interval
    /// after any read.
    reads: Schedule,
    /// A config read that follows a check is under way.
    after_check: bool,
}

impl Enforcer {
    /// The decisions for the running `version`.
    pub fn new(version: &str) -> Self {
        let running = Version::parse(version).unwrap_or_else(|e| {
            log::error!("the running version {version:?} isn't SemVer ({e}); taking it as 0.0.0");
            Version::new(0, 0, 0)
        });
        Self {
            running,
            inner: Mutex::new(Inner {
                reads: Schedule::every(REQUIRED_READ_INTERVAL),
                after_check: false,
            }),
        }
    }

    /// After every update check (DST-R51): reads config, then, while update
    /// required, shows the text this check picks (DST-R57), and exits headless
    /// once an update is waiting (DST-R58).
    pub fn after_check<E: Env>(&self, env: &E, checked: &Checked, now: SystemTime) {
        {
            let mut inner = self.lock();
            inner.after_check = true;
            inner.reads.restart(now);
        }
        self.read(env);
        self.lock().after_check = false;
        if !env.update_required() {
            return;
        }
        let waiting = env.update_waiting();
        if let Some(text) = pick(checked, waiting, env.failed_verifications()) {
            env.show(text);
        }
        if waiting {
            env.exit_for_update();
        }
    }

    /// At each update tick (DST-R51): while update required, reads config
    /// once the required-state config interval has passed (DST-R56).
    pub fn tick<E: Env>(&self, env: &E, now: SystemTime) {
        if env.update_required() && self.lock().reads.take_due(now) {
            self.read(env);
        }
    }

    /// Update required was just entered (DST-R56): linking stops, and an
    /// update check runs at once, unless the config read that follows a
    /// check entered it, since that check counts as the one at once (#18).
    pub fn entered<E: Env>(&self, env: &E) {
        env.stop_linking();
        if !self.lock().after_check {
            env.check_soon();
        }
    }

    fn read<E: Env>(&self, env: &E) {
        let Some(config) = env.read_config() else {
            return;
        };
        env.keep_config(&config);
        let below = self
            .running
            .cmp_precedence(&config.min_companion_version)
            .is_lt();
        match (below, env.update_required()) {
            (true, false) => {
                log::warn!(
                    "{} is below the minimum {}: update required",
                    self.running,
                    config.min_companion_version
                );
                env.enter();
            }
            (false, true) => {
                log::info!(
                    "{} is no longer below the minimum {}: update required left",
                    self.running,
                    config.min_companion_version
                );
                env.leave();
            }
            _ => {}
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

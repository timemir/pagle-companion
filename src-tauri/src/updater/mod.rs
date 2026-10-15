//! The updater's first half (DST-R50 to DST-R55): it checks `latest.json` at
//! start and every update interval ([`schedule`]), downloads a higher version
//! ([`release`]) in the background at once, verifies it against the embedded
//! public key, and keeps it **waiting** ([`keep`]). It counts failed
//! verifications in `updater.json` ([`record`]). It shows nothing: no
//! notification and no setting for an ordinary update (DST-R55).
//!
//! It runs in the process that holds the watcher lock, tray or headless
//! ([`start`], from `companion::start_watching`; DST-R51, CMP-R2), and needs
//! no Tauri app, which headless mode doesn't have.
//!
//! Its seams:
//! - [`Hooks::after_check`] hears the outcome of every check, [`Checked`]:
//!   the config read that follows each check (DST-R51, DST-R63), DST-R57's
//!   notification and DST-R58's headless exit hang off it;
//! - [`Updater::check_soon`] checks at the next tick, whenever the last check
//!   was (DST-R56's check at once);
//! - [`Updater::waiting`], [`Updater::verify_waiting`] and
//!   [`Updater::discard_waiting`] are for installing a waiting update
//!   (DST-R54, DST-R13).
//!
//! An update check sends no credential, no Player data and no device
//! identifier (DST-R50, CMP-R6): its own client, without the backend's
//! version header (DST-R60 is `/api/v1`'s), a `User-Agent` or cookies.

pub mod keep;
pub mod record;
pub mod release;
pub mod schedule;

#[cfg(test)]
mod tests;

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use reqwest::header::{ACCEPT, HeaderValue};
use semver::Version;

use crate::api::{CONNECT_TIMEOUT, REQUEST_TIMEOUT};
use crate::host::Host;
use crate::state_dir::StateDir;
use keep::{Keeper, Waiting};
use record::Failures;
use release::{Latest, VerifyError};
use schedule::{Schedule, UPDATE_TICK};

/// The outcome of one update check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    /// `latest.json` couldn't be fetched or read: offline, GitHub down, a
    /// bad document. Tried again at the next interval (DST-R51).
    Failed,
    /// No version above the running one (DST-R52).
    UpToDate,
    /// An update of this version is waiting, verified now or before
    /// (DST-R53).
    Waiting(Version),
    /// The download failed verification and was discarded, the
    /// failed-verification count's `in_a_row` time for this version; fetched
    /// again at the next check (DST-R53, DST-R57).
    NotVerified { in_a_row: u32 },
    /// The download didn't complete or couldn't be kept; fetched again at
    /// the next check.
    NotDownloaded,
    /// Linux without `APPIMAGE` or a writable AppImage folder: a higher
    /// version exists, and is never downloaded (DST-R54).
    CantDownload,
}

/// What the rest of the Companion does after each check.
pub struct Hooks {
    pub after_check: Box<dyn Fn(&Checked) + Send + Sync>,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            after_check: Box::new(|_| {}),
        }
    }
}

/// Where updates come from and the key they must verify against.
pub struct Source {
    endpoint: String,
    pubkey: String,
    platform: String,
    http: reqwest::Client,
}

impl Source {
    /// `endpoint` serves `latest.json`; `pubkey` is base64, as in
    /// `plugins.updater.pubkey`.
    pub fn new(endpoint: impl Into<String>, pubkey: impl Into<String>) -> reqwest::Result<Self> {
        // No default headers: nothing about the machine (CMP-R6).
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            endpoint: endpoint.into(),
            pubkey: pubkey.into(),
            platform: release::platform(),
            http,
        })
    }

    /// The configured endpoint and key (DST-R50, DST-R47).
    pub fn configured() -> reqwest::Result<Self> {
        let config = release::plugin_config();
        Self::new(release::endpoint(&config), config.pubkey)
    }

    async fn latest(&self) -> Result<Latest, String> {
        let reply = self
            .http
            .get(&self.endpoint)
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !reply.status().is_success() {
            return Err(format!("status {}", reply.status()));
        }
        let body = reply.bytes().await.map_err(|e| e.to_string())?;
        release::read_latest(&body, &self.platform).ok_or_else(|| "a bad latest.json".into())
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        let reply = self
            .http
            .get(url)
            .header(ACCEPT, HeaderValue::from_static("application/octet-stream"))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !reply.status().is_success() {
            return Err(format!("status {}", reply.status()));
        }
        Ok(reply.bytes().await.map_err(|e| e.to_string())?.to_vec())
    }
}

/// The process's updater: one per process, checking only once [`start`]ed.
pub struct Updater {
    running: Version,
    source: Source,
    keeper: Keeper,
    failures: Failures,
    waiting: Mutex<Option<Waiting>>,
    schedule: Mutex<Schedule>,
    wake: Sender<()>,
    woken: Mutex<Option<Receiver<()>>>,
    hooks: Hooks,
}

impl Updater {
    /// The updater for the running `version`, with this OS's keeper and
    /// `updater.json` in `state_dir`. Nothing is read or written until it
    /// starts.
    pub fn new(state_dir: &StateDir, version: &str, hooks: Hooks) -> reqwest::Result<Self> {
        Ok(Self::with(
            Source::configured()?,
            version,
            Keeper::platform(state_dir),
            Failures::load(state_dir.updater_file()),
            hooks,
        ))
    }

    pub fn with(
        source: Source,
        version: &str,
        keeper: Keeper,
        failures: Failures,
        hooks: Hooks,
    ) -> Self {
        let running = Version::parse(version).unwrap_or_else(|e| {
            log::error!("the running version {version:?} isn't SemVer ({e}); updating from 0.0.0");
            Version::new(0, 0, 0)
        });
        let (wake, woken) = mpsc::channel();
        Self {
            running,
            source,
            keeper,
            failures,
            waiting: Mutex::new(None),
            schedule: Mutex::new(Schedule::default()),
            wake,
            woken: Mutex::new(Some(woken)),
            hooks,
        }
    }

    /// The update waiting to be installed, if any (DST-R53). Before the
    /// first check, call [`Updater::find_waiting`] to look on disk.
    pub fn waiting(&self) -> Option<Waiting> {
        self.waiting_slot().clone()
    }

    /// Looks for an update kept by an earlier run, deleting a Windows
    /// installer not above the running version (DST-R53). Only for the
    /// process that holds the watcher lock; [`start`] calls it first.
    pub fn find_waiting(&self) -> Option<Waiting> {
        let found = self.keeper.find_waiting(&self.running);
        if let Some(waiting) = &found {
            log::info!("update {} is waiting", waiting.version);
        }
        *self.waiting_slot() = found.clone();
        found
    }

    /// Verifies the waiting update's file again against the public key, as
    /// Windows does just before running the installer (DST-R54).
    pub fn verify_waiting(&self, waiting: &Waiting) -> Result<(), VerifyError> {
        let bytes =
            std::fs::read(&waiting.file).map_err(|e| VerifyError::Malformed(e.to_string()))?;
        release::verify(
            &self.source.pubkey,
            &bytes,
            &waiting.signature,
            &waiting.version,
        )
    }

    /// Forgets the waiting update and deletes a Windows installer: once
    /// installed, or when it fails verification before running, to be
    /// fetched again at the next check (DST-R54).
    pub fn discard_waiting(&self) {
        if let Some(waiting) = self.waiting_slot().take() {
            self.keeper.discard(&waiting);
        }
    }

    /// The failed-verification count in `updater.json` (DST-R57).
    pub fn failed_verifications(&self) -> u32 {
        self.failures.count()
    }

    /// Checks at the next tick, however recent the last check (DST-R56: an
    /// update check at once when update required is entered).
    pub fn check_soon(&self) {
        self.schedule_slot().force();
        let _ = self.wake.send(());
    }

    /// One update tick at wall-clock time `now`: checks if a check is due
    /// (DST-R51), then tells [`Hooks::after_check`]. Blocks while checking
    /// and downloading.
    pub fn tick(&self, now: SystemTime) -> Option<Checked> {
        if !self.schedule_slot().take_due(now) {
            return None;
        }
        let checked = tauri::async_runtime::block_on(self.check());
        (self.hooks.after_check)(&checked);
        Some(checked)
    }

    /// One update check (DST-R51 to DST-R54).
    async fn check(&self) -> Checked {
        let latest = match self.source.latest().await {
            Ok(latest) => latest,
            Err(e) => {
                log::warn!("update check failed: {e}");
                return Checked::Failed;
            }
        };
        self.failures.saw(&latest.version);
        if !release::is_above(&latest.version, &self.running) {
            log::info!("update check: {} is the latest", latest.version);
            return Checked::UpToDate;
        }
        // Nothing is downloaded while the same or a higher version waits.
        if let Some(waiting) = self.waiting()
            && !release::is_above(&latest.version, &waiting.version)
        {
            return Checked::Waiting(waiting.version);
        }
        if !self.keeper.can_keep() {
            log::info!(
                "update {} found; this AppImage can't be replaced, so it isn't downloaded",
                latest.version
            );
            return Checked::CantDownload;
        }
        let Some(asset) = latest.asset else {
            log::warn!("update check failed: latest.json names no file for this platform");
            return Checked::Failed;
        };
        log::info!("downloading update {}", latest.version);
        let bytes = match self.source.download(&asset.url).await {
            Ok(bytes) => bytes,
            Err(e) => {
                log::warn!("couldn't download update {}: {e}", latest.version);
                return Checked::NotDownloaded;
            }
        };
        if let Err(e) = release::verify(
            &self.source.pubkey,
            &bytes,
            &asset.signature,
            &latest.version,
        ) {
            let in_a_row = self.failures.failed(&latest.version);
            log::warn!(
                "update {} failed verification ({in_a_row} in a row) and was discarded: {e}",
                latest.version
            );
            return Checked::NotVerified { in_a_row };
        }
        self.failures.verified();
        match self.keeper.keep(&latest.version, &bytes, &asset.signature) {
            Ok(waiting) => {
                log::info!("update {} verified; it is waiting", waiting.version);
                let version = waiting.version.clone();
                *self.waiting_slot() = Some(waiting);
                Checked::Waiting(version)
            }
            Err(e) => {
                log::error!("couldn't keep update {}: {e}", latest.version);
                Checked::NotDownloaded
            }
        }
    }

    fn waiting_slot(&self) -> MutexGuard<'_, Option<Waiting>> {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn schedule_slot(&self) -> MutexGuard<'_, Schedule> {
        self.schedule.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Starts update checks for this process, which holds the watcher lock
/// (CMP-R2): the update kept by an earlier run, the check at start, then a
/// tick every update tick (DST-R51). A second call does nothing.
pub fn start<H: Host>(host: &H) {
    let Some(updater) = host.try_state::<Updater>() else {
        return;
    };
    let Some(woken) = updater
        .woken
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    else {
        return;
    };
    let host = host.clone();
    std::thread::spawn(move || {
        let updater = host.state::<Updater>();
        updater.find_waiting();
        loop {
            updater.tick(SystemTime::now());
            // The updater keeps the sender, so this only times out or wakes.
            if let Err(RecvTimeoutError::Disconnected) = woken.recv_timeout(UPDATE_TICK) {
                return;
            }
        }
    });
}

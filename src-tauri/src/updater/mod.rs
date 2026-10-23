//! The updater (DST-R50 to DST-R55): it checks `latest.json` at start and
//! every update interval ([`schedule`]), downloads a higher version
//! ([`release`]) in the background at once, verifies it against the embedded
//! public key, and keeps it **waiting** ([`keep`]). It counts failed
//! verifications in `updater.json` ([`record`]). It installs the waiting
//! update when asked, from **Restart to update** or at the next start
//! ([`Updater::install`], [`install`]). It shows nothing: no notification and
//! no setting for an ordinary update (DST-R55).
//!
//! It runs in the process that holds the watcher lock, tray or headless
//! ([`start`], from `companion::start_watching`; DST-R51, CMP-R2), and needs
//! no Tauri app, which headless mode doesn't have.
//!
//! Its seams:
//! - [`Hooks::after_check`] hears the outcome of every check, [`Checked`]:
//!   the config read that follows each check (DST-R51, DST-R63), DST-R57's
//!   notification and DST-R58's headless exit hang off it
//!   (`update_required`); [`Hooks::ticked`] hears every update tick, for the
//!   required-state config reads (DST-R56);
//! - [`Updater::check_soon`] checks at the next tick, whenever the last check
//!   was (DST-R56's check at once);
//! - [`Updater::install`] and [`Updater::install_at_start`] install the
//!   waiting update through an [`install::Launcher`], and say whether the
//!   process must now exit (DST-R54, DST-R13).
//!
//! An update check sends no credential, no Player data and no device
//! identifier (DST-R50, CMP-R6): its own client, without the backend's
//! version header (DST-R60 is `/api/v1`'s), a `User-Agent` or cookies.

pub mod install;
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

use crate::api::{self, CONNECT_TIMEOUT, REQUEST_TIMEOUT};
use crate::host::Host;
use crate::state_dir::StateDir;
use install::Launcher;
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

/// What installing the waiting update came to (DST-R54).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// No update is waiting.
    NothingWaiting,
    /// The new version of this number is starting: the installer, or the
    /// replaced AppImage. The process exits now.
    Started(Version),
    /// The kept installer failed its second verification: deleted, counted
    /// as the failed-verification count's `in_a_row` time, and fetched again
    /// at the next check (DST-R53, DST-R54, DST-R57).
    NotVerified { in_a_row: u32 },
    /// The new version couldn't be started. The update stays waiting.
    CouldntStart,
}

/// The most bytes of `latest.json` the updater reads. The real one is a few
/// kilobytes; a longer one is a bad document (DST-R51).
pub const LATEST_CAP: usize = 1024 * 1024;

/// The most bytes of an update the updater downloads: far above the
/// installer and the AppImage, so that a broken or hostile server can't fill
/// memory before the signature is checked.
pub const DOWNLOAD_CAP: usize = 512 * 1024 * 1024;

/// What the rest of the Companion does after each check.
pub struct Hooks {
    pub after_check: Box<dyn Fn(&Checked) + Send + Sync>,
    /// Every update tick, after its check if one was due.
    pub ticked: Box<dyn Fn(SystemTime) + Send + Sync>,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            after_check: Box::new(|_| {}),
            ticked: Box::new(|_| {}),
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
        // No default headers: nothing about the machine (CMP-R6). GitHub
        // redirects a download to its file host, so redirects are followed,
        // but a release build never leaves HTTPS.
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(REQUEST_TIMEOUT)
            .https_only(!cfg!(debug_assertions))
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
        let body = api::read_capped(reply, LATEST_CAP)
            .await
            .map_err(|e| e.to_string())?;
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
        api::read_capped(reply, DOWNLOAD_CAP)
            .await
            .map_err(|e| e.to_string())
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
            self.keeper.bundle_suffix(),
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

    /// Installs the waiting update (DST-R54): on Windows verifies the kept
    /// installer again against the public key, then runs it passive as an
    /// update that starts the new version with `args` (DST-R13); on Linux
    /// starts the replaced AppImage with `args`. `args` are this process's
    /// arguments without the program. On [`Installed::Started`] the caller
    /// exits.
    ///
    /// A running installer can't be deleted: the new version's start deletes
    /// it, as an installer not above the running version (DST-R53).
    pub fn install(&self, launcher: &dyn Launcher, args: &[String]) -> Installed {
        let Some(waiting) = self.waiting() else {
            return Installed::NothingWaiting;
        };
        let launch = match &self.keeper {
            Keeper::Installer { .. } => {
                if let Err(e) = self.verify_waiting(&waiting) {
                    let in_a_row = self.failures.failed(&waiting.version);
                    log::warn!(
                        "waiting update {} failed verification before it ran \
                         ({in_a_row} in a row) and was deleted: {e}",
                        waiting.version
                    );
                    self.discard_waiting();
                    return Installed::NotVerified { in_a_row };
                }
                install::installer_launch(&waiting.file, &release::plugin_config(), args)
            }
            // Replaced in place once verified: the file every start runs.
            Keeper::AppImage { .. } => install::appimage_launch(&waiting.file, args),
        };
        log::info!(
            "installing update {}: {}",
            waiting.version,
            launch.program.display()
        );
        match launcher.launch(&launch) {
            Ok(()) => Installed::Started(waiting.version),
            Err(e) => {
                log::error!("couldn't start update {}: {e}", waiting.version);
                Installed::CouldntStart
            }
        }
    }

    /// At start, before anything is scanned or uploaded: installs an update
    /// an earlier run kept (DST-R54, DST-R100). Only for the process that
    /// holds the watcher lock, as [`Updater::find_waiting`]. On Linux nothing
    /// waits across a restart: the replaced AppImage is what runs.
    pub fn install_at_start(&self, launcher: &dyn Launcher, args: &[String]) -> Installed {
        if self.find_waiting().is_none() {
            return Installed::NothingWaiting;
        }
        self.install(launcher, args)
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
    /// (DST-R51), then tells [`Hooks::after_check`], then [`Hooks::ticked`].
    /// Blocks while checking and downloading.
    pub fn tick(&self, now: SystemTime) -> Option<Checked> {
        let due = self.schedule_slot().take_due(now);
        let checked = due.then(|| tauri::async_runtime::block_on(self.check()));
        if let Some(checked) = &checked {
            (self.hooks.after_check)(checked);
        }
        (self.hooks.ticked)(now);
        checked
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
        if !release::is_release_file(&self.source.endpoint, &asset.url) {
            log::warn!("update check failed: latest.json names a file outside the Releases");
            return Checked::Failed;
        }
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
            self.keeper.bundle_suffix(),
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

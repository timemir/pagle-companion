//! The real [`Effects`] of removal: what each step does to the machine.
//! Everything that reaches outside the process (the stores, the backend,
//! `systemctl` and the `--quit` child, the folders it deletes) is a field,
//! so tests point it at temp folders, in-memory stores, a stub server and
//! fake commands, and never at this machine's real state.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use super::{Effects, Step};
use crate::api::{self, Api};
use crate::companion;
use crate::credential_store::Keychain;
use crate::in_game::folders;
use crate::installs::{self, DbSource};
use crate::link::{self, REVOKE_TIMEOUT};
use crate::local_log::Logger;
use crate::state::State;
use crate::state_dir::{self, StateDir, WatcherLock};
use crate::tray::LOCK_WAIT;
use crate::xdg;

/// How often the lock wait tries the lock.
const LOCK_POLL: Duration = Duration::from_millis(100);

/// How long a `systemctl` call may take before removal ends it and carries
/// on. Not a spec tunable: it only keeps a hung `systemctl` from holding up
/// the removal wait (DST Tunables).
pub const COMMAND_WAIT: Duration = Duration::from_secs(10);

/// How long step 5 keeps trying to delete the state directory, while the
/// processes that just quit (and on Windows the webview's own processes)
/// let go of files in it.
const DELETE_STATE_DIR_WAIT: Duration = Duration::from_secs(2);

/// The commands removal runs.
pub trait Commands: Send {
    /// Starts `pagle-companion --quit` as a child process (CMP-R70).
    fn start_quit(&mut self) -> Result<(), String>;
    /// Waits for the `--quit` child until `deadline`, then ends it if it
    /// still runs, so no Companion process is left running.
    fn finish_quit(&mut self, deadline: Instant);
    /// Runs `systemctl --user <args>`. `Ok(false)` when there is no
    /// `systemctl` at all.
    fn systemctl(&mut self, args: &[&str]) -> Result<bool, String>;
}

/// The real commands: this executable with `--quit`, and `systemctl`.
pub struct OsCommands {
    exe: Option<PathBuf>,
    quit: Option<Child>,
}

impl OsCommands {
    pub fn new() -> Self {
        Self {
            exe: std::env::current_exe().ok(),
            quit: None,
        }
    }
}

impl Default for OsCommands {
    fn default() -> Self {
        Self::new()
    }
}

impl Commands for OsCommands {
    fn start_quit(&mut self) -> Result<(), String> {
        let exe = self
            .exe
            .as_ref()
            .ok_or("this executable's path is unknown")?;
        let child = Command::new(exe)
            .arg(crate::cli::QUIT)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("couldn't start {} --quit: {e}", exe.display()))?;
        self.quit = Some(child);
        Ok(())
    }

    fn finish_quit(&mut self, deadline: Instant) {
        if let Some(mut child) = self.quit.take() {
            match wait_until(&mut child, deadline) {
                Some(status) => log::info!("--quit exited with {status}"),
                None => log::warn!("--quit was still running; ended it"),
            }
        }
    }

    fn systemctl(&mut self, args: &[&str]) -> Result<bool, String> {
        let spawned = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(format!("couldn't run systemctl: {e}")),
        };
        match wait_until(&mut child, Instant::now() + COMMAND_WAIT) {
            Some(status) if status.success() => Ok(true),
            Some(status) => Err(format!(
                "systemctl --user {} exited with {status}",
                args.join(" ")
            )),
            None => Err(format!("systemctl --user {} took too long", args.join(" "))),
        }
    }
}

/// Waits for `child` until `deadline`, then kills it. `None` if it had to be
/// killed.
fn wait_until(child: &mut Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => sleep(LOCK_POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Deletes the autostart entry (CMP-R70 step 3).
pub type DeleteAutostart = Box<dyn FnMut() -> io::Result<()> + Send>;

/// Stops the tray-mode process's watching and queue (CMP-R71).
pub type StopWatching = Box<dyn FnOnce() + Send>;

/// The machine removal works on.
pub struct System {
    /// The state directory, `None` when this OS user has no config folder.
    pub state_dir: Option<StateDir>,
    /// Local state as removal found it.
    pub state: State,
    pub keychain: Keychain,
    pub api: Option<Api>,
    /// The revoke timeout (CMP Tunables).
    pub revoke_timeout: Duration,
    /// The lock wait (CMP Tunables).
    pub lock_wait: Duration,
    /// `$XDG_CONFIG_HOME`, where the Game Mode unit is (Linux).
    pub config_home: Option<PathBuf>,
    /// The home folder, where the menu entry is (Linux, DST-R31).
    pub home: Option<PathBuf>,
    /// `$APPIMAGE` (Linux).
    pub appimage: Option<PathBuf>,
    /// The `product.db` folders the scan reads (CMP-R10).
    pub db_sources: Vec<DbSource>,
    pub commands: Box<dyn Commands>,
    pub delete_autostart: DeleteAutostart,
    pub stop_watching: Option<StopWatching>,
    /// The watcher lock, once held; let go just before step 5.
    pub lock: Option<WatcherLock>,
    /// The installs whose data addon folders step 4 deletes: those local
    /// state names until the scan adds the ones it finds.
    flavors: Vec<String>,
    /// Whether the Game Mode unit's file was there when removal looked.
    had_unit: bool,
}

impl System {
    /// A machine with nothing outside the process: no state directory, no
    /// backend, no config or home folder, no installs. Fill in the fields
    /// that apply.
    pub fn new(state: State, keychain: Keychain, commands: Box<dyn Commands>) -> Self {
        Self {
            flavors: state.manual_installs.clone(),
            state_dir: None,
            state,
            keychain,
            api: None,
            revoke_timeout: REVOKE_TIMEOUT,
            lock_wait: LOCK_WAIT,
            config_home: None,
            home: None,
            appimage: None,
            db_sources: Vec::new(),
            commands,
            delete_autostart: Box::new(|| Ok(())),
            stop_watching: None,
            lock: None,
            had_unit: false,
        }
    }

    /// This OS user's machine, for `--remove`: the real state directory
    /// (never created if missing), stores, backend, unit, autostart entry and
    /// `product.db`s. The local log file gets every step until step 5.
    pub fn for_command() -> Self {
        let state_dir = state_dir::default_path().ok().map(StateDir::at);
        if let Some(dir) = state_dir.as_ref().filter(|d| d.path().is_dir()) {
            Logger::install(dir.log_file());
        }
        let version = env!("CARGO_PKG_VERSION");
        log::info!("Pagle Companion {version} removing itself (--remove)");
        let state = state_dir
            .as_ref()
            .filter(|d| d.path().is_dir())
            .map(companion::load_state)
            .unwrap_or_default();
        let keychain = platform_keychain(state_dir.as_ref());
        let mut system = Self::new(state, keychain, Box::new(OsCommands::new()));
        system.state_dir = state_dir;
        system.api = Api::new(version, api::base_url())
            .map_err(|e| log::error!("couldn't set up the HTTP client: {e:?}"))
            .ok();
        system.with_this_users_folders()
    }

    /// Points the unit, menu entry, AppImage, autostart entry and scan at
    /// this OS user's own.
    pub fn with_this_users_folders(mut self) -> Self {
        self.config_home = xdg::this_config_home();
        self.home = dirs::home_dir();
        self.appimage = xdg::appimage().map(PathBuf::from);
        self.db_sources = installs::system::db_sources();
        self.delete_autostart = Box::new(crate::autostart::delete_entry);
        self
    }

    /// The installs step 4 deletes the folders of.
    pub fn flavors(&self) -> &[String] {
        &self.flavors
    }

    fn unit_path(&self) -> Result<PathBuf, String> {
        self.config_home
            .as_deref()
            .map(xdg::unit_path)
            .ok_or_else(|| "no config folder".to_owned())
    }

    fn note_unit(&mut self) {
        if let Ok(path) = self.unit_path() {
            self.had_unit |= path.exists();
        }
    }

    /// Runs `systemctl --user <args>`. A missing `systemctl`, or a failure
    /// while there is no unit to act on, is no failure: the unit doesn't
    /// exist yet on most machines (CMP-R90).
    fn systemctl(&mut self, args: &[&str]) -> Result<(), String> {
        self.note_unit();
        match self.commands.systemctl(args) {
            Ok(true) => Ok(()),
            Ok(false) => {
                log::info!("no systemctl here");
                Ok(())
            }
            Err(e) if !self.had_unit => {
                log::info!("no Game Mode unit; {e}");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn wait_for_lock(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + self.lock_wait;
        let result = self.take_lock(deadline);
        self.commands.finish_quit(deadline);
        result
    }

    fn take_lock(&mut self, deadline: Instant) -> Result<(), String> {
        if self.lock.is_some() {
            return Ok(());
        }
        let Some(dir) = self.state_dir.as_ref().filter(|d| d.path().is_dir()) else {
            // No state directory: no process has a lock to let go of.
            return Ok(());
        };
        loop {
            match dir.try_lock_watcher() {
                Ok(Some(lock)) => {
                    self.lock = Some(lock);
                    log::info!("took the watcher lock");
                    return Ok(());
                }
                Ok(None) if Instant::now() < deadline => sleep(LOCK_POLL),
                Ok(None) => return Err("another process still holds the watcher lock".into()),
                Err(e) => return Err(format!("couldn't open watcher.lock: {e}")),
            }
        }
    }

    fn scan(&mut self) -> Result<(), String> {
        let uid = installs::forever_product_uid(&self.state);
        let scan = installs::scan(&self.db_sources, &self.state.manual_installs, &uid);
        for skipped in &scan.skipped {
            log::warn!(
                "couldn't read {}: {}",
                skipped.path.display(),
                skipped.reason
            );
        }
        let found = scan.installs.into_iter().map(|i| i.flavor).collect();
        self.flavors = folders::known_installs(found, self.state.manual_installs.clone());
        log::info!("removal knows {} installs", self.flavors.len());
        Ok(())
    }

    fn self_revoke(&mut self) -> Result<(), String> {
        let credential = self
            .keychain
            .every_store(self.state.credential_store)
            .into_iter()
            .find_map(|store| store.read().ok().flatten());
        let Some(credential) = credential else {
            log::info!("no credential to revoke");
            return Ok(());
        };
        let api = self.api.as_ref().ok_or("no HTTP client")?;
        tauri::async_runtime::block_on(link::self_revoke(api, &credential, self.revoke_timeout))
    }

    fn delete_credential(&mut self) -> Result<(), String> {
        let failures: Vec<String> = self
            .keychain
            .every_store(self.state.credential_store)
            .into_iter()
            .filter_map(|store| {
                store
                    .delete()
                    .err()
                    .map(|e| format!("{}: {e}", store.place()))
            })
            .collect();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    fn delete_unit(&mut self) -> Result<(), String> {
        self.note_unit();
        remove_file(&self.unit_path()?)
    }

    /// Step 5. The local log file says so first: it can't log its own
    /// deletion. Then the lock is let go and the log closed, so nothing in
    /// the folder is held open.
    fn delete_state_dir(&mut self) -> Result<(), String> {
        let dir = self.state_dir.as_ref().ok_or("no config folder")?.clone();
        log::info!(
            "deleting {}; the local log file ends here",
            dir.path().display()
        );
        self.lock = None;
        Logger::close();
        let deadline = Instant::now() + DELETE_STATE_DIR_WAIT;
        loop {
            match fs::remove_dir_all(dir.path()) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(_) if Instant::now() < deadline => sleep(LOCK_POLL),
                Err(e) => return Err(format!("couldn't delete {}: {e}", dir.path().display())),
            }
        }
    }

    fn delete_menu_entry(&mut self) -> Result<(), String> {
        let home = self.home.as_deref().ok_or("no home folder")?;
        let entry = remove_file(&xdg::menu_entry_path(home));
        let icon = remove_file(&xdg::menu_icon_path(home));
        entry.and(icon)
    }

    fn delete_appimage(&mut self) -> Result<(), String> {
        match &self.appimage {
            Some(path) => remove_file(path),
            None => Ok(()),
        }
    }
}

impl Effects for System {
    fn run(&mut self, step: Step) -> Result<(), String> {
        match step {
            Step::StopWatching => {
                if let Some(stop) = self.stop_watching.take() {
                    stop();
                }
                Ok(())
            }
            Step::QuitOthers => self.commands.start_quit(),
            Step::DisableUnit => self.systemctl(&["disable", "--now", xdg::UNIT_NAME]),
            Step::WaitForLock => self.wait_for_lock(),
            Step::Scan => self.scan(),
            Step::SelfRevoke => self.self_revoke(),
            Step::DeleteCredential => self.delete_credential(),
            Step::DeleteUnit => self.delete_unit(),
            Step::DaemonReload => self.systemctl(&["daemon-reload"]),
            Step::DeleteAutostart => {
                (self.delete_autostart)().map_err(|e| format!("couldn't delete it: {e}"))
            }
            Step::DeleteDataAddonFolders => {
                folders::delete_data_addon_folders(&self.flavors);
                Ok(())
            }
            Step::DeleteStateDir => self.delete_state_dir(),
            Step::DeleteMenuEntry => self.delete_menu_entry(),
            Step::DeleteAppImage => self.delete_appimage(),
        }
    }
}

/// Deletes the file at `path`; a missing one is already gone.
fn remove_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("couldn't delete {}: {e}", path.display())),
    }
}

/// This platform's stores (CMP Contracts, Credential store entries): every
/// one the credential may be in. Without a state directory there is no
/// credential file to look in.
pub fn platform_keychain(state_dir: Option<&StateDir>) -> Keychain {
    #[cfg(windows)]
    {
        let _ = state_dir;
        Keychain::new(
            Box::new(crate::credential_store::windows::CredentialManager::new()),
            None,
        )
    }
    #[cfg(target_os = "linux")]
    {
        use crate::credential_store::{FileStore, secret_service::SecretService};
        Keychain::new(
            Box::new(SecretService::new()),
            state_dir.map(|dir| {
                Box::new(FileStore::in_state_dir(dir))
                    as Box<dyn crate::credential_store::CredentialStore>
            }),
        )
    }
}

#[cfg(test)]
mod tests;

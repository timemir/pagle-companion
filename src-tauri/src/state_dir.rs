//! The state directory (CMP Data, State directory) and the watcher lock in it
//! (CMP-R2).

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

const DIR_NAME: &str = "pagle-companion";

/// `%APPDATA%\pagle-companion\` on Windows, `$XDG_CONFIG_HOME/pagle-companion/`
/// (by default `~/.config/pagle-companion/`) on Linux.
pub fn default_path() -> io::Result<PathBuf> {
    dirs::config_dir()
        .map(|base| base.join(DIR_NAME))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config directory"))
}

/// The Companion's own folder: local state, the lock, the local log file and
/// the webview's data all live here, so removal deletes them in one go
/// (CMP-R1, CMP-R70).
#[derive(Debug, Clone)]
pub struct StateDir {
    path: PathBuf,
}

impl StateDir {
    /// Opens the state directory at `path`, creating it (mode `0700` on Linux)
    /// if it is missing.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        create_private_dir(&path)?;
        Ok(Self { path })
    }

    /// The state directory at `path`, as it is: nothing is created. Removal
    /// uses it (CMP-R70), so a missing directory stays missing.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn state_file(&self) -> PathBuf {
        self.path.join("state.json")
    }

    pub fn log_file(&self) -> PathBuf {
        self.path.join("companion.log")
    }

    /// The updater's failed-verification count (DST-R57, CMP Data).
    pub fn updater_file(&self) -> PathBuf {
        self.path.join("updater.json")
    }

    /// Where the webview keeps its data, never Tauri's default folders (CMP-R1).
    pub fn webview_dir(&self) -> PathBuf {
        self.path.join("webview")
    }

    /// Takes the exclusive lock on `watcher.lock` without waiting. `Ok(None)`
    /// means another process holds it (CMP-R2).
    pub fn try_lock_watcher(&self) -> io::Result<Option<WatcherLock>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.join("watcher.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(WatcherLock { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}

/// Held by the one process per OS user that finds installs, watches, uploads
/// and writes in-game files (CMP-R2). Dropping it releases the lock.
#[derive(Debug)]
pub struct WatcherLock {
    _file: File,
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !path.is_dir() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    // A folder made earlier by something else is made private too.
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_path_ends_in_pagle_companion_under_the_config_folder() {
        // CMP Data, State directory: %APPDATA%\pagle-companion\ or
        // $XDG_CONFIG_HOME/pagle-companion/
        let path = default_path().unwrap();
        assert_eq!(path.file_name().unwrap(), "pagle-companion");
        assert_eq!(path.parent().unwrap(), dirs::config_dir().unwrap());
    }

    #[test]
    fn opening_creates_the_folder_and_names_its_files() {
        // CMP Data, State directory: state.json, watcher.lock, companion.log;
        // CMP-R1: webview/ inside it
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::open(tmp.path().join("pagle-companion")).unwrap();
        assert!(dir.path().is_dir());
        assert_eq!(dir.state_file(), dir.path().join("state.json"));
        assert_eq!(dir.log_file(), dir.path().join("companion.log"));
        assert_eq!(dir.webview_dir(), dir.path().join("webview"));
        // CMP Data, DST-R57: the updater's own updater.json
        assert_eq!(dir.updater_file(), dir.path().join("updater.json"));
    }

    #[cfg(unix)]
    #[test]
    fn the_folder_is_private_on_linux() {
        // CMP Data, State directory: mode 0700
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::open(tmp.path().join("pagle-companion")).unwrap();
        let mode = fs::metadata(dir.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn only_one_holder_of_the_watcher_lock_at_a_time() {
        // CMP-R2: only the process holding an exclusive lock on watcher.lock
        // watches. Two handles in one process stand in for two processes:
        // the OS lock is per open file.
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::open(tmp.path()).unwrap();
        let first = dir.try_lock_watcher().unwrap();
        assert!(first.is_some());
        assert!(dir.try_lock_watcher().unwrap().is_none());
        drop(first);
        assert!(dir.try_lock_watcher().unwrap().is_some());
    }
}

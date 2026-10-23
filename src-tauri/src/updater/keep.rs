//! Where a verified update waits until it runs (DST-R53, DST-R54's second
//! paragraph):
//!
//! - **Windows:** the installer, kept in the state directory (CMP Data) as
//!   `pagle-companion_<version>_x64-setup.exe` with its signature beside it
//!   as `….sig`, so it can be verified again just before it runs. A newer
//!   one replaces it; one whose version isn't above the running one is
//!   deleted.
//! - **Linux:** the AppImage at `$APPIMAGE`, replaced as soon as the update
//!   is verified: written beside it, then renamed over it, so a kill
//!   mid-write leaves the old file whole. The running process keeps its old
//!   file open and runs on; the new one takes effect at the next start. Without
//!   `APPIMAGE`, or when its folder isn't writable, nothing is downloaded.
//!
//! Both are plain file code, compiled and tested on every OS; [`Keeper::
//! platform`] picks this OS's.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use semver::Version;

use super::release::is_above;
use crate::state_dir::StateDir;

const INSTALLER_PREFIX: &str = "pagle-companion_";
const INSTALLER_SUFFIX: &str = "_x64-setup.exe";
const SIGNATURE_SUFFIX: &str = ".sig";
/// An installer or signature being written, renamed into place once whole.
const PARTIAL_SUFFIX: &str = ".partial";

/// A verified update that hasn't run yet (DST-R53).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    pub version: Version,
    /// Windows: the kept installer. Linux: `$APPIMAGE`, already replaced.
    pub file: PathBuf,
    /// The signature the file verified against, base64 as in `latest.json`:
    /// Windows verifies the installer with it again before running it
    /// (DST-R54).
    pub signature: String,
}

/// Where this OS keeps a waiting update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keeper {
    /// Windows: installers in the state directory.
    Installer { dir: PathBuf },
    /// Linux: the AppImage file at `$APPIMAGE`; `None` when it is unset.
    AppImage { path: Option<PathBuf> },
}

impl Keeper {
    /// This OS's keeper: the state directory on Windows, `$APPIMAGE` on
    /// Linux.
    pub fn platform(state_dir: &StateDir) -> Self {
        if cfg!(windows) {
            Keeper::Installer {
                dir: state_dir.path().to_path_buf(),
            }
        } else {
            Keeper::AppImage {
                path: std::env::var_os("APPIMAGE")
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from),
            }
        }
    }

    /// How the names of this keeper's bundles end (DST Contracts, "Release
    /// assets"): the NSIS installer, or the AppImage. An update must be
    /// signed as one ([`super::release::verify`]).
    pub fn bundle_suffix(&self) -> &'static str {
        match self {
            Keeper::Installer { .. } => "-setup.exe",
            Keeper::AppImage { .. } => ".AppImage",
        }
    }

    /// Whether an update can be downloaded at all: on Linux only with
    /// `APPIMAGE` set and its folder writable (DST-R54).
    pub fn can_keep(&self) -> bool {
        match self {
            Keeper::Installer { .. } => true,
            Keeper::AppImage { path: None } => false,
            Keeper::AppImage { path: Some(path) } => {
                path.is_file() && path.parent().is_some_and(is_writable_dir)
            }
        }
    }

    /// The update waiting from an earlier run: on Windows the highest kept
    /// installer above `running`, everything else of the updater's in the
    /// folder deleted (DST-R53). On Linux nothing: a replaced AppImage is
    /// what runs after a restart.
    ///
    /// Only the process that holds the watcher lock calls it, so no other
    /// process is writing an installer meanwhile (CMP-R2).
    pub fn find_waiting(&self, running: &Version) -> Option<Waiting> {
        let Keeper::Installer { dir } = self else {
            return None;
        };
        let best = kept_installers(dir)
            .into_iter()
            .filter(|(version, _)| is_above(version, running))
            .filter_map(|(version, file)| {
                let signature = fs::read_to_string(signature_path(&file)).ok()?;
                Some(Waiting {
                    version,
                    file,
                    signature,
                })
            })
            .max_by(|a, b| a.version.cmp_precedence(&b.version));
        delete_all_but(dir, best.as_ref());
        best
    }

    /// Keeps the verified `bytes` of `version` as the waiting update,
    /// replacing any older one.
    pub fn keep(&self, version: &Version, bytes: &[u8], signature: &str) -> io::Result<Waiting> {
        match self {
            Keeper::Installer { dir } => {
                let file = dir.join(installer_name(version));
                write_whole(&signature_path(&file), signature.as_bytes())?;
                write_whole(&file, bytes)?;
                let waiting = Waiting {
                    version: version.clone(),
                    file,
                    signature: signature.to_owned(),
                };
                delete_all_but(dir, Some(&waiting));
                Ok(waiting)
            }
            Keeper::AppImage { path: None } => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "APPIMAGE isn't set",
            )),
            Keeper::AppImage { path: Some(path) } => {
                replace(path, bytes)?;
                Ok(Waiting {
                    version: version.clone(),
                    file: path.clone(),
                    signature: signature.to_owned(),
                })
            }
        }
    }

    /// Deletes a waiting installer and its signature: one that failed its
    /// second verification, or once installed (DST-R54). A Linux AppImage
    /// has been replaced already and stays.
    pub fn discard(&self, waiting: &Waiting) {
        if let Keeper::Installer { .. } = self {
            remove_if_there(&waiting.file);
            remove_if_there(&signature_path(&waiting.file));
        }
    }
}

fn installer_name(version: &Version) -> String {
    format!("{INSTALLER_PREFIX}{version}{INSTALLER_SUFFIX}")
}

fn installer_version(name: &str) -> Option<Version> {
    let version = name
        .strip_prefix(INSTALLER_PREFIX)?
        .strip_suffix(INSTALLER_SUFFIX)?;
    Version::parse(version).ok()
}

fn signature_path(installer: &Path) -> PathBuf {
    with_suffix(installer, SIGNATURE_SUFFIX)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The installers kept in `dir`, by version.
fn kept_installers(dir: &Path) -> Vec<(Version, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let version = installer_version(entry.file_name().to_str()?)?;
            Some((version, entry.path()))
        })
        .collect()
}

/// Deletes every installer, signature and partial file of the updater's in
/// `dir` except `keep`'s installer and signature.
fn delete_all_but(dir: &Path, keep: Option<&Waiting>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let kept: Vec<PathBuf> = keep
        .map(|w| vec![w.file.clone(), signature_path(&w.file)])
        .unwrap_or_default();
    for entry in entries.flatten() {
        let ours = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(INSTALLER_PREFIX));
        if ours && !kept.contains(&entry.path()) {
            remove_if_there(&entry.path());
        }
    }
}

fn remove_if_there(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => log::info!("deleted {}", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("couldn't delete {}: {e}", path.display()),
    }
}

/// Writes `bytes` to `path` through a partial file beside it, renamed into
/// place once whole.
fn write_whole(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let partial = with_suffix(path, PARTIAL_SUFFIX);
    let written = write_synced(&partial, bytes).and_then(|()| fs::rename(&partial, path));
    if written.is_err() {
        let _ = fs::remove_file(&partial);
    }
    written
}

/// Writes `bytes` to a new file at `path`, flushed. Whatever is at `path`
/// first, a leftover of a run killed mid-write or a link someone put there,
/// is deleted, never written through.
fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Replaces the AppImage at `target` with `bytes` atomically (DST-R54).
fn replace(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let staged = stage(target, bytes)?;
    let renamed = fs::rename(&staged, target);
    if renamed.is_err() {
        let _ = fs::remove_file(&staged);
    }
    renamed
}

/// Writes the new AppImage beside `target`, with its permissions, and
/// returns where. Until it is renamed over `target`, `target` is untouched;
/// a file left by a run killed here is replaced by the next.
fn stage(target: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let mut staged_name = std::ffi::OsString::from(".");
    staged_name.push(name);
    staged_name.push(".update");
    let staged = target.with_file_name(staged_name);
    let permissions = fs::metadata(target)?.permissions();
    let written =
        write_synced(&staged, bytes).and_then(|()| fs::set_permissions(&staged, permissions));
    match written {
        Ok(()) => Ok(staged),
        Err(e) => {
            let _ = fs::remove_file(&staged);
            Err(e)
        }
    }
}

#[cfg(unix)]
fn is_writable_dir(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a NUL-terminated path that lives across the call.
    dir.is_dir() && unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0
}

#[cfg(not(unix))]
fn is_writable_dir(dir: &Path) -> bool {
    dir.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(version: &str) -> Version {
        Version::parse(version).unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_link_at_a_file_being_written_is_never_written_through() {
        // DST-R54: the partial installer and the staged AppImage are new
        // files; a hard link someone put at their names, sharing another
        // file's data, is deleted, not overwritten
        let tmp = tempfile::tempdir().unwrap();
        let precious = tmp.path().join("precious");
        fs::write(&precious, b"mine").unwrap();

        let state = tmp.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::hard_link(
            &precious,
            state.join("pagle-companion_1.2.0_x64-setup.exe.partial"),
        )
        .unwrap();
        Keeper::Installer { dir: state.clone() }
            .keep(&v("1.2.0"), b"MZ installer 1.2.0", "sig")
            .unwrap();
        assert_eq!(
            fs::read(state.join("pagle-companion_1.2.0_x64-setup.exe")).unwrap(),
            b"MZ installer 1.2.0"
        );

        let apps = tmp.path().join("Applications");
        fs::create_dir(&apps).unwrap();
        let appimage = apps.join("pagle-companion.AppImage");
        fs::write(&appimage, b"AppImage v1").unwrap();
        fs::hard_link(&precious, apps.join(".pagle-companion.AppImage.update")).unwrap();
        Keeper::AppImage {
            path: Some(appimage.clone()),
        }
        .keep(&v("1.2.0"), b"AppImage v2", "sig")
        .unwrap();
        assert_eq!(fs::read(&appimage).unwrap(), b"AppImage v2");

        assert_eq!(fs::read(&precious).unwrap(), b"mine");
    }

    #[test]
    fn the_installer_is_kept_in_the_state_directory_with_its_signature() {
        // DST-R54: on Windows the verified installer is kept in the local
        // state folder (CMP Data)
        let tmp = tempfile::tempdir().unwrap();
        let keeper = Keeper::Installer {
            dir: tmp.path().into(),
        };
        let waiting = keeper.keep(&v("1.2.0"), b"MZ 1.2.0", "c2ln").unwrap();
        assert_eq!(waiting.version, v("1.2.0"));
        assert_eq!(
            waiting.file,
            tmp.path().join("pagle-companion_1.2.0_x64-setup.exe")
        );
        assert_eq!(fs::read(&waiting.file).unwrap(), b"MZ 1.2.0");
        assert_eq!(
            names(tmp.path()),
            [
                "pagle-companion_1.2.0_x64-setup.exe",
                "pagle-companion_1.2.0_x64-setup.exe.sig"
            ]
        );
        assert_eq!(keeper.find_waiting(&v("1.0.0")), Some(waiting));
    }

    #[test]
    fn a_newer_installer_replaces_the_waiting_one() {
        // DST-R53, DST-R54: deleted once replaced by a newer one
        let tmp = tempfile::tempdir().unwrap();
        let keeper = Keeper::Installer {
            dir: tmp.path().into(),
        };
        keeper.keep(&v("1.2.0"), b"MZ 1.2.0", "a").unwrap();
        keeper.keep(&v("1.3.0"), b"MZ 1.3.0", "b").unwrap();
        assert_eq!(
            names(tmp.path()),
            [
                "pagle-companion_1.3.0_x64-setup.exe",
                "pagle-companion_1.3.0_x64-setup.exe.sig"
            ]
        );
    }

    #[test]
    fn a_waiting_installer_not_above_the_running_version_is_deleted() {
        // DST-R53: a waiting Windows installer whose version isn't above the
        // running one is deleted (the Companion was updated another way)
        let tmp = tempfile::tempdir().unwrap();
        let keeper = Keeper::Installer {
            dir: tmp.path().into(),
        };
        keeper.keep(&v("1.2.0"), b"MZ 1.2.0", "a").unwrap();
        fs::write(tmp.path().join("state.json"), "{}").unwrap();

        assert_eq!(keeper.find_waiting(&v("1.2.0")), None);
        assert_eq!(names(tmp.path()), ["state.json"]);
    }

    #[test]
    fn leftovers_of_an_interrupted_write_are_deleted() {
        // DST-R53: only a whole, verified installer is waiting; a partial
        // file, or an installer without its signature, is not
        let tmp = tempfile::tempdir().unwrap();
        let keeper = Keeper::Installer {
            dir: tmp.path().into(),
        };
        fs::write(
            tmp.path()
                .join("pagle-companion_1.3.0_x64-setup.exe.partial"),
            b"MZ",
        )
        .unwrap();
        fs::write(
            tmp.path().join("pagle-companion_1.3.0_x64-setup.exe.sig"),
            "a",
        )
        .unwrap();
        fs::write(
            tmp.path().join("pagle-companion_1.4.0_x64-setup.exe"),
            b"MZ",
        )
        .unwrap();
        assert_eq!(keeper.find_waiting(&v("1.0.0")), None);
        assert!(names(tmp.path()).is_empty());
    }

    #[test]
    fn of_two_kept_installers_the_higher_waits() {
        // DST-R53: nothing older than the waiting update is kept
        let tmp = tempfile::tempdir().unwrap();
        let keeper = Keeper::Installer {
            dir: tmp.path().into(),
        };
        for version in ["1.3.0", "1.2.0"] {
            let file = tmp.path().join(installer_name(&v(version)));
            fs::write(&file, b"MZ").unwrap();
            fs::write(signature_path(&file), version).unwrap();
        }
        let waiting = keeper.find_waiting(&v("1.0.0")).unwrap();
        assert_eq!(waiting.version, v("1.3.0"));
        assert_eq!(waiting.signature, "1.3.0");
        assert_eq!(names(tmp.path()).len(), 2);
    }

    #[test]
    fn discarding_deletes_the_installer_and_its_signature() {
        // DST-R54: deleted once installed, or when it fails its second
        // verification ("Install a waiting update…" uses it)
        let tmp = tempfile::tempdir().unwrap();
        let keeper = Keeper::Installer {
            dir: tmp.path().into(),
        };
        let waiting = keeper.keep(&v("1.2.0"), b"MZ", "a").unwrap();
        keeper.discard(&waiting);
        assert!(names(tmp.path()).is_empty());
    }

    #[test]
    fn the_appimage_is_replaced_in_place() {
        // DST-R54: on Linux the verified AppImage replaces the running file
        // as soon as it is verified, keeping its permissions
        let tmp = tempfile::tempdir().unwrap();
        let appimage = tmp.path().join("pagle-companion.AppImage");
        fs::write(&appimage, b"old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&appimage, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let keeper = Keeper::AppImage {
            path: Some(appimage.clone()),
        };
        assert!(keeper.can_keep());

        let waiting = keeper.keep(&v("1.2.0"), b"new", "a").unwrap();
        assert_eq!(waiting.file, appimage);
        assert_eq!(fs::read(&appimage).unwrap(), b"new");
        assert_eq!(names(tmp.path()), ["pagle-companion.AppImage"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&appimage).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        // Nothing waits across a restart: the new file is what runs.
        assert_eq!(keeper.find_waiting(&v("1.0.0")), None);
    }

    #[test]
    fn a_kill_mid_write_leaves_the_old_appimage_whole() {
        // DST-R54, DST-R96: written beside, then renamed over it, so the old
        // or the new AppImage remains. A run killed before the rename leaves
        // the old file, and its staged file is overwritten by the next run.
        let tmp = tempfile::tempdir().unwrap();
        let appimage = tmp.path().join("pagle-companion.AppImage");
        fs::write(&appimage, b"old").unwrap();

        let staged = stage(&appimage, b"half").unwrap();
        assert_eq!(staged.parent(), appimage.parent());
        assert_eq!(fs::read(&appimage).unwrap(), b"old");

        replace(&appimage, b"new").unwrap();
        assert_eq!(fs::read(&appimage).unwrap(), b"new");
        assert!(!staged.exists());
    }

    #[test]
    fn without_appimage_nothing_can_be_kept() {
        // DST-R54: with APPIMAGE unset the Linux updater never downloads
        let keeper = Keeper::AppImage { path: None };
        assert!(!keeper.can_keep());
        assert!(keeper.keep(&v("1.2.0"), b"new", "a").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_folder_can_keep_nothing() {
        // DST-R54: when the AppImage's folder isn't writable the Linux
        // updater never downloads
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ro");
        fs::create_dir(&dir).unwrap();
        let appimage = dir.join("pagle-companion.AppImage");
        fs::write(&appimage, b"old").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

        let keeper = Keeper::AppImage {
            path: Some(appimage),
        };
        let can_keep = keeper.can_keep();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!can_keep);
    }
}

//! Moving out of Downloads (DST-R32): when the AppImage starts from the XDG
//! download directory or any folder below it, a tray-mode start asks "Move
//! Pagle Companion to ~/Applications?" before anything else, first run
//! included (CMP-R100). **Move** puts the file at
//! `~/Applications/pagle-companion.AppImage`; **Keep here** is remembered
//! in `keptAppImagePaths` and that path is never asked about again.
//!
//! Here are the decisions and the move, as plain functions over paths;
//! `window` asks the question and starts the moved copy. Headless mode never
//! asks (CMP-R1), and neither does a second tray-mode start, which hands
//! over to the running one and exits before the question (CMP-R2).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::cli::Mode;
use crate::state::{self, State};

/// The question (DST UI "Move dialog").
pub const QUESTION: &str = "Move Pagle Companion to ~/Applications?";
/// The button that moves the file.
pub const MOVE: &str = "Move";
/// The button that keeps it where it is.
pub const KEEP_HERE: &str = "Keep here";
/// What a failed move shows before the Companion carries on.
pub const MOVE_FAILED: &str =
    "Pagle Companion couldn't move itself. It will keep running from Downloads.";

/// The folder **Move** moves to, under the home folder.
pub const APPLICATIONS: &str = "Applications";
/// The name the moved file gets.
pub const MOVED_NAME: &str = "pagle-companion.AppImage";

/// The XDG download directory: what `xdg-user-dir DOWNLOAD` printed, or
/// `~/Downloads` when that isn't available. `xdg-user-dir` prints the home
/// folder itself for a directory that isn't set (the XDG user-dirs way of
/// saying "disabled"), and that, an empty or a relative answer count as not
/// available too: otherwise every AppImage under the home folder would be
/// "in Downloads".
pub fn download_dir(xdg_user_dir: Option<&str>, home: &Path) -> PathBuf {
    xdg_user_dir
        .map(|out| Path::new(out.trim_end_matches(['\n', '\r'])))
        .filter(|dir| dir.is_absolute() && *dir != home)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join("Downloads"))
}

/// Whether `file` is in `dir` or any folder below it. Both should already be
/// canonical (symbolic links resolved), as `window` makes them.
pub fn is_in(file: &Path, dir: &Path) -> bool {
    file != dir && file.starts_with(dir)
}

/// Whether this start asks DST-R32's question: a tray-mode start (headless
/// mode never asks), from an AppImage (`$APPIMAGE`, here canonical) in the
/// download directory, whose path the Player hasn't kept (`$APPIMAGE` as
/// **Keep here** remembered it).
pub fn asks(
    mode: Mode,
    appimage: Option<&str>,
    canonical: Option<&Path>,
    download_dir: &Path,
    state: &State,
) -> bool {
    let (Mode::Tray { .. }, Some(appimage), Some(canonical)) = (mode, appimage, canonical) else {
        return false;
    };
    is_in(canonical, download_dir) && !state.kept_app_image_paths.iter().any(|p| p == appimage)
}

/// `~/Applications`.
pub fn applications_dir(home: &Path) -> PathBuf {
    home.join(APPLICATIONS)
}

/// **Keep here**: remembers `appimage` in `keptAppImagePaths` and writes
/// local state at once, even during a first run, whose other state stays in
/// memory until step 1 ends (CMP-R100). A failed write keeps the answer in
/// memory, so the first write after step 1 keeps it.
pub fn keep_here(state_file: &Path, state: &mut State, appimage: &str) -> io::Result<()> {
    if !state.kept_app_image_paths.iter().any(|p| p == appimage) {
        state.kept_app_image_paths.push(appimage.to_owned());
    }
    state::save(state_file, state)
}

/// **Move**: creates `to_dir` if needed and moves `from` into it as
/// `pagle-companion.AppImage`, replacing a file of that name, then makes it
/// executable. Across file systems (`rename` fails with "crosses devices")
/// it copies, then deletes the original. Returns the new path.
///
/// `rename` is `std::fs::rename`; tests pass one that fails across devices.
pub fn move_to(
    from: &Path,
    to_dir: &Path,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    fs::create_dir_all(to_dir)?;
    let to = to_dir.join(MOVED_NAME);
    match rename(from, &to) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            copy_then_delete(from, &to)?;
        }
        Err(e) => return Err(e),
    }
    make_executable(&to)?;
    Ok(to)
}

/// The move across file systems: copies to a temporary file beside `to`,
/// renames it over `to` (same folder, so it replaces the old copy in one
/// step), then deletes `from`. A copy that fails leaves nothing behind. An
/// original that can't be deleted stays: the copy is complete, so the move
/// still counts.
fn copy_then_delete(from: &Path, to: &Path) -> io::Result<()> {
    let mut part = to.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let copied = fs::copy(from, &part).and_then(|_| fs::rename(&part, to));
    if let Err(e) = copied {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    if let Err(e) = fs::remove_file(from) {
        log::warn!("moved a copy, but couldn't delete {}: {e}", from.display());
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions)
}

/// Windows has no executable bit (and no AppImage).
#[cfg(not(unix))]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRAY: Mode = Mode::Tray { autostart: false };

    fn home(tmp: &tempfile::TempDir) -> PathBuf {
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join("Downloads/x")).unwrap();
        home
    }

    fn appimage_in(dir: &Path) -> PathBuf {
        let path = dir.join("Pagle-Companion_1.0.0_amd64.AppImage");
        fs::write(&path, b"AppImage v1").unwrap();
        path
    }

    /// `asks` over a path that is already canonical.
    fn asks_for(mode: Mode, appimage: &Path, downloads: &Path, state: &State) -> bool {
        let text = appimage.to_str().unwrap();
        asks(mode, Some(text), Some(appimage), downloads, state)
    }

    fn crosses_devices(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::CrossesDevices))
    }

    #[test]
    fn the_question_and_answers_are_dst_ui() {
        // DST-R32, DST UI "Move dialog": the question, buttons and failure
        // text
        assert_eq!(QUESTION, "Move Pagle Companion to ~/Applications?");
        assert_eq!((MOVE, KEEP_HERE), ("Move", "Keep here"));
        assert_eq!(
            MOVE_FAILED,
            "Pagle Companion couldn't move itself. It will keep running from Downloads."
        );
    }

    #[test]
    fn the_download_dir_is_xdg_user_dirs_else_tilde_downloads() {
        // DST-R32: `xdg-user-dir DOWNLOAD`, or ~/Downloads when that isn't
        // available
        // Absolute on every OS: a temp folder stands for /.
        let root = std::env::temp_dir();
        let home = root.join("home/u");
        let fr = home.join("Téléchargements");
        let printed = format!("{}\n", fr.display());
        assert_eq!(download_dir(Some(&printed), &home), fr);
        let elsewhere = root.join("mnt/data/dl");
        assert_eq!(download_dir(elsewhere.to_str(), &home), elsewhere);
        let fallback = home.join("Downloads");
        assert_eq!(download_dir(None, &home), fallback);
        assert_eq!(download_dir(Some(""), &home), fallback);
        assert_eq!(download_dir(Some("dl\n"), &home), fallback);
    }

    #[test]
    fn an_unset_xdg_download_dir_is_tilde_downloads_not_home() {
        // DST-R32: xdg-user-dir prints $HOME for a directory that isn't set;
        // that isn't a download directory, or the whole home would be one
        let home = std::env::temp_dir().join("home/u");
        let printed = format!("{}\n", home.display());
        assert_eq!(download_dir(Some(&printed), &home), home.join("Downloads"));
    }

    #[test]
    fn in_downloads_means_in_it_or_any_folder_below() {
        // DST-R32: the download directory or any folder below it
        let dl = Path::new("/home/u/Downloads");
        assert!(is_in(Path::new("/home/u/Downloads/p.AppImage"), dl));
        assert!(is_in(Path::new("/home/u/Downloads/x/y/p.AppImage"), dl));
        assert!(!is_in(Path::new("/home/u/Applications/p.AppImage"), dl));
        assert!(!is_in(Path::new("/home/u/Downloads2/p.AppImage"), dl));
        assert!(!is_in(Path::new("/home/u/p.AppImage"), dl));
        assert!(!is_in(dl, dl));
    }

    #[test]
    fn a_tray_start_from_downloads_asks() {
        // DST-R32: a start from the download directory or below asks, first
        // run (no state) and --autostart included (CMP-R100)
        let tmp = tempfile::tempdir().unwrap();
        let home = home(&tmp);
        let dl = home.join("Downloads");
        let state = State::default();
        assert!(asks_for(TRAY, &appimage_in(&dl.join("x")), &dl, &state));
        let autostart = Mode::Tray { autostart: true };
        assert!(asks_for(autostart, &appimage_in(&dl), &dl, &state));
    }

    #[test]
    fn a_start_elsewhere_or_without_appimage_doesnt_ask() {
        // DST-R32: only from Downloads; DST-R31: no $APPIMAGE, a development
        // run
        let tmp = tempfile::tempdir().unwrap();
        let home = home(&tmp);
        let dl = home.join("Downloads");
        let state = State::default();
        let apps = home.join(APPLICATIONS);
        fs::create_dir_all(&apps).unwrap();
        assert!(!asks_for(TRAY, &appimage_in(&apps), &dl, &state));
        assert!(!asks(TRAY, None, None, &dl, &state));
    }

    #[test]
    fn headless_mode_never_asks() {
        // DST-R32: not asked in headless mode (CMP-R1: no questions); the
        // other modes never reach a start either
        let tmp = tempfile::tempdir().unwrap();
        let home = home(&tmp);
        let dl = home.join("Downloads");
        let appimage = appimage_in(&dl);
        for mode in [Mode::Headless, Mode::Remove, Mode::Quit] {
            assert!(!asks_for(mode, &appimage, &dl, &State::default()));
        }
    }

    #[test]
    fn keep_here_is_remembered_for_that_path_only() {
        // DST-R32: Keep here is remembered for that path in local state
        // (keptAppImagePaths) and isn't asked again for it; another
        // Downloads path asks again
        let tmp = tempfile::tempdir().unwrap();
        let home = home(&tmp);
        let dl = home.join("Downloads");
        let kept = appimage_in(&dl);
        let state_file = tmp.path().join("state.json");
        let mut state = State::default();
        keep_here(&state_file, &mut state, kept.to_str().unwrap()).unwrap();
        keep_here(&state_file, &mut state, kept.to_str().unwrap()).unwrap();
        assert_eq!(state.kept_app_image_paths, [kept.to_str().unwrap()]);

        let Ok(state::Loaded::State(saved)) = state::load(&state_file) else {
            panic!("state.json not written");
        };
        assert!(!asks_for(TRAY, &kept, &dl, &saved));
        assert!(asks_for(TRAY, &appimage_in(&dl.join("x")), &dl, &saved));
    }

    #[test]
    fn keep_here_is_written_during_a_first_run_and_nothing_else_is() {
        // CMP-R100: until step 1 ends nothing of local state is written,
        // except DST-R32's answer; the next start is still a first run
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("state.json");
        let mut state = State::default();
        keep_here(&state_file, &mut state, "/home/u/Downloads/p.AppImage").unwrap();
        let Ok(state::Loaded::State(saved)) = state::load(&state_file) else {
            panic!("state.json not written");
        };
        assert!(!saved.first_run_done);
        assert_eq!(
            saved,
            State {
                kept_app_image_paths: vec!["/home/u/Downloads/p.AppImage".into()],
                ..State::default()
            }
        );
    }

    #[test]
    fn move_puts_it_in_applications_as_pagle_companion_appimage() {
        // DST-R32: Move creates ~/Applications if needed and moves the file
        // there as pagle-companion.AppImage, executable
        let tmp = tempfile::tempdir().unwrap();
        let home = home(&tmp);
        let from = appimage_in(&home.join("Downloads/x"));
        let apps = applications_dir(&home);
        assert_eq!(apps, home.join("Applications"));

        let to = move_to(&from, &apps, |a, b| fs::rename(a, b)).unwrap();
        assert_eq!(to, home.join("Applications/pagle-companion.AppImage"));
        assert!(!from.exists());
        assert_eq!(fs::read(&to).unwrap(), b"AppImage v1");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&to).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "not executable: {mode:o}");
        }
    }

    #[test]
    fn move_replaces_an_older_copy() {
        // DST-R32: replacing a file of that name; DST-R101: a second copy
        // started from Downloads replaces the older one in ~/Applications
        let tmp = tempfile::tempdir().unwrap();
        let home = home(&tmp);
        let apps = applications_dir(&home);
        fs::create_dir_all(&apps).unwrap();
        fs::write(apps.join(MOVED_NAME), b"AppImage v0").unwrap();
        let from = appimage_in(&home.join("Downloads"));
        let to = move_to(&from, &apps, |a, b| fs::rename(a, b)).unwrap();
        assert_eq!(fs::read(to).unwrap(), b"AppImage v1");
    }

    #[test]
    fn a_move_across_file_systems_copies_then_deletes() {
        // DST-R32: across file systems, by copying and then deleting the
        // original
        let tmp = tempfile::tempdir().unwrap();
        let home = home(&tmp);
        let apps = applications_dir(&home);
        fs::create_dir_all(&apps).unwrap();
        fs::write(apps.join(MOVED_NAME), b"AppImage v0").unwrap();
        let from = appimage_in(&home.join("Downloads"));

        let to = move_to(&from, &apps, crosses_devices).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"AppImage v1");
        assert!(!from.exists());
        assert_eq!(
            fs::read_dir(&apps).unwrap().count(),
            1,
            "no temporary file left"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&to).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "not executable: {mode:o}");
        }
    }

    #[test]
    fn a_move_that_cant_create_applications_fails_and_keeps_the_file() {
        // DST-R32: a move that fails shows the failure text and carries on
        // from Downloads, so the file stays where it is
        let tmp = tempfile::tempdir().unwrap();
        let home = home(&tmp);
        fs::write(applications_dir(&home), b"a file, not a folder").unwrap();
        let from = appimage_in(&home.join("Downloads"));
        assert!(move_to(&from, &applications_dir(&home), |a, b| fs::rename(a, b)).is_err());
        assert_eq!(fs::read(&from).unwrap(), b"AppImage v1");
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_applications_folder_fails_and_keeps_the_file() {
        // DST-R32: a read-only ~/Applications fails the move, which carries
        // on from Downloads; across file systems too, leaving no part file
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let home = home(&tmp);
        let apps = applications_dir(&home);
        fs::create_dir_all(&apps).unwrap();
        fs::set_permissions(&apps, fs::Permissions::from_mode(0o555)).unwrap();
        let from = appimage_in(&home.join("Downloads"));
        // Root ignores permissions; nothing to check then.
        let probe = apps.join("probe");
        if fs::write(&probe, b"").is_ok() {
            let _ = fs::remove_file(probe);
            return;
        }

        assert!(move_to(&from, &apps, |a, b| fs::rename(a, b)).is_err());
        assert!(move_to(&from, &apps, crosses_devices).is_err());
        assert_eq!(fs::read(&from).unwrap(), b"AppImage v1");
        assert_eq!(fs::read_dir(&apps).unwrap().count(), 0);
        fs::set_permissions(&apps, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_failed_copy_leaves_nothing_behind() {
        // DST-R32: a move that fails carries on from Downloads; nothing
        // half-copied is left in ~/Applications
        let tmp = tempfile::tempdir().unwrap();
        let home = home(&tmp);
        let apps = applications_dir(&home);
        let missing = home.join("Downloads/gone.AppImage");
        assert!(move_to(&missing, &apps, crosses_devices).is_err());
        assert_eq!(fs::read_dir(&apps).unwrap().count(), 0);
    }
}

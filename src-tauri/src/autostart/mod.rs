//! Start at login (CMP-R3): the entry that starts the Companion with
//! `--autostart` when the Player logs in. On Windows it is the value
//! `tauri-plugin-autostart` would write under the current user's `Run` key
//! (see [`windows`]); on Linux the XDG autostart entry
//! `$XDG_CONFIG_HOME/autostart/app.pagle.companion.desktop` (CMP Contracts,
//! "Linux autostart entry").
//!
//! Both sit behind [`Entry`], which deals in the command line the entry
//! starts, so [`Autostart`]'s decisions (create, keep, rewrite a changed
//! path, delete) are tested over temp folders and stand-ins.
//!
//! Who calls what:
//! - every tray-mode start: [`Autostart::at_start`] (CMP-R3, CMP-R91);
//! - the end of first run's step 1: `window::create` (CMP-R100);
//! - the window's "Start at login" toggle: [`Autostart::set`];
//! - removal: [`delete_entry`] (CMP-R70 step 3).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::state::{State, Store};
use crate::xdg;

pub mod window;
#[cfg(windows)]
pub mod windows;

/// The argument the entry starts the Companion with (CMP Contracts, Command
/// line).
pub const ARG: &str = "--autostart";

/// The Linux entry's file name, in `$XDG_CONFIG_HOME/autostart/`.
pub const DESKTOP_FILE: &str = "app.pagle.companion.desktop";

/// Where an entry is kept. It holds the command line the entry starts.
pub trait Entry {
    /// The command the entry starts, or `None` when there is no entry.
    fn read(&self) -> io::Result<Option<String>>;
    /// Creates the entry, or replaces it, so it starts `command`.
    fn write(&self, command: &str) -> io::Result<()>;
    /// Deletes the entry; deleting a missing one succeeds.
    fn delete(&self) -> io::Result<()>;
}

/// The command an entry starts: the program's path, already quoted, then
/// `--autostart`.
pub fn command(quoted_path: &str) -> String {
    format!("{quoted_path} {ARG}")
}

/// The Windows entry's command: `"<install path>\pagle-companion.exe"
/// --autostart`. A Windows path can't hold a `"`, so quoting is only the
/// quotes.
pub fn windows_command(exe: &Path) -> String {
    command(&format!("\"{}\"", exe.display()))
}

/// The Linux entry's `Exec`: `$APPIMAGE` quoted as DST-R31 quotes it, then
/// `--autostart` (CMP Contracts, "Linux autostart entry").
pub fn linux_command(appimage: &str) -> String {
    command(&xdg::quote_exec(appimage))
}

/// The Linux autostart entry starting `command`, exactly as CMP Contracts
/// gives it.
pub fn desktop_entry(command: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Pagle Companion\n\
         Exec={command}\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

/// The `Exec` value of a desktop entry, if it has one.
pub fn exec_of(desktop_entry: &str) -> Option<&str> {
    desktop_entry
        .lines()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("Exec="))
}

/// `<config home>/autostart/app.pagle.companion.desktop` (CMP-R3).
pub fn desktop_file_path(config_home: &Path) -> PathBuf {
    config_home.join("autostart").join(DESKTOP_FILE)
}

/// The Linux autostart entry, a desktop file.
#[derive(Debug, Clone)]
pub struct DesktopFile {
    path: PathBuf,
}

impl DesktopFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Entry for DesktopFile {
    /// A file without `Exec` reads as an empty command, so it is rewritten.
    fn read(&self) -> io::Result<Option<String>> {
        match fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(exec_of(&text).unwrap_or_default().to_owned())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Writes the whole entry as CMP Contracts gives it, through a temporary
    /// file, so a session that starts meanwhile never reads half of one.
    fn write(&self, command: &str) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("desktop.tmp");
        fs::write(&tmp, desktop_entry(command))?;
        fs::rename(&tmp, &self.path)
    }

    fn delete(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// What a call did to the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Done {
    /// There was none; now there is.
    Created,
    /// It started another path (or wasn't readable as one); now it starts
    /// this one (CMP-R91).
    Rewrote,
    /// It already started this process's path.
    Unchanged,
    /// Left as it was, or none created: this process doesn't know the path
    /// to start (Linux without `$APPIMAGE`, CMP Contracts).
    NoPath,
    /// Nothing to do: the toggle is off, or a first run hasn't ended step 1.
    Off,
    /// The entry was deleted.
    Deleted,
}

/// The entry of this process: where it is kept and the command it should
/// start (`None` when that isn't known).
pub struct Autostart<E> {
    entry: E,
    command: Option<String>,
}

impl<E: Entry> Autostart<E> {
    pub fn new(entry: E, command: Option<String>) -> Self {
        Self { entry, command }
    }

    pub fn entry(&self) -> &E {
        &self.entry
    }

    /// At every tray-mode start: while the toggle is on, recreates a missing
    /// entry and rewrites one whose path changed (CMP-R3, CMP-R91). A first
    /// run leaves it to the end of step 1 (CMP-R100); with the toggle off
    /// nothing is created.
    pub fn at_start(&self, state: &State) -> io::Result<Done> {
        if !state.first_run_done {
            return Ok(Done::Off);
        }
        self.create(state)
    }

    /// Creates the entry while the toggle is on, or brings it up to date
    /// (CMP-R3). First run calls this at the end of step 1 (CMP-R100).
    pub fn create(&self, state: &State) -> io::Result<Done> {
        if !state.settings.start_at_login {
            return Ok(Done::Off);
        }
        self.keep_current()
    }

    /// Deletes the entry (CMP-R3 toggle off, CMP-R70 step 3).
    pub fn delete(&self) -> io::Result<Done> {
        self.entry.delete()?;
        Ok(Done::Deleted)
    }

    /// The window's "Start at login" toggle (CMP-R3): on creates the entry,
    /// off deletes it, and then `settings.startAtLogin` is kept. If the entry
    /// can't be changed, the setting stays as it was. Before a first run
    /// ends step 1, turning it on only sets the setting: step 1's end creates
    /// the entry (CMP-R100).
    pub fn set(&self, store: &Store, on: bool) -> io::Result<Done> {
        let done = if !on {
            self.delete()?
        } else if store.read(|s| s.first_run_done) {
            self.keep_current()?
        } else {
            Done::Off
        };
        if let Err(e) = store.update(|s| s.settings.start_at_login = on) {
            // The setting is kept in memory and written with the next change.
            log::error!("couldn't write state.json: {e}");
        }
        Ok(done)
    }

    fn keep_current(&self) -> io::Result<Done> {
        let Some(command) = &self.command else {
            return Ok(Done::NoPath);
        };
        match self.entry.read()? {
            Some(current) if current == *command => Ok(Done::Unchanged),
            current => {
                self.entry.write(command)?;
                Ok(if current.is_none() {
                    Done::Created
                } else {
                    Done::Rewrote
                })
            }
        }
    }
}

/// This OS's kind of entry.
#[cfg(windows)]
pub type PlatformEntry = windows::RunValue;
/// This OS's kind of entry.
#[cfg(not(windows))]
pub type PlatformEntry = DesktopFile;

/// The entry of this process: on Windows the `Run` value starting this
/// executable; on Linux the XDG autostart entry starting `$APPIMAGE`, or no
/// command without it.
pub fn for_this_process() -> io::Result<Autostart<PlatformEntry>> {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe()?;
        let exe = dunce::simplified(&exe);
        Ok(Autostart::new(
            windows::RunValue::new(),
            Some(windows_command(exe)),
        ))
    }
    #[cfg(not(windows))]
    {
        let config_home = xdg::this_config_home()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config folder"))?;
        Ok(Autostart::new(
            DesktopFile::new(desktop_file_path(&config_home)),
            xdg::appimage().map(|path| linux_command(&path)),
        ))
    }
}

/// Deletes this OS user's autostart entry, for removal (CMP-R70 step 3).
pub fn delete_entry() -> io::Result<()> {
    for_this_process()?.delete()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const APPIMAGE: &str = r#"/home/u/My "Apps"/p.AppImage"#;
    const MOVED: &str = "/home/u/Applications/p.AppImage";

    /// An entry in memory, counting writes.
    #[derive(Default)]
    struct Fake {
        command: RefCell<Option<String>>,
        writes: RefCell<u32>,
    }

    impl Entry for Fake {
        fn read(&self) -> io::Result<Option<String>> {
            Ok(self.command.borrow().clone())
        }
        fn write(&self, command: &str) -> io::Result<()> {
            *self.writes.borrow_mut() += 1;
            *self.command.borrow_mut() = Some(command.to_owned());
            Ok(())
        }
        fn delete(&self) -> io::Result<()> {
            *self.command.borrow_mut() = None;
            Ok(())
        }
    }

    /// An entry that can't be written or deleted.
    struct Broken;

    impl Entry for Broken {
        fn read(&self) -> io::Result<Option<String>> {
            Ok(None)
        }
        fn write(&self, _: &str) -> io::Result<()> {
            Err(io::Error::other("read-only"))
        }
        fn delete(&self) -> io::Result<()> {
            Err(io::Error::other("read-only"))
        }
    }

    fn state(first_run_done: bool, start_at_login: bool) -> State {
        let mut state = State {
            first_run_done,
            ..State::default()
        };
        state.settings.start_at_login = start_at_login;
        state
    }

    fn linked() -> State {
        state(true, true)
    }

    /// The Linux entry in a temp config home, for `$APPIMAGE` (or none).
    fn linux(config_home: &Path, appimage: Option<&str>) -> Autostart<DesktopFile> {
        Autostart::new(
            DesktopFile::new(desktop_file_path(config_home)),
            appimage.map(linux_command),
        )
    }

    fn read_file(autostart: &Autostart<DesktopFile>) -> String {
        fs::read_to_string(autostart.entry().path()).unwrap()
    }

    #[test]
    fn the_linux_entry_is_exactly_the_contract() {
        // CMP Contracts, Linux autostart entry; DST-R31 quoting
        assert_eq!(
            desktop_entry(&linux_command(APPIMAGE)),
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Pagle Companion\n\
             Exec=\"/home/u/My \\\"Apps\\\"/p.AppImage\" --autostart\n\
             X-GNOME-Autostart-enabled=true\n"
        );
    }

    #[test]
    fn the_linux_entry_lives_in_the_autostart_folder_of_the_config_home() {
        // CMP-R3: $XDG_CONFIG_HOME/autostart/app.pagle.companion.desktop
        let home = Path::new("/home/u/.config");
        assert_eq!(
            desktop_file_path(home),
            home.join("autostart").join("app.pagle.companion.desktop")
        );
    }

    #[test]
    fn the_windows_entry_starts_the_quoted_exe_with_autostart() {
        // CMP-R3: the entry starts "<install path>\pagle-companion.exe"
        // --autostart
        let exe =
            Path::new(r"C:\Users\Tim Smith\AppData\Local\Pagle Companion\pagle-companion.exe");
        assert_eq!(
            windows_command(exe),
            r#""C:\Users\Tim Smith\AppData\Local\Pagle Companion\pagle-companion.exe" --autostart"#
        );
    }

    #[test]
    fn the_exec_line_is_read_back_from_an_entry() {
        // CMP-R91: the entry's path is compared with $APPIMAGE
        let text = desktop_entry(&linux_command(APPIMAGE));
        assert_eq!(exec_of(&text), Some(linux_command(APPIMAGE).as_str()));
        assert_eq!(
            exec_of("[Desktop Entry]\r\nExec=x --autostart\r\n"),
            Some("x --autostart")
        );
        assert_eq!(exec_of("[Desktop Entry]\n"), None);
    }

    #[test]
    fn a_tray_start_creates_a_missing_entry_while_the_toggle_is_on() {
        // CMP-R3: at every tray-mode start, recreates the entry if it is
        // missing while the toggle is on
        let tmp = tempfile::tempdir().unwrap();
        let autostart = linux(tmp.path(), Some(APPIMAGE));
        assert_eq!(autostart.at_start(&linked()).unwrap(), Done::Created);
        assert_eq!(
            read_file(&autostart),
            desktop_entry(r#""/home/u/My \"Apps\"/p.AppImage" --autostart"#)
        );
        assert!(read_file(&autostart).contains("X-GNOME-Autostart-enabled=true\n"));
    }

    #[test]
    fn a_tray_start_from_a_new_appimage_path_rewrites_exec() {
        // CMP-R91: the autostart entry's path is kept current; CMP-R3
        let tmp = tempfile::tempdir().unwrap();
        linux(tmp.path(), Some(APPIMAGE))
            .at_start(&linked())
            .unwrap();
        let moved = linux(tmp.path(), Some(MOVED));
        assert_eq!(moved.at_start(&linked()).unwrap(), Done::Rewrote);
        assert_eq!(read_file(&moved), desktop_entry(&linux_command(MOVED)));
    }

    #[test]
    fn a_tray_start_leaves_a_current_entry_alone() {
        // CMP-R3: only a missing entry or a changed path is written
        let tmp = tempfile::tempdir().unwrap();
        let autostart = linux(tmp.path(), Some(APPIMAGE));
        let own = format!("{}Comment=kept\n", desktop_entry(&linux_command(APPIMAGE)));
        fs::create_dir_all(autostart.entry().path().parent().unwrap()).unwrap();
        fs::write(autostart.entry().path(), &own).unwrap();
        assert_eq!(autostart.at_start(&linked()).unwrap(), Done::Unchanged);
        assert_eq!(read_file(&autostart), own);

        let fake = Autostart::new(Fake::default(), Some("p --autostart".into()));
        fake.at_start(&linked()).unwrap();
        fake.at_start(&linked()).unwrap();
        assert_eq!(*fake.entry().writes.borrow(), 1);
    }

    #[test]
    fn an_entry_without_exec_is_rewritten() {
        // CMP-R3, CMP Contracts: the entry is the contract's, starting
        // $APPIMAGE
        let tmp = tempfile::tempdir().unwrap();
        let autostart = linux(tmp.path(), Some(APPIMAGE));
        fs::create_dir_all(autostart.entry().path().parent().unwrap()).unwrap();
        fs::write(autostart.entry().path(), "[Desktop Entry]\n").unwrap();
        assert_eq!(autostart.at_start(&linked()).unwrap(), Done::Rewrote);
        assert_eq!(
            read_file(&autostart),
            desktop_entry(&linux_command(APPIMAGE))
        );
    }

    #[test]
    fn with_the_toggle_off_a_tray_start_creates_nothing() {
        // CMP-R3: recreated only while the toggle is on
        let tmp = tempfile::tempdir().unwrap();
        let autostart = linux(tmp.path(), Some(APPIMAGE));
        assert_eq!(autostart.at_start(&state(true, false)).unwrap(), Done::Off);
        assert!(!autostart.entry().path().exists());
    }

    #[test]
    fn a_first_run_start_leaves_the_entry_to_the_end_of_step_1() {
        // CMP-R100: the entry is created when step 1 ends; CMP-R3
        let tmp = tempfile::tempdir().unwrap();
        let autostart = linux(tmp.path(), Some(APPIMAGE));
        assert_eq!(autostart.at_start(&state(false, true)).unwrap(), Done::Off);
        assert!(!autostart.entry().path().exists());
        assert_eq!(autostart.create(&state(true, true)).unwrap(), Done::Created);
        assert!(autostart.entry().path().exists());
    }

    #[test]
    fn without_appimage_an_existing_entry_is_untouched_and_none_is_created() {
        // CMP Contracts, Linux autostart entry: without $APPIMAGE the
        // Companion leaves an existing entry as it is and creates none
        let tmp = tempfile::tempdir().unwrap();
        let dev = linux(tmp.path(), None);
        assert_eq!(dev.at_start(&linked()).unwrap(), Done::NoPath);
        assert_eq!(dev.create(&linked()).unwrap(), Done::NoPath);
        assert!(!dev.entry().path().exists());

        linux(tmp.path(), Some(APPIMAGE)).create(&linked()).unwrap();
        let before = read_file(&dev);
        assert_eq!(dev.at_start(&linked()).unwrap(), Done::NoPath);
        assert_eq!(read_file(&dev), before);
    }

    #[test]
    fn creating_respects_the_toggle() {
        // CMP-R3: the entry exists while the toggle is on
        let fake = Autostart::new(Fake::default(), Some("p --autostart".into()));
        assert_eq!(fake.create(&state(true, false)).unwrap(), Done::Off);
        assert_eq!(fake.entry().read().unwrap(), None);
    }

    #[test]
    fn turning_the_toggle_off_deletes_the_entry_and_keeps_the_setting() {
        // CMP-R3: turning the toggle off deletes the entry; CMP Data:
        // settings.startAtLogin
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("state.json"), linked());
        let autostart = linux(&tmp.path().join("config"), Some(APPIMAGE));
        autostart.at_start(&linked()).unwrap();

        assert_eq!(autostart.set(&store, false).unwrap(), Done::Deleted);
        assert!(!autostart.entry().path().exists());
        let saved = crate::state::load(&tmp.path().join("state.json")).unwrap();
        let crate::state::Loaded::State(saved) = saved else {
            panic!("state.json not written")
        };
        assert!(!saved.settings.start_at_login);

        assert_eq!(autostart.set(&store, true).unwrap(), Done::Created);
        assert!(autostart.entry().path().exists());
        assert!(store.read(|s| s.settings.start_at_login));
    }

    #[test]
    fn turning_it_off_twice_is_fine() {
        // CMP-R3: deleting a missing entry succeeds
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("state.json"), linked());
        let autostart = linux(tmp.path(), Some(APPIMAGE));
        assert_eq!(autostart.set(&store, false).unwrap(), Done::Deleted);
        assert_eq!(autostart.set(&store, false).unwrap(), Done::Deleted);
    }

    #[test]
    fn a_toggle_that_cant_change_the_entry_keeps_the_setting() {
        // CMP-R3: the toggle shows whether the entry is kept
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("state.json"), linked());
        let broken = Autostart::new(Broken, Some("p --autostart".into()));
        assert!(broken.set(&store, false).is_err());
        assert!(store.read(|s| s.settings.start_at_login));

        store.update(|s| s.settings.start_at_login = false).unwrap();
        assert!(broken.set(&store, true).is_err());
        assert!(!store.read(|s| s.settings.start_at_login));
    }

    #[test]
    fn turning_it_on_during_a_first_run_waits_for_step_1() {
        // CMP-R100: the entry is created at the end of step 1, and nothing
        // is written before it
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("state.json"), state(false, false));
        let autostart = linux(&tmp.path().join("config"), Some(APPIMAGE));
        assert_eq!(autostart.set(&store, true).unwrap(), Done::Off);
        assert!(!autostart.entry().path().exists());
        assert!(!tmp.path().join("state.json").exists());
        assert!(store.read(|s| s.settings.start_at_login));
    }

    #[test]
    fn turning_it_on_without_appimage_keeps_the_setting_and_creates_nothing() {
        // CMP Contracts, Linux autostart entry: without $APPIMAGE none is
        // created; CMP-R3: the toggle is kept
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("state.json"), state(true, false));
        let dev = linux(&tmp.path().join("config"), None);
        assert_eq!(dev.set(&store, true).unwrap(), Done::NoPath);
        assert!(!dev.entry().path().exists());
        assert!(store.read(|s| s.settings.start_at_login));
    }

    #[test]
    fn removal_deletes_the_entry() {
        // CMP-R70 step 3: deletes the autostart entry
        let tmp = tempfile::tempdir().unwrap();
        let autostart = linux(tmp.path(), Some(APPIMAGE));
        autostart.create(&linked()).unwrap();
        assert_eq!(autostart.delete().unwrap(), Done::Deleted);
        assert!(!autostart.entry().path().exists());
        // Without $APPIMAGE too: deleting needs no path.
        linux(tmp.path(), Some(APPIMAGE)).create(&linked()).unwrap();
        linux(tmp.path(), None).delete().unwrap();
        assert!(!autostart.entry().path().exists());
    }
}

//! The menu entry (DST-R31): at every start the AppImage writes its desktop
//! entry, `<data home>/applications/app.pagle.companion.desktop`, as DST
//! Contracts "Desktop entry" gives it, and its 256 × 256 icon (DST
//! Tunables), embedded in the binary, to
//! `<data home>/icons/hicolor/256x256/apps/app.pagle.companion.png`. Each
//! file is written only when it is missing or differs, so a moved AppImage
//! fixes its own `Exec` at its next start and an unchanged one writes
//! nothing. Without `$APPIMAGE` nothing is written.
//!
//! Removal's Linux tray item deletes both (DST-R34) with
//! [`MenuEntry::delete`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::xdg;

/// The icon, 256 × 256 px (DST Tunables): the bundle's `128x128@2x.png`.
pub const ICON: &[u8] = include_bytes!("../../icons/128x128@2x.png");

/// The icon's width and height in pixels (DST Tunables).
pub const ICON_SIZE: u32 = 256;

/// The desktop entry starting `appimage`, exactly as DST Contracts gives it,
/// with `Exec` quoted as DST-R31 says.
pub fn desktop_entry(appimage: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Pagle Companion\n\
         Exec={}\n\
         Icon={}\n\
         Categories=Game;\n\
         Terminal=false\n",
        xdg::quote_exec(appimage),
        xdg::MENU_ENTRY_ID,
    )
}

/// What [`MenuEntry::keep_current`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    /// The desktop entry was missing or differed, and was written.
    pub entry: bool,
    /// The icon was missing or differed, and was written.
    pub icon: bool,
}

/// The menu entry and its icon under one data home.
#[derive(Debug, Clone)]
pub struct MenuEntry {
    data_home: PathBuf,
}

impl MenuEntry {
    /// The menu entry under `data_home` (`$XDG_DATA_HOME`).
    pub fn new(data_home: impl Into<PathBuf>) -> Self {
        Self {
            data_home: data_home.into(),
        }
    }

    /// This OS user's, `None` without a data home or home folder.
    pub fn for_this_user() -> Option<Self> {
        xdg::this_data_home().map(Self::new)
    }

    pub fn entry_path(&self) -> PathBuf {
        xdg::menu_entry_path(&self.data_home)
    }

    pub fn icon_path(&self) -> PathBuf {
        xdg::menu_icon_path(&self.data_home)
    }

    /// DST-R31, at every start: writes the entry for `appimage` and the icon,
    /// each only when missing or different. Without `$APPIMAGE` (`None`),
    /// writes nothing and returns `None`.
    pub fn keep_current(&self, appimage: Option<&str>) -> io::Result<Option<Written>> {
        let Some(appimage) = appimage else {
            return Ok(None);
        };
        // The icon first: an entry is never shown pointing at a missing one.
        let icon = write_if_different(&self.icon_path(), ICON)?;
        let entry = write_if_different(&self.entry_path(), desktop_entry(appimage).as_bytes())?;
        Ok(Some(Written { entry, icon }))
    }

    /// Deletes the entry and the icon (DST-R34). Missing files are fine; a
    /// file that can't be deleted doesn't stop the other from being tried.
    pub fn delete(&self) -> io::Result<()> {
        let entry = remove_file(&self.entry_path());
        let icon = remove_file(&self.icon_path());
        entry.and(icon)
    }
}

/// DST-R31, at every start: keeps this OS user's menu entry current for
/// `$APPIMAGE`, and logs what it did.
pub fn at_start() {
    let Some(menu) = MenuEntry::for_this_user() else {
        log::warn!("no home folder: no menu entry");
        return;
    };
    match menu.keep_current(xdg::appimage().as_deref()) {
        Ok(None) => log::info!("no $APPIMAGE: no menu entry"),
        Ok(Some(Written { entry, icon })) => {
            if entry {
                log::info!("wrote the menu entry");
            }
            if icon {
                log::info!("wrote the menu entry's icon");
            }
        }
        Err(e) => log::error!("couldn't write the menu entry: {e}"),
    }
}

/// Writes `bytes` to `path` unless it already holds exactly them. Writes
/// through a temporary file, so a menu that reads meanwhile never sees half
/// a file. Returns whether it wrote.
fn write_if_different(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    match fs::read(path) {
        Ok(current) if current == bytes => return Ok(false),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(true)
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPIMAGE: &str = "/home/u/Applications/pagle-companion.AppImage";
    const ODD: &str = r#"/home/u/My "Apps" $HOME `x` \ 100%/p.AppImage"#;

    fn menu(tmp: &tempfile::TempDir) -> MenuEntry {
        MenuEntry::new(tmp.path().join("home/.local/share"))
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn the_entry_is_exactly_the_contract() {
        // DST Contracts "Desktop entry"; DST-R31: Exec is $APPIMAGE quoted
        assert_eq!(
            desktop_entry(APPIMAGE),
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Pagle Companion\n\
             Exec=\"/home/u/Applications/pagle-companion.AppImage\"\n\
             Icon=app.pagle.companion\n\
             Categories=Game;\n\
             Terminal=false\n"
        );
    }

    #[test]
    fn an_odd_path_is_quoted_in_exec() {
        // DST-R31: inside double quotes, ", `, $ and \ are escaped with a
        // backslash, and % is written %%
        let entry = desktop_entry(ODD);
        assert!(entry.contains(
            "\nExec=\"/home/u/My \\\"Apps\\\" \\$HOME \\`x\\` \\\\ 100%%/p.AppImage\"\n"
        ));
    }

    #[test]
    fn the_icon_is_a_256_px_square_png() {
        // DST Tunables: desktop entry icon size 256 × 256 px
        assert_eq!(&ICON[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&ICON[12..16], b"IHDR");
        let width = u32::from_be_bytes(ICON[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(ICON[20..24].try_into().unwrap());
        assert_eq!((width, height), (ICON_SIZE, ICON_SIZE));
        assert_eq!(ICON_SIZE, 256);
    }

    #[test]
    fn an_appimage_start_writes_the_entry_and_the_icon() {
        // DST-R31: the desktop entry and the icon, at their paths under the
        // data home
        let tmp = tempfile::tempdir().unwrap();
        let menu = menu(&tmp);
        assert_eq!(
            menu.keep_current(Some(ODD)).unwrap(),
            Some(Written {
                entry: true,
                icon: true
            })
        );
        let share = tmp.path().join("home/.local/share");
        assert_eq!(
            menu.entry_path(),
            share.join("applications/app.pagle.companion.desktop")
        );
        assert_eq!(
            menu.icon_path(),
            share.join("icons/hicolor/256x256/apps/app.pagle.companion.png")
        );
        assert_eq!(read(&menu.entry_path()), desktop_entry(ODD));
        assert_eq!(fs::read(menu.icon_path()).unwrap(), ICON);
        assert!(!menu.entry_path().with_extension("desktop.tmp").exists());
    }

    #[test]
    fn a_start_with_nothing_changed_writes_nothing() {
        // DST-R31: written or rewritten only when anything differs
        let tmp = tempfile::tempdir().unwrap();
        let menu = menu(&tmp);
        menu.keep_current(Some(APPIMAGE)).unwrap();
        // Dated back to 2023: a rewrite would date them now.
        // Whole seconds, which every file system keeps exactly.
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        for path in [menu.entry_path(), menu.icon_path()] {
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_modified(old).unwrap();
        }
        assert_eq!(
            menu.keep_current(Some(APPIMAGE)).unwrap(),
            Some(Written {
                entry: false,
                icon: false
            })
        );
        for path in [menu.entry_path(), menu.icon_path()] {
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), old);
        }
    }

    #[test]
    fn a_moved_appimage_rewrites_exec_only() {
        // DST-R31: the entry follows the path, so a moved AppImage fixes its
        // own entry at its next start
        let tmp = tempfile::tempdir().unwrap();
        let menu = menu(&tmp);
        menu.keep_current(Some("/home/u/Downloads/p.AppImage"))
            .unwrap();
        assert_eq!(
            menu.keep_current(Some(APPIMAGE)).unwrap(),
            Some(Written {
                entry: true,
                icon: false
            })
        );
        assert!(read(&menu.entry_path()).contains(&format!("\nExec=\"{APPIMAGE}\"\n")));
    }

    #[test]
    fn a_changed_entry_or_icon_is_rewritten() {
        // DST-R31: rewritten when anything differs
        let tmp = tempfile::tempdir().unwrap();
        let menu = menu(&tmp);
        menu.keep_current(Some(APPIMAGE)).unwrap();
        fs::write(menu.entry_path(), "[Desktop Entry]\nName=Other\n").unwrap();
        fs::write(menu.icon_path(), b"not a png").unwrap();
        assert_eq!(
            menu.keep_current(Some(APPIMAGE)).unwrap(),
            Some(Written {
                entry: true,
                icon: true
            })
        );
        assert_eq!(read(&menu.entry_path()), desktop_entry(APPIMAGE));
        assert_eq!(fs::read(menu.icon_path()).unwrap(), ICON);
    }

    #[test]
    fn without_appimage_nothing_is_written() {
        // DST-R31: without APPIMAGE (a development run), nothing is written
        let tmp = tempfile::tempdir().unwrap();
        let menu = menu(&tmp);
        assert_eq!(menu.keep_current(None).unwrap(), None);
        assert!(!menu.entry_path().exists());
        assert!(!menu.icon_path().exists());
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn removal_deletes_the_entry_and_the_icon() {
        // DST-R34: Remove deletes what DST-R31 created; deleting twice is
        // fine
        let tmp = tempfile::tempdir().unwrap();
        let menu = menu(&tmp);
        menu.keep_current(Some(APPIMAGE)).unwrap();
        menu.delete().unwrap();
        assert!(!menu.entry_path().exists());
        assert!(!menu.icon_path().exists());
        menu.delete().unwrap();
    }
}

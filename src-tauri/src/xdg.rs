//! The Linux desktop's shared conventions: `$XDG_CONFIG_HOME` (`~/.config`
//! when unset, CMP-R3 "here and below") and the Desktop Entry
//! Specification's quoting of an `Exec` path (DST-R31). The autostart entry
//! (CMP-R3), the menu entry (DST-R31) and the Game Mode unit (CMP Contracts)
//! all point at `$APPIMAGE` quoted this way.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// `$XDG_CONFIG_HOME`, or `~/.config` when it is unset. A relative or empty
/// value counts as unset, as the XDG Base Directory Specification says (and
/// as `dirs::config_dir`, which places the state directory, reads it).
/// `None` when neither it nor a home folder is known.
pub fn config_home(xdg_config_home: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_config_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| home.join(".config")))
}

/// [`config_home`] of this process's environment.
pub fn this_config_home() -> Option<PathBuf> {
    config_home(std::env::var_os("XDG_CONFIG_HOME"), dirs::home_dir())
}

/// The Game Mode unit's name (CMP-R90).
pub const UNIT_NAME: &str = "pagle-companion.service";

/// The Game Mode unit, `$XDG_CONFIG_HOME/systemd/user/pagle-companion.service`
/// (CMP Contracts, Game Mode unit).
pub fn unit_path(config_home: &Path) -> PathBuf {
    config_home.join("systemd").join("user").join(UNIT_NAME)
}

/// The menu entry, `~/.local/share/applications/app.pagle.companion.desktop`
/// (DST-R31, DST Contracts "Desktop entry").
pub fn menu_entry_path(home: &Path) -> PathBuf {
    home.join(".local/share/applications/app.pagle.companion.desktop")
}

/// The menu entry's icon,
/// `~/.local/share/icons/hicolor/256x256/apps/app.pagle.companion.png`
/// (DST-R31).
pub fn menu_icon_path(home: &Path) -> PathBuf {
    home.join(".local/share/icons/hicolor/256x256/apps/app.pagle.companion.png")
}

/// `$APPIMAGE`: the AppImage this process runs from. `None` for a
/// development run or `--appimage-extract-and-run`, and on Windows.
pub fn appimage() -> Option<String> {
    std::env::var("APPIMAGE")
        .ok()
        .filter(|path| !path.is_empty())
}

/// `path` quoted for an `Exec` key as DST-R31 says: inside double quotes,
/// with `"`, `` ` ``, `$` and `\` escaped with a backslash, and `%` written
/// `%%`.
pub fn quote_exec(path: &str) -> String {
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('"');
    for c in path.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_home_is_xdg_config_home_when_set() {
        // CMP-R3: $XDG_CONFIG_HOME/autostart/…
        let cfg = std::env::temp_dir().join("cfg");
        assert_eq!(
            config_home(Some(cfg.clone().into()), Some("/home/u".into())),
            Some(cfg)
        );
    }

    #[test]
    fn config_home_defaults_to_dot_config() {
        // CMP-R3: $XDG_CONFIG_HOME defaults to ~/.config
        assert_eq!(
            config_home(None, Some("/home/u".into())),
            Some(PathBuf::from("/home/u").join(".config"))
        );
    }

    #[test]
    fn an_empty_or_relative_xdg_config_home_counts_as_unset() {
        // CMP-R3: $XDG_CONFIG_HOME defaults to ~/.config; the XDG Base
        // Directory Specification ignores a relative path
        let home = Some(PathBuf::from("/home/u"));
        let dot_config = Some(PathBuf::from("/home/u").join(".config"));
        assert_eq!(config_home(Some("".into()), home.clone()), dot_config);
        assert_eq!(config_home(Some("cfg".into()), home), dot_config);
    }

    #[test]
    fn no_config_home_without_a_home_folder() {
        // CMP-R3: nothing to point at without $XDG_CONFIG_HOME or ~
        assert_eq!(config_home(None, None), None);
    }

    #[test]
    fn the_unit_and_the_menu_entry_are_at_their_contract_paths() {
        // CMP Contracts, Game Mode unit; DST-R31, DST Contracts "Desktop
        // entry"
        let cfg = Path::new("/home/u/.config");
        assert_eq!(
            unit_path(cfg),
            Path::new("/home/u/.config/systemd/user/pagle-companion.service")
        );
        let home = Path::new("/home/u");
        assert_eq!(
            menu_entry_path(home),
            Path::new("/home/u/.local/share/applications/app.pagle.companion.desktop")
        );
        assert_eq!(
            menu_icon_path(home),
            Path::new("/home/u/.local/share/icons/hicolor/256x256/apps/app.pagle.companion.png")
        );
    }

    #[test]
    fn a_plain_path_is_only_put_in_double_quotes() {
        // DST-R31: Exec is the path inside double quotes
        assert_eq!(
            quote_exec("/home/u/Applications/Pagle-Companion.AppImage"),
            "\"/home/u/Applications/Pagle-Companion.AppImage\""
        );
        assert_eq!(quote_exec("/home/u/My Apps/p"), "\"/home/u/My Apps/p\"");
    }

    #[test]
    fn quotes_backticks_dollars_and_backslashes_are_escaped() {
        // DST-R31: inside double quotes, ", `, $ and \ are escaped with a
        // backslash
        assert_eq!(
            quote_exec(r#"/home/u/My "Apps"/p.AppImage"#),
            r#""/home/u/My \"Apps\"/p.AppImage""#
        );
        assert_eq!(quote_exec(r"/a`b$c\d"), r#""/a\`b\$c\\d""#);
    }

    #[test]
    fn a_percent_is_doubled() {
        // DST-R31: % is written %%
        assert_eq!(quote_exec("/home/u/100%/p"), "\"/home/u/100%%/p\"");
    }
}

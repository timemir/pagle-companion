//! Keep uploading in Game Mode (CMP-R90 to CMP-R93): a toggle in the
//! window, on SteamOS when running as an AppImage, that keeps a `systemd
//! --user` unit running `$APPIMAGE --headless` whenever the Player's session
//! is up, Game Mode included (CMP Contracts "Game Mode unit").
//!
//! The decisions are here and tested over temp folders, in-memory credential
//! stores and a fake `systemctl`:
//! - whether the toggle is offered ([`offered`]);
//! - the unit's exact text ([`unit_text`]);
//! - turning it on and off, with CMP-R62's credential moves and the
//!   rollback ([`Machine::turn_on`], [`Machine::turn_off`]);
//! - CMP-R91's rewrite at every tray-mode start ([`Machine::at_start`]).
//!
//! The handover around the watcher lock (CMP-R92) is `handover`'s, and the
//! window's toggle is [`window`]'s.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::credential_store::Keychain;
use crate::state::{CredentialStore as StoreKind, Store};
use crate::systemctl::{OsSystemctl, Systemctl};
use crate::xdg;

pub mod window;

/// The unit restart delay, the unit's `RestartSec` (CMP Tunables).
pub const RESTART_DELAY: Duration = Duration::from_secs(30);

/// The Steam Deck report's new-issue URL, which the toggle links to
/// (DST-R71).
pub const STEAM_DECK_REPORT_URL: &str =
    "https://github.com/timemir/pagle-companion/issues/new?template=steam-deck-report.yml";

/// The window's text when turning the toggle on fails (CMP-R90).
pub const TURN_ON_FAILED: &str = "Couldn't set up Game Mode uploading.";
/// The window's text when turning the toggle off fails (CMP-R90).
pub const TURN_OFF_FAILED: &str = "Couldn't turn off Game Mode uploading.";

/// Where SteamOS says what it is.
pub const OS_RELEASE: &str = "/etc/os-release";

/// Whether `os_release`, the text of `/etc/os-release`, says SteamOS:
/// `ID=steamos`, quoted or not. `ID_LIKE` doesn't count.
pub fn is_steamos(os_release: &str) -> bool {
    os_release.lines().any(|line| {
        line.trim()
            .strip_prefix("ID=")
            .map(|id| id.trim().trim_matches(['"', '\'']))
            == Some("steamos")
    })
}

/// CMP-R90: the toggle is shown only on SteamOS (`ID=steamos` in
/// `/etc/os-release`) when running as an AppImage (`$APPIMAGE` is set).
pub fn offered(os_release: Option<&str>, appimage: Option<&str>) -> bool {
    appimage.is_some_and(|path| !path.is_empty()) && os_release.is_some_and(is_steamos)
}

/// The unit's `ExecStart`: `$APPIMAGE` quoted as DST-R31 quotes `Exec`,
/// then `--headless`.
pub fn exec_start(appimage: &str) -> String {
    format!("{} --headless", xdg::quote_exec(appimage))
}

/// The `ExecStart` value of a unit's text, if it has one.
pub fn exec_start_of(unit: &str) -> Option<&str> {
    unit.lines()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("ExecStart="))
}

/// The Game Mode unit exactly as CMP Contracts gives it. The `Environment=`
/// line is written only when `XDG_CONFIG_HOME` is set (`xdg_config_home`),
/// so the headless process uses the same state directory and lock.
pub fn unit_text(appimage: &str, xdg_config_home: Option<&str>) -> String {
    let environment = xdg_config_home
        .filter(|value| !value.is_empty())
        .map(|value| format!("{}\n", environment_line(value)))
        .unwrap_or_default();
    format!(
        "[Unit]\n\
         Description=Pagle Companion (uploads Catch logs in the background)\n\
         \n\
         [Service]\n\
         {environment}\
         ExecStart={exec_start}\n\
         Restart=always\n\
         RestartPreventExitStatus=3\n\
         RestartSec={restart_sec}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec_start = exec_start(appimage),
        restart_sec = RESTART_DELAY.as_secs(),
    )
}

/// `Environment=XDG_CONFIG_HOME=<value>`. systemd expands `%` specifiers
/// there, so `%` is written `%%`; and it splits assignments at whitespace,
/// so a value with whitespace, quotes, backslashes or control characters is
/// put in double quotes with `"` and `\` escaped, and each control
/// character written as C escapes (`\x0a`), which systemd reads back: a raw
/// line break would end the line and start a directive of its own. A plain
/// path is written as it is.
fn environment_line(value: &str) -> String {
    let assignment = format!("XDG_CONFIG_HOME={}", value.replace('%', "%%"));
    if assignment
        .contains(|c: char| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '\\'))
    {
        let mut escaped = String::with_capacity(assignment.len() + 8);
        for c in assignment.chars() {
            match c {
                '\\' => escaped.push_str("\\\\"),
                '"' => escaped.push_str("\\\""),
                c if c.is_control() => {
                    let mut utf8 = [0; 4];
                    for byte in c.encode_utf8(&mut utf8).bytes() {
                        escaped.push_str(&format!("\\x{byte:02x}"));
                    }
                }
                c => escaped.push(c),
            }
        }
        format!("Environment=\"{escaped}\"")
    } else {
        format!("Environment={assignment}")
    }
}

/// What [`Machine::at_start`] did to the unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Done {
    /// Its `ExecStart` wasn't `$APPIMAGE` (or it was missing): rewritten,
    /// and the units reloaded (CMP-R91).
    Rewrote,
    /// It already runs `$APPIMAGE`.
    Unchanged,
    /// Left as it was: this process doesn't know `$APPIMAGE`.
    NoPath,
    /// Nothing to do: the toggle is off.
    Off,
}

/// What the toggle works on: the unit file, `$APPIMAGE`, `$XDG_CONFIG_HOME`
/// and `systemctl`.
pub struct Machine {
    /// `$XDG_CONFIG_HOME/systemd/user/pagle-companion.service`, `None`
    /// without a config folder.
    pub unit: Option<PathBuf>,
    /// `$APPIMAGE`, the unit's `ExecStart`.
    pub appimage: Option<String>,
    /// `$XDG_CONFIG_HOME` as this process's environment has it, for the
    /// unit's `Environment=`.
    pub xdg_config_home: Option<String>,
    pub systemctl: Box<dyn Systemctl>,
}

impl Machine {
    /// This OS user's unit and this process's `$APPIMAGE`, with the real
    /// `systemctl`.
    pub fn for_this_process() -> Self {
        Self {
            unit: xdg::this_config_home().map(|home| xdg::unit_path(&home)),
            appimage: xdg::appimage(),
            xdg_config_home: std::env::var("XDG_CONFIG_HOME").ok(),
            systemctl: Box::new(OsSystemctl::new()),
        }
    }

    /// Turning the toggle on (CMP-R90), in order: moves the credential to
    /// the credential file (CMP-R62), writes the unit, runs `daemon-reload`
    /// and `enable`. If a step fails, the steps before it are undone and
    /// the toggle stays off. The unit isn't started: the handover starts it
    /// when this tray-mode process exits (CMP-R92).
    ///
    /// The move writes the credential file first and switches
    /// `credentialStore` together with `settings.gameMode`, once every step
    /// worked; only then does the old store let go of it. Until then the
    /// old store still holds the credential and is still the one read, so
    /// undoing the move is deleting the file's copy.
    pub fn turn_on(&self, store: &Store, keychain: &Keychain) -> Result<(), String> {
        let appimage = self
            .appimage
            .as_deref()
            .ok_or("not running as an AppImage")?;
        let unit = self.unit.as_deref().ok_or("no config folder")?;

        let moved = copy_credential(store, keychain, StoreKind::File)
            .map_err(|e| format!("couldn't move the credential to the credential file: {e}"))?;
        let undo_move = || {
            if moved.is_some() {
                delete_copy(keychain, StoreKind::File);
            }
        };

        if let Err(e) = write_unit(unit, &unit_text(appimage, self.xdg_config_home.as_deref())) {
            undo_move();
            return Err(format!("couldn't write {}: {e}", unit.display()));
        }
        let undo_unit = || {
            if let Err(e) = remove_file(unit) {
                log::warn!("couldn't delete {} again: {e}", unit.display());
            }
        };
        if let Err(e) = self.systemctl.run(&["daemon-reload"]) {
            undo_unit();
            undo_move();
            return Err(e);
        }
        if let Err(e) = self.systemctl.run(&["enable", xdg::UNIT_NAME]) {
            undo_unit();
            if let Err(e) = self.systemctl.run(&["daemon-reload"]) {
                log::warn!("{e}");
            }
            undo_move();
            return Err(e);
        }

        let saved = store.update(|s| {
            s.settings.game_mode = true;
            if moved.is_some() {
                s.credential_store = Some(StoreKind::File);
            }
        });
        if let Err(e) = saved {
            // Kept in memory and written with the next change.
            log::error!("couldn't write state.json: {e}");
        }
        if let Some(from) = moved {
            delete_moved(keychain, from);
        }
        Ok(())
    }

    /// Turning the toggle off (CMP-R90): `disable --now`, deletes the unit,
    /// `daemon-reload`, then moves the credential back to the Secret Service
    /// when one answers (CMP-R62); when none answers it stays in the
    /// credential file. If `systemctl` (or the delete) fails, the toggle
    /// stays on.
    pub fn turn_off(&self, store: &Store, keychain: &Keychain) -> Result<(), String> {
        let unit = self.unit.as_deref().ok_or("no config folder")?;
        self.systemctl.run(&["disable", "--now", xdg::UNIT_NAME])?;
        remove_file(unit).map_err(|e| format!("couldn't delete {}: {e}", unit.display()))?;
        self.systemctl.run(&["daemon-reload"])?;

        let moved =
            copy_credential(store, keychain, StoreKind::SecretService).unwrap_or_else(|e| {
                log::info!("the credential stays in the credential file: {e}");
                None
            });
        let saved = store.update(|s| {
            s.settings.game_mode = false;
            if moved.is_some() {
                s.credential_store = Some(StoreKind::SecretService);
            }
        });
        if let Err(e) = saved {
            log::error!("couldn't write state.json: {e}");
        }
        if let Some(from) = moved {
            delete_moved(keychain, from);
        }
        Ok(())
    }

    /// CMP-R91: at every tray-mode start, while the toggle is on and
    /// `$APPIMAGE` is set, rewrites the unit if its `ExecStart` path is no
    /// longer `$APPIMAGE` (or it is missing), and runs `daemon-reload`.
    pub fn at_start(&self, game_mode: bool) -> Result<Done, String> {
        if !game_mode {
            return Ok(Done::Off);
        }
        let (Some(appimage), Some(unit)) = (self.appimage.as_deref(), self.unit.as_deref()) else {
            return Ok(Done::NoPath);
        };
        let current = fs::read_to_string(unit).ok();
        if current.as_deref().and_then(exec_start_of) == Some(exec_start(appimage).as_str()) {
            return Ok(Done::Unchanged);
        }
        write_unit(unit, &unit_text(appimage, self.xdg_config_home.as_deref()))
            .map_err(|e| format!("couldn't write {}: {e}", unit.display()))?;
        self.systemctl.run(&["daemon-reload"])?;
        Ok(Done::Rewrote)
    }
}

/// Writes the credential, while linked, from the store `credentialStore`
/// names into the store `to` too, and returns the store it came from. `None`
/// when there is nothing to move: not linked, already there, or the store
/// holds no credential.
fn copy_credential(
    store: &Store,
    keychain: &Keychain,
    to: StoreKind,
) -> Result<Option<StoreKind>, String> {
    let (linked, from) = store.read(|s| (s.battle_tag.is_some(), s.credential_store));
    let Some(from) = from.filter(|from| linked && *from != to) else {
        return Ok(None);
    };
    let target = keychain
        .store(to)
        .ok_or("this platform has no such store")?;
    let credential = keychain
        .current(Some(from))
        .read()
        .map_err(|e| e.to_string())?;
    let Some(credential) = credential else {
        return Ok(None);
    };
    target.write(&credential).map_err(|e| e.to_string())?;
    Ok(Some(from))
}

/// Undoes [`copy_credential`]: the store `to` lets go of its copy.
fn delete_copy(keychain: &Keychain, to: StoreKind) {
    if let Some(Err(e)) = keychain.store(to).map(|store| store.delete()) {
        log::warn!(
            "couldn't delete the credential's copy again; deleting it at the next start: {e}"
        );
    }
}

/// The store the credential moved out of lets go of it; a failed delete is
/// tried again at the next start (CMP-R62).
fn delete_moved(keychain: &Keychain, from: StoreKind) {
    if let Some(Err(e)) = keychain.store(from).map(|store| store.delete()) {
        log::warn!(
            "couldn't delete the credential from its old store; trying again at the next start: {e}"
        );
    }
}

/// Writes the unit whole through a temporary file, so systemd never reads
/// half of one.
fn write_unit(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("service.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Deletes the file at `path`; a missing one is already gone.
fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;

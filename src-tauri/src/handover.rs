//! The handover between a tray-mode process and the Game Mode unit
//! (CMP-R92), so that only one of them watches (CMP-R2). While the toggle is
//! on (CMP-R90), on Linux:
//! - a tray-mode start runs `systemctl --user stop pagle-companion.service`
//!   before taking the watcher lock ([`before_taking_lock`]). A headless
//!   process stopped this way releases the lock and exits 0 (`headless`),
//!   and tray mode's lock wait (CMP-R2) follows;
//! - a tray-mode process that exits, once it has released the lock, runs
//!   `systemctl --user start pagle-companion.service`
//!   ([`after_releasing_lock`]), unless it is quitting for `--quit` or
//!   removal (CMP-R70), or exiting for **Restart to update** (DST-R54): the
//!   new version starts itself in tray mode, which would stop the unit
//!   again at once.
//!
//! Tray mode quits through one exit path for **Quit**, `--quit` passed
//! through the single-instance channel, and `SIGTERM` or `SIGHUP` (logout,
//! the switch to Game Mode). It notes why with [`note_exit`] before it asks
//! Tauri to exit, so the exit tells them apart ([`noted_exit`]). Removal from
//! the Linux tray item exits with `std::process::exit` and never reaches
//! this step (`removal::window`); [`Exit::Removal`] covers it anyway. A tray
//! process that is killed outright starts nothing; the unit then starts at
//! the next session.

use std::sync::OnceLock;

use crate::companion::Companion;
use crate::systemctl::{OsSystemctl, Systemctl};
use crate::xdg;

/// Why a tray-mode process exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// **Quit** in the tray menu or the window.
    Quit,
    /// `SIGTERM` or `SIGHUP`: logout, or the switch to Game Mode.
    Signal,
    /// `--quit`, passed through the single-instance channel (CMP-R2); sent
    /// by removal (CMP-R70).
    QuitArgument,
    /// Removal from the Linux tray item (CMP-R71).
    Removal,
    /// **Restart to update**, or an update installed at start (DST-R54):
    /// the new version starts in tray mode itself.
    Update,
}

/// Whether the handover runs here: the unit is Linux's.
pub const HANDS_OVER: bool = cfg!(target_os = "linux");

/// CMP-R92: an exiting tray-mode process starts the unit while the toggle is
/// on, unless it is quitting for `--quit` or removal (CMP-R70), or exiting
/// for an update, after which the new version starts in tray mode (DST-R54).
pub fn starts_unit(game_mode: bool, exit: Exit) -> bool {
    game_mode && matches!(exit, Exit::Quit | Exit::Signal)
}

/// CMP-R92: a tray-mode start stops the unit, while the toggle is on, before
/// it takes the watcher lock.
pub fn stop_unit(game_mode: bool, systemctl: &dyn Systemctl) {
    if !game_mode {
        return;
    }
    match systemctl.run(&["stop", xdg::UNIT_NAME]) {
        Ok(()) => log::info!("stopped the Game Mode unit"),
        Err(e) => log::warn!("couldn't stop the Game Mode unit: {e}"),
    }
}

/// CMP-R92: an exiting tray-mode process, once it has released the lock,
/// starts the unit when [`starts_unit`] says so.
pub fn start_unit(game_mode: bool, exit: Exit, systemctl: &dyn Systemctl) {
    if !starts_unit(game_mode, exit) {
        if game_mode {
            log::info!("quitting for {exit:?}: not starting the Game Mode unit");
        }
        return;
    }
    match systemctl.run(&["start", xdg::UNIT_NAME]) {
        Ok(()) => log::info!("started the Game Mode unit"),
        Err(e) => log::warn!("couldn't start the Game Mode unit: {e}"),
    }
}

/// Why this process is exiting, once known. The first reason wins.
static EXITING: OnceLock<Exit> = OnceLock::new();

/// Notes why this tray-mode process is about to exit, before it asks Tauri
/// to.
pub fn note_exit(exit: Exit) {
    let _ = EXITING.set(exit);
}

/// Why this process exits: what [`note_exit`] noted, else **Quit**.
pub fn noted_exit() -> Exit {
    EXITING.get().copied().unwrap_or(Exit::Quit)
}

/// CMP-R92: a tray-mode start stops the Game Mode unit before it takes the
/// watcher lock.
pub fn before_taking_lock(companion: &Companion) {
    if HANDS_OVER {
        let game_mode = companion.state.read(|s| s.settings.game_mode);
        stop_unit(game_mode, &OsSystemctl::new());
    }
}

/// CMP-R92: a tray-mode process that exits, once it has released the
/// watcher lock, starts the Game Mode unit, unless it quits for `exit`s that
/// don't.
pub fn after_releasing_lock(companion: &Companion, exit: Exit) {
    if HANDS_OVER {
        let game_mode = companion.state.read(|s| s.settings.game_mode);
        start_unit(game_mode, exit, &OsSystemctl::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systemctl::testing::FakeSystemctl;

    const STOP: &str = "systemctl --user stop pagle-companion.service";
    const START: &str = "systemctl --user start pagle-companion.service";

    #[test]
    fn a_tray_start_stops_the_unit_only_while_the_toggle_is_on() {
        // CMP-R92: while the toggle is on, a tray-mode start runs systemctl
        // --user stop pagle-companion.service before taking the lock;
        // CMP-R93: without the toggle, nothing runs in Game Mode
        let systemctl = FakeSystemctl::default();
        stop_unit(false, &systemctl);
        assert!(systemctl.calls().is_empty());
        stop_unit(true, &systemctl);
        assert_eq!(systemctl.calls(), [STOP]);
    }

    #[test]
    fn exiting_for_an_update_doesnt_start_the_unit() {
        // CMP-R92, DST-R54: Restart to update exits and the new version
        // starts itself in tray mode, which stops the unit before it takes
        // the lock; so the exit starts nothing
        assert!(!starts_unit(true, Exit::Update));
        let systemctl = FakeSystemctl::default();
        start_unit(true, Exit::Update, &systemctl);
        assert!(systemctl.calls().is_empty());
    }

    #[test]
    fn quit_and_signals_start_the_unit_but_quit_argument_and_removal_dont() {
        // CMP-R92: when the tray-mode process exits (Quit, or SIGTERM or
        // SIGHUP), it runs systemctl --user start pagle-companion.service,
        // unless it is quitting for --quit or removal (CMP-R70)
        for (exit, starts) in [
            (Exit::Quit, true),
            (Exit::Signal, true),
            (Exit::QuitArgument, false),
            (Exit::Removal, false),
        ] {
            assert_eq!(starts_unit(true, exit), starts, "{exit:?}");
            assert!(!starts_unit(false, exit), "{exit:?}");
            let systemctl = FakeSystemctl::default();
            start_unit(true, exit, &systemctl);
            let expected: &[&str] = if starts { &[START] } else { &[] };
            assert_eq!(systemctl.calls(), expected, "{exit:?}");
            let systemctl = FakeSystemctl::default();
            start_unit(false, exit, &systemctl);
            assert!(systemctl.calls().is_empty(), "{exit:?}");
        }
    }

    #[test]
    fn a_failing_systemctl_lets_the_start_and_the_exit_go_on() {
        // CMP-R92: the handover never holds up the lock wait or the exit;
        // a unit that isn't stopped makes the lock wait show the window
        // read-only (CMP-R2)
        let systemctl = FakeSystemctl::default();
        systemctl.fail("");
        stop_unit(true, &systemctl);
        start_unit(true, Exit::Quit, &systemctl);
        assert_eq!(systemctl.calls(), [STOP, START]);
    }

    #[test]
    fn an_exit_nobody_noted_is_quit() {
        // CMP-R92: Quit starts the unit; only --quit and removal are noted
        // as exits that don't
        assert_eq!(noted_exit(), Exit::Quit);
    }

    #[test]
    fn the_handover_is_linux_only() {
        // CMP-R90: the unit is a systemd --user unit, on SteamOS
        assert_eq!(HANDS_OVER, cfg!(target_os = "linux"));
    }
}

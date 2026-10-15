//! Start at login in tray mode (CMP-R3): the entry kept current at every
//! start, created when first run's step 1 ends, and the window's Settings
//! toggle. The toggle itself is `ui/settings.js`.

use tauri::{AppHandle, Manager};

use super::Done;
use crate::companion::Companion;

/// The window's text when turning the toggle on fails.
pub const TURN_ON_FAILED: &str = "Couldn't turn on Start at login.";
/// The window's text when turning the toggle off fails.
pub const TURN_OFF_FAILED: &str = "Couldn't turn off Start at login.";

/// At every tray-mode start: recreates a missing entry and rewrites a changed
/// path while the toggle is on (CMP-R3, CMP-R91). A first run leaves it to
/// the end of step 1.
pub fn at_start(companion: &Companion) {
    let done = super::for_this_process()
        .and_then(|autostart| companion.state.read(|s| autostart.at_start(s)));
    log_done(done);
}

/// Creates the entry while the toggle is on (CMP-R3). First run calls this
/// when step 1 ends, right after it sets `firstRunDone` (CMP-R100).
pub fn create(app: &AppHandle) {
    let companion = app.state::<Companion>();
    let done = super::for_this_process()
        .and_then(|autostart| companion.state.read(|s| autostart.create(s)));
    log_done(done);
}

fn log_done(done: std::io::Result<Done>) {
    match done {
        Ok(Done::Created) => log::info!("created the autostart entry"),
        Ok(Done::Rewrote) => log::info!("rewrote the autostart entry's path"),
        Ok(Done::NoPath) => log::info!("no $APPIMAGE: left the autostart entry as it is"),
        Ok(_) => {}
        Err(e) => log::error!("couldn't write the autostart entry: {e}"),
    }
}

/// The toggle's state: `settings.startAtLogin`.
#[tauri::command]
pub fn start_at_login(companion: tauri::State<'_, Companion>) -> bool {
    companion.state.read(|s| s.settings.start_at_login)
}

/// The window's "Start at login" toggle (CMP-R3). Returns the toggle as it
/// now is; the error is the text to show.
#[tauri::command]
pub fn set_start_at_login(
    companion: tauri::State<'_, Companion>,
    on: bool,
) -> Result<bool, String> {
    if !companion.is_watcher() {
        // CMP-R2: the window of a process without the lock is read-only.
        return Err("Another Pagle Companion is running for this user.".into());
    }
    let failed = if on { TURN_ON_FAILED } else { TURN_OFF_FAILED };
    match super::for_this_process().and_then(|autostart| autostart.set(&companion.state, on)) {
        Ok(done) => {
            log::info!("Start at login turned {}", if on { "on" } else { "off" });
            log_done(Ok(done));
            Ok(on)
        }
        Err(e) => {
            log::error!("couldn't change the autostart entry: {e}");
            Err(failed.into())
        }
    }
}

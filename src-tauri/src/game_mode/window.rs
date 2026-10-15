//! The "Keep uploading in Game Mode (experimental)" toggle in the window's
//! Settings (CMP-R90, CMP UI), with its Steam Deck report link (DST-R71),
//! and CMP-R91's unit rewrite at every tray-mode start. The toggle itself is
//! `ui/settings.js`.

use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Manager};

use super::{
    Done, Machine, OS_RELEASE, STEAM_DECK_REPORT_URL, TURN_OFF_FAILED, TURN_ON_FAILED, offered,
};
use crate::companion::Companion;
use crate::xdg;

/// What Settings shows of the toggle.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameModeView {
    /// Whether the toggle is shown: on SteamOS when running as an AppImage
    /// (CMP-R90), and always while it is on, so it can be turned off.
    pub shown: bool,
    /// `settings.gameMode`.
    pub on: bool,
}

/// Whether this machine offers the toggle (CMP-R90).
fn offered_here() -> bool {
    let os_release = std::fs::read_to_string(OS_RELEASE).ok();
    offered(os_release.as_deref(), xdg::appimage().as_deref())
}

/// CMP-R91: at every tray-mode start, while the toggle is on, the unit's
/// `ExecStart` is kept at `$APPIMAGE`.
pub fn at_start(companion: &Companion) {
    let game_mode = companion.state.read(|s| s.settings.game_mode);
    match Machine::for_this_process().at_start(game_mode) {
        Ok(Done::Rewrote) => log::info!("rewrote the Game Mode unit's path"),
        Ok(Done::NoPath) => log::info!("no $APPIMAGE: left the Game Mode unit as it is"),
        Ok(Done::Unchanged | Done::Off) => {}
        Err(e) => log::error!("couldn't rewrite the Game Mode unit: {e}"),
    }
}

/// The toggle's state.
#[tauri::command]
pub fn game_mode(companion: tauri::State<'_, Companion>) -> GameModeView {
    let on = companion.state.read(|s| s.settings.game_mode);
    GameModeView {
        shown: on || offered_here(),
        on,
    }
}

/// The toggle (CMP-R90). Returns the toggle as it now is; the error is the
/// text to show. `systemctl` may take a while, so it runs off the main
/// thread.
#[tauri::command]
pub async fn set_game_mode(app: AppHandle, on: bool) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || set(&app, on))
        .await
        .unwrap_or_else(|_| Err(failed(on).into()))
}

/// The toggle's link: the Steam Deck report's new-issue page (DST-R71).
#[tauri::command]
pub fn open_steam_deck_report() {
    if let Err(e) = tauri_plugin_opener::open_url(STEAM_DECK_REPORT_URL, None::<&str>) {
        log::warn!("couldn't open the browser: {e}");
    }
}

fn failed(on: bool) -> &'static str {
    if on { TURN_ON_FAILED } else { TURN_OFF_FAILED }
}

fn set(app: &AppHandle, on: bool) -> Result<bool, String> {
    // One change at a time.
    static CHANGING: Mutex<()> = Mutex::new(());
    let _changing = CHANGING.lock().unwrap_or_else(|e| e.into_inner());

    let companion = app.state::<Companion>();
    if !companion.is_watcher() {
        // CMP-R2: the window of a process without the lock is read-only.
        return Err("Another Pagle Companion is running for this user.".into());
    }
    if companion.state.read(|s| s.settings.game_mode) == on {
        return Ok(on);
    }
    if on && !offered_here() {
        log::error!("Game Mode uploading is offered only on SteamOS, as an AppImage");
        return Err(TURN_ON_FAILED.into());
    }
    let machine = Machine::for_this_process();
    let keychain = companion.link.keychain();
    let done = if on {
        machine.turn_on(&companion.state, keychain)
    } else {
        machine.turn_off(&companion.state, keychain)
    };
    match done {
        Ok(()) => {
            log::info!(
                "Game Mode uploading turned {}",
                if on { "on" } else { "off" }
            );
            Ok(on)
        }
        Err(e) => {
            log::error!(
                "couldn't turn Game Mode uploading {}: {e}",
                if on { "on" } else { "off" }
            );
            Err(failed(on).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_show_the_toggle_as_experimental_with_the_report_link() {
        // CMP UI, Settings: "Keep uploading in Game Mode (experimental)";
        // CMP-R90, DST-R70: labelled experimental; DST-R71: it links to the
        // Steam Deck report's new-issue URL
        let page = include_str!("../../../ui/index.html");
        assert!(page.contains("Keep uploading in Game Mode (experimental)"));
        let script = include_str!("../../../ui/settings.js");
        assert!(script.contains("open_steam_deck_report"));
        assert_eq!(
            STEAM_DECK_REPORT_URL,
            "https://github.com/timemir/pagle-companion/issues/new?template=steam-deck-report.yml"
        );
    }
}

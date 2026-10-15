//! The window's WoW folders section (CMP UI): the installs and their account
//! folders, "Add a WoW folder…" and **Remove** (CMP-R13). The section itself
//! is `ui/folders.js`.

use std::path::PathBuf;

use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_dialog::DialogExt;

use super::{Inputs, POLL_INTERVAL, Scanner, Snapshot};
use crate::companion::Companion;
use crate::host::Host;

/// The event that carries a new [`View`] to the window.
pub const EVENT: &str = "installs";

/// What the WoW folders section shows.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    /// `None` until the first scan has ended.
    pub snapshot: Option<Snapshot>,
    /// The poll interval, for "Checked every {poll interval}" (CMP-R22).
    pub poll_seconds: u64,
}

impl View {
    fn of(snapshot: Option<Snapshot>) -> Self {
        Self {
            snapshot,
            poll_seconds: POLL_INTERVAL.as_secs(),
        }
    }
}

/// The Forever product uid: the last successful config read's, or the
/// built-in one until one has succeeded (CMP-R12).
pub fn forever_product_uid(companion: &Companion) -> String {
    companion.state.read(super::forever_product_uid)
}

/// Starts scanning for this process, which holds the watcher lock (CMP-R2),
/// and keeps the window's section up to date.
pub fn start<H: Host>(app: &H) {
    let scanner = app.state::<Scanner>();
    let updates = scanner.subscribe();
    let inputs_app = app.clone();
    scanner.start(move || {
        let companion = inputs_app.state::<Companion>();
        Inputs {
            sources: super::system::db_sources(),
            manual: companion.state.read(|s| s.manual_installs.clone()),
            uid: forever_product_uid(companion),
        }
    });
    let app = app.clone();
    std::thread::spawn(move || {
        for snapshot in updates {
            app.emit(EVENT, View::of(Some(snapshot)));
        }
    });
}

#[tauri::command]
pub fn installs(scanner: tauri::State<'_, Scanner>) -> View {
    View::of(scanner.snapshot())
}

/// "Add a WoW folder…": picks a folder and adds the installs it stands for
/// (CMP-R13). `Ok(false)` when the Player cancelled the picker; the error is
/// the text to show.
#[tauri::command]
pub async fn add_wow_folder(app: AppHandle) -> Result<bool, String> {
    let companion = app.state::<Companion>();
    if !companion.is_watcher() {
        return Err("Another Pagle Companion is running for this user.".into());
    }
    let Some(picked) = app
        .dialog()
        .file()
        .set_title("Add a WoW folder")
        .blocking_pick_folder()
    else {
        return Ok(false);
    };
    let picked: PathBuf = picked.into_path().map_err(|e| e.to_string())?;
    let installs = super::picked_installs(&picked, &forever_product_uid(companion))?;
    companion
        .state
        .update(|s| super::add_manual(&mut s.manual_installs, &installs))
        .map_err(|e| {
            log::error!("couldn't write state.json: {e}");
            e.to_string()
        })?;
    for install in &installs {
        log::info!("added WoW folder {install}");
    }
    app.state::<Scanner>().rescan();
    Ok(true)
}

/// **Remove**: deletes a manual install from local state, which stops
/// watching it at the scan that follows (CMP-R13).
#[tauri::command]
pub fn remove_wow_folder(app: AppHandle, flavor: String) -> Result<(), String> {
    let companion = app.state::<Companion>();
    if !companion.is_watcher() {
        return Err("Another Pagle Companion is running for this user.".into());
    }
    let removed = companion
        .state
        .update(|s| super::remove_manual(&mut s.manual_installs, &flavor))
        .map_err(|e| {
            log::error!("couldn't write state.json: {e}");
            e.to_string()
        })?;
    if removed {
        log::info!("removed WoW folder {flavor}");
        app.state::<Scanner>().rescan();
    }
    Ok(())
}

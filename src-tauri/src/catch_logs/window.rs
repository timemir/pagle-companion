//! The window's Catch logs list (CMP-R41, CMP UI): its rows, the answers,
//! the picks, and the link to My Anglers. The list itself is
//! `ui/catch-logs.js`.

use tauri::{AppHandle, Emitter, Manager};

use super::{CatchLogs, View};
use crate::installs::Scanner;
use crate::linking;
use crate::readings::{Readings, Snapshot};
use crate::tray::Companion;

/// The event that carries a new [`View`] to the window.
pub const EVENT: &str = "catch-logs";

/// What a change made from a read-only window says (CMP-R2).
const READ_ONLY: &str = "Another Pagle Companion is running for this user.";

/// Takes every reading into local state, the notifications and the
/// confirmed set, for this process, which holds the watcher lock (CMP-R2),
/// and keeps the window's list up to date. Call it after the readings have
/// started.
pub fn start(app: &AppHandle) {
    let updates = app.state::<Readings>().subscribe();
    let app = app.clone();
    std::thread::spawn(move || {
        for update in updates {
            refresh(&app, &update.snapshot);
        }
    });
}

/// Runs the decisions on `snapshot` and shows the list again.
pub fn refresh(app: &AppHandle, snapshot: &Snapshot) {
    let companion = app.state::<Companion>();
    app.state::<CatchLogs>()
        .refresh(&companion.state, &companion.notifications, snapshot);
    emit(app);
}

/// Opens or closes the first-run window's hold on notifications (CMP-R100,
/// #9): see [`CatchLogs::set_first_run_window`]. Closing it raises what is
/// due.
pub fn set_first_run_window(app: &AppHandle, open: bool) {
    app.state::<CatchLogs>().set_first_run_window(open);
    if !open && let Some(snapshot) = app.state::<Readings>().snapshot() {
        refresh(app, &snapshot);
    }
}

fn current(app: &AppHandle) -> View {
    let installs: Vec<String> = app
        .state::<Scanner>()
        .installs()
        .into_iter()
        .map(|install| install.flavor)
        .collect();
    let snapshot = app.state::<Readings>().snapshot();
    app.state::<Companion>()
        .state
        .read(|s| super::view(s, snapshot.as_ref(), &installs))
}

fn emit(app: &AppHandle) {
    let _ = app.emit(EVENT, current(app));
}

fn watcher(app: &AppHandle) -> Result<(), String> {
    if app.state::<Companion>().is_watcher() {
        Ok(())
    } else {
        Err(READ_ONLY.into())
    }
}

#[tauri::command]
pub fn catch_logs(app: AppHandle) -> View {
    current(&app)
}

/// **Upload** or **Don't upload**, from the question or the switch (CMP-R40,
/// CMP-R41).
#[tauri::command]
pub fn answer_catch_log(app: AppHandle, log_id: String, upload: bool) -> Result<(), String> {
    watcher(&app)?;
    let companion = app.state::<Companion>();
    let snapshot = app.state::<Readings>().snapshot();
    let answered = app.state::<CatchLogs>().answer(
        &companion.state,
        &companion.notifications,
        snapshot.as_ref(),
        &log_id,
        upload,
    );
    emit(&app);
    answered
}

/// A folder from CMP-R42's question or **Change folder**.
#[tauri::command]
pub fn pick_folder(app: AppHandle, log_id: String, folder: String) -> Result<(), String> {
    watcher(&app)?;
    let companion = app.state::<Companion>();
    let Some(snapshot) = app.state::<Readings>().snapshot() else {
        return Err("Pagle Companion is still reading your WoW folders.".into());
    };
    let picked = app.state::<CatchLogs>().pick(
        &companion.state,
        &companion.notifications,
        &snapshot,
        &log_id,
        &folder,
    );
    emit(&app);
    picked
}

/// The link after switching **Upload** off: My Anglers, in the browser
/// (CMP-R41).
#[tauri::command]
pub fn open_my_anglers(companion: tauri::State<'_, Companion>) {
    let url = super::my_anglers_url(companion.api.base_url());
    if !linking::may_open(&url) {
        log::warn!("not opening My Anglers: the base URL isn't HTTPS");
        return;
    }
    if let Err(e) = tauri_plugin_opener::open_url(&url, None::<&str>) {
        log::warn!("couldn't open the browser: {e}");
    }
}

//! The window's Catch logs list (CMP-R41, CMP UI): its rows, the answers,
//! the picks, and the links to My Anglers and sign-in. The list itself is
//! `ui/catch-logs.js`.

use tauri::AppHandle;

use super::{CatchLogs, View};
use crate::companion::Companion;
use crate::host::Host;
use crate::installs::Scanner;
use crate::linking;
use crate::readings::{Readings, Snapshot};
use crate::upload;

/// The event that carries a new [`View`] to the window.
pub const EVENT: &str = "catch-logs";

/// What a change made from a read-only window says (CMP-R2).
const READ_ONLY: &str = "Another Pagle Companion is running for this user.";

/// Takes every reading into local state, the notifications and the
/// confirmed set, for this process, which holds the watcher lock (CMP-R2),
/// and keeps the window's list up to date. Call it after the readings have
/// started.
pub fn start<H: Host>(app: &H) {
    let updates = app.state::<Readings>().subscribe();
    let app = app.clone();
    std::thread::spawn(move || {
        for update in updates {
            refresh(&app, &update.snapshot);
            // CMP-R45: the triggers see the confirmed set this update made.
            upload::window::after_reading(&app, &update);
        }
    });
}

/// Runs the decisions on `snapshot` and shows the list again.
pub fn refresh<H: Host>(app: &H, snapshot: &Snapshot) {
    let companion = app.state::<Companion>();
    app.state::<CatchLogs>()
        .refresh(&companion.state, &companion.notifications, snapshot);
    upload::notify(&companion.state, &companion.notifications);
    emit(app);
}

/// The Catch logs list as it is now.
pub fn current<H: Host>(app: &H) -> View {
    let installs: Vec<String> = app
        .state::<Scanner>()
        .installs()
        .into_iter()
        .map(|install| install.flavor)
        .collect();
    let snapshot = app.state::<Readings>().snapshot();
    app.state::<Companion>()
        .state
        .read(|s| super::view(s, snapshot.as_ref(), &installs, upload::window::now()))
}

/// Shows the list again.
pub fn emit<H: Host>(app: &H) {
    app.emit(EVENT, current(app));
}

fn watcher<H: Host>(app: &H) -> Result<(), String> {
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
    // A log switched off no longer raises its runs' causes.
    upload::notify(&companion.state, &companion.notifications);
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

/// The link to My Anglers, in the browser: after switching **Upload** off
/// (CMP-R41), on deleted (CMP-R54), log cap (CMP UI) and left off
/// (CMP-R53).
#[tauri::command]
pub fn open_my_anglers(companion: tauri::State<'_, Companion>) {
    open_page(&super::my_anglers_url(companion.api.base_url()));
}

/// **Sign in**, for held characters (CMP-R53): `/signin` in the browser.
#[tauri::command]
pub fn open_sign_in(companion: tauri::State<'_, Companion>) {
    open_page(&super::sign_in_url(companion.api.base_url()));
}

fn open_page(url: &str) {
    if !linking::may_open(url) {
        log::warn!("not opening a Pagle page: the base URL isn't HTTPS");
        return;
    }
    if let Err(e) = tauri_plugin_opener::open_url(url, None::<&str>) {
        log::warn!("couldn't open the browser: {e}");
    }
}

use crate::catch_logs::CatchLogs;
use crate::companion::Companion;
use crate::upload::Uploads;
use tauri::{AppHandle, Manager};

#[tauri::command]
pub fn recover_catch_log(app: AppHandle, log_id: String) -> Result<(), String> {
    let companion = app.state::<Companion>();
    if !companion.is_watcher() {
        return Err("Another Pagle Companion is running for this user.".into());
    }
    if !app.state::<CatchLogs>().confirmed().contains_key(&log_id) {
        return Err("Choose the Catch log's folder first.".into());
    }
    if !companion.account().is_linked() || companion.account().is_stopped() {
        return Err("Link Pagle Companion before preparing Recovery.".into());
    }
    app.state::<Uploads>().trigger_recovery(&log_id);
    Ok(())
}

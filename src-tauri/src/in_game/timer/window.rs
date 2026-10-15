//! The in-game timer in tray mode (CMP-R81, CMP-R101): its checks, once the
//! first scan has ended and then every in-game check interval, and the check
//! right after linking. Each check is [`Uploads::in_game_check`] over the
//! app's installs; the queue's worker sends what it queues.

use tauri::{AppHandle, Manager};

use super::CHECK_INTERVAL;
use crate::catch_logs::CatchLogs;
use crate::installs::Scanner;
use crate::tray::Companion;
use crate::upload::window::AppEnv;
use crate::upload::{Ctx, Uploads};

/// Starts the timer, for this process, which holds the watcher lock
/// (CMP-R2): the start's check once the first scan has ended, so the installs
/// are known, then one every in-game check interval.
pub fn start(app: &AppHandle) {
    let first_scan = app.state::<Scanner>().subscribe();
    let app = app.clone();
    std::thread::spawn(move || {
        if first_scan.recv().is_err() {
            return;
        }
        drop(first_scan);
        loop {
            check(&app);
            std::thread::sleep(CHECK_INTERVAL);
        }
    });
}

/// Right after any link (CMP-R101): both downloads are owed again, and the
/// timer is checked at once.
pub fn after_link(app: &AppHandle) {
    let Some(uploads) = app.try_state::<Uploads>() else {
        return;
    };
    uploads.owe_downloads();
    check(app);
}

/// One check of the timer (CMP-R81, CMP-R83).
fn check(app: &AppHandle) {
    let (Some(companion), Some(uploads), Some(catch_logs)) = (
        app.try_state::<Companion>(),
        app.try_state::<Uploads>(),
        app.try_state::<CatchLogs>(),
    ) else {
        return;
    };
    if !companion.is_watcher() {
        return;
    }
    let confirmed = catch_logs.confirmed();
    let env = AppEnv(app.clone());
    let ctx = Ctx {
        account: companion.account(),
        state: &companion.state,
        version: &companion.version,
        confirmed: &confirmed,
        env: &env,
    };
    uploads.in_game_check(&ctx);
}

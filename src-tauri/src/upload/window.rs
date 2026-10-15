//! Uploading in tray mode: the queue's worker, CMP-R45's triggers (a
//! debounced change, start, later scans, the **Upload** switch, **Upload
//! now**, linking, **Replace**), which queue nothing while not linked or
//! during a stop, and "Last uploaded" in the window's Status and the tray
//! menu (CMP UI).

use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Emitter, Manager};

use super::queue::Trigger;
use super::{Ctx, Env, Uploads, plan};
use crate::catch_logs::{self, CatchLogs};
use crate::installs::paths;
use crate::readings::{self, Reading, Readings};
use crate::state::State;
use crate::tray::{self, Companion};

/// The event that carries the Status part's "Last uploaded …" text.
pub const LAST_UPLOADED_EVENT: &str = "last-uploaded";

/// How often relative times are shown again.
pub const TICK: Duration = Duration::from_secs(30);

/// Unix seconds now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The newest `lastUploadedAt` of every `logId` (CMP-R50: "last uploaded").
pub fn last_uploaded_at(state: &State) -> Option<i64> {
    state
        .logs
        .values()
        .filter_map(|log| log.last_uploaded_at)
        .max()
}

/// The Status part's line (CMP UI, Status): "Last uploaded {relative time}"
/// or "Nothing uploaded yet".
pub fn last_uploaded_text(last: Option<i64>, now: i64) -> String {
    match last {
        Some(at) => format!("Last uploaded {}", plan::relative_time(now, at)),
        None => "Nothing uploaded yet".into(),
    }
}

/// Starts the queue's worker, for this process, which holds the watcher
/// lock (CMP-R2), and shows relative times again every [`TICK`].
pub fn start(app: &AppHandle) {
    let worker = app.clone();
    tauri::async_runtime::spawn(async move {
        let uploads = worker.state::<Uploads>();
        let env = AppEnv(worker.clone());
        {
            // CMP-R110: a cause still present at the start notifies once
            // more.
            let companion = worker.state::<Companion>();
            super::notify(&companion.state, &companion.notifications);
        }
        loop {
            let item = uploads.next().await;
            let companion = worker.state::<Companion>();
            let confirmed = worker.state::<CatchLogs>().confirmed();
            let ctx = Ctx {
                account: companion.account(),
                state: &companion.state,
                version: &companion.version,
                confirmed: &confirmed,
                env: &env,
            };
            uploads.run_item(&ctx, item).await;
        }
    });
    let ticker = app.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(TICK);
            show(&ticker);
        }
    });
}

/// CMP-R45's triggers from the readings, once the Catch logs list has taken
/// `update` in: the first update is the start's (every confirmed `logId`),
/// a later scan's is a scan's (with its skip), and each folder read anew is
/// a debounced change of the `logId` it uploads from.
pub fn after_reading(app: &AppHandle, update: &readings::Update) {
    let uploads = app.state::<Uploads>();
    let confirmed = app.state::<CatchLogs>().confirmed();
    if let Some(trigger) = uploads.reading_trigger(update.scan) {
        trigger_all(app, trigger);
    }
    for folder in &update.read {
        let key = paths::same_folder_key(folder);
        for (log_id, from) in &confirmed {
            if paths::same_folder_key(from) == key {
                trigger_one(app, log_id, Trigger::Change);
            }
        }
    }
}

/// Queues a run of every confirmed `logId` for `trigger`, while linked and
/// outside a stop (CMP-R45, CMP-R55).
pub fn trigger_all(app: &AppHandle, trigger: Trigger) {
    if !may_run(app) {
        return;
    }
    let uploads = app.state::<Uploads>();
    for log_id in app.state::<CatchLogs>().confirmed().keys() {
        uploads.trigger(log_id, trigger);
    }
}

/// Queues a run of `log_id` for `trigger`, while linked and outside a stop
/// (CMP-R45, CMP-R55).
pub fn trigger_one(app: &AppHandle, log_id: &str, trigger: Trigger) {
    if may_run(app) {
        app.state::<Uploads>().trigger(log_id, trigger);
    }
}

fn may_run(app: &AppHandle) -> bool {
    app.try_state::<Companion>().is_some_and(|companion| {
        let account = companion.account();
        account.is_linked() && !account.is_stopped()
    })
}

/// **Replace Pagle's copy with this computer's**, once the window asked its
/// question and the Player chose **Replace** (CMP-R52): sets the whole-send
/// flag, lifts the hold, and starts a run.
#[tauri::command]
pub fn replace_catch_log(app: AppHandle, log_id: String) -> Result<(), String> {
    let companion = app.state::<Companion>();
    if !companion.is_watcher() {
        return Err("Another Pagle Companion is running for this user.".into());
    }
    Uploads::replace(&companion.state, &log_id)?;
    // The divergence's cause has cleared (CMP-R110).
    super::notify(&companion.state, &companion.notifications);
    show(&app);
    trigger_one(&app, &log_id, Trigger::Replace);
    Ok(())
}

/// Shows local state's upload times again: each row's state line, the
/// Status part's "Last uploaded …" and the tray's status line.
pub fn show(app: &AppHandle) {
    catch_logs::window::emit(app);
    let _ = app.emit(LAST_UPLOADED_EVENT, current(app));
    tray::show_tray_status(app);
}

fn current(app: &AppHandle) -> String {
    let last = app.state::<Companion>().state.read(last_uploaded_at);
    last_uploaded_text(last, now())
}

/// The Status part's "Last uploaded …" (CMP UI, Status).
#[tauri::command]
pub fn last_uploaded(app: AppHandle) -> String {
    current(&app)
}

/// What a run in tray mode works with.
struct AppEnv(AppHandle);

impl Env for AppEnv {
    fn read(&self, folder: &str) -> impl Future<Output = Reading> + Send {
        let app = self.0.clone();
        let folder = folder.to_owned();
        async move {
            tauri::async_runtime::spawn_blocking(move || app.state::<Readings>().read_now(&folder))
                .await
                .unwrap_or(Reading::Unreadable)
        }
    }

    fn sleep(&self, wait: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(wait)
    }

    fn now(&self) -> i64 {
        now()
    }

    fn changed(&self) {
        show(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::LogState;

    #[test]
    fn the_status_says_nothing_uploaded_until_something_is() {
        // CMP UI, Status: "Last uploaded {relative time}" or "Nothing
        // uploaded yet"
        assert_eq!(last_uploaded_text(None, 100), "Nothing uploaded yet");
        assert_eq!(
            last_uploaded_text(Some(1_000), 1_000 + 5 * 60),
            "Last uploaded 5 minutes ago"
        );
    }

    #[test]
    fn last_uploaded_is_the_newest_of_every_log() {
        // CMP UI, Status: the window's one "Last uploaded"
        let mut state = State::default();
        for (id, at) in [("a", Some(10)), ("b", Some(30)), ("c", None)] {
            state.logs.insert(
                id.into(),
                LogState {
                    answer: None,
                    folder: None,
                    characters: String::new(),
                    last_folder: String::new(),
                    whole_send_pending: false,
                    last_run: None,
                    last_uploaded_at: at,
                },
            );
        }
        assert_eq!(last_uploaded_at(&state), Some(30));
        assert_eq!(last_uploaded_at(&State::default()), None);
    }
}

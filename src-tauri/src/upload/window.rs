//! Uploading, in tray and headless mode alike: the queue's worker, CMP-R45's triggers (a
//! debounced change, start, later scans, the **Upload** switch, **Upload
//! now**, linking), which queue nothing while not linked or
//! during a stop, and "Last uploaded" in the window's Status and the tray
//! menu (CMP UI).

use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::AppHandle;

use super::queue::Trigger;
use super::{Ctx, Env, Uploads, plan};
use crate::catch_logs::{self, CatchLogs};
use crate::companion::Companion;
use crate::host::Host;
use crate::in_game::folders::{self, DataFile, Qualifying, WriteReport};
use crate::installs::paths;
use crate::readings::{self, Reading, Readings};
use crate::state::State;

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
pub fn start<H: Host>(app: &H) {
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
pub fn after_reading<H: Host>(app: &H, update: &readings::Update) {
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
pub fn trigger_all<H: Host>(app: &H, trigger: Trigger) {
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
pub fn trigger_one<H: Host>(app: &H, log_id: &str, trigger: Trigger) {
    if may_run(app) {
        app.state::<Uploads>().trigger(log_id, trigger);
    }
}

fn may_run<H: Host>(app: &H) -> bool {
    app.try_state::<Companion>().is_some_and(|companion| {
        let account = companion.account();
        account.is_linked() && !account.is_stopped()
    })
}

/// Shows local state's upload times again: each row's state line, the
/// Status part's "Last uploaded …" and the tray's status line.
pub fn show<H: Host>(app: &H) {
    catch_logs::window::emit(app);
    app.emit(LAST_UPLOADED_EVENT, current(app));
    app.show_tray_status();
}

fn current<H: Host>(app: &H) -> String {
    let last = app.state::<Companion>().state.read(last_uploaded_at);
    last_uploaded_text(last, now())
}

/// The Status part's "Last uploaded …" (CMP UI, Status).
#[tauri::command]
pub fn last_uploaded(app: AppHandle) -> String {
    current(&app)
}

/// What a queue item works with: the process's readings, clock and data
/// addon folders.
pub struct AppEnv<H>(pub H);

impl<H: Host> Env for AppEnv<H> {
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

    fn qualifying(&self) -> Qualifying {
        folders::window::qualifying(&self.0)
    }

    fn data_files_hash_to(&self, file: DataFile, etag: &str) -> impl Future<Output = bool> + Send {
        let app = self.0.clone();
        let etag = etag.to_owned();
        async move {
            tauri::async_runtime::spawn_blocking(move || {
                folders::window::data_files_hash_to(&app, file, &etag)
            })
            .await
            .unwrap_or(false)
        }
    }

    fn write(&self, file: DataFile, body: Vec<u8>) -> impl Future<Output = WriteReport> + Send {
        let app = self.0.clone();
        async move {
            tauri::async_runtime::spawn_blocking(move || folders::window::write(&app, file, &body))
                .await
                .unwrap_or_default()
        }
    }

    fn stage_recovery(
        &self,
        log_id: &str,
        folder: &str,
        lua: Vec<u8>,
    ) -> impl Future<Output = Result<bool, String>> + Send {
        let app = self.0.clone();
        let folder = folder.to_owned();
        let log_id = log_id.to_owned();
        async move {
            tauri::async_runtime::spawn_blocking(move || {
                app.state::<folders::Writer>()
                    .stage_recovery(&folder, &lua, || {
                        let companion = app.state::<Companion>();
                        companion.account().is_linked()
                            && !companion.account().is_stopped()
                            && !app.state::<Uploads>().stopping()
                            && companion.state.read(|s| {
                                s.logs.get(&log_id).is_some_and(|log| {
                                    log.answer == Some(crate::state::Answer::Upload)
                                        && paths::same_folder_key(
                                            log.folder.as_deref().unwrap_or(&log.last_folder),
                                        ) == paths::same_folder_key(&folder)
                                })
                            })
                    })
                    .map_err(|e| format!("Couldn't stage Recovery: {e}"))
            })
            .await
            .map_err(|_| "Recovery writer stopped".to_owned())?
        }
    }

    fn retire_recovery(
        &self,
        pending: &crate::recovery::Pending,
    ) -> impl Future<Output = Result<(), String>> + Send {
        let app = self.0.clone();
        let pending = pending.clone();
        async move {
            tauri::async_runtime::spawn_blocking(move || {
                app.state::<folders::Writer>()
                    .retire_recovery(&pending)
                    .map_err(|e| {
                        format!("Couldn't retire completed Recovery; try Upload again: {e}")
                    })
            })
            .await
            .map_err(|_| "Recovery writer stopped".to_owned())?
        }
    }

    fn refresh_tocs(&self) {
        folders::window::refresh_tocs(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::LogState;

    #[tokio::test]
    async fn cmp_r80_changing_selection_while_the_writer_is_busy_cannot_stage_into_the_old_install()
    {
        use std::sync::mpsc;
        use std::task::Poll;

        use crate::host::Parts;
        use crate::readings::{FolderReading, Snapshot, read};
        use crate::state::State;
        use crate::state_dir::StateDir;

        let tmp = tempfile::tempdir().unwrap();
        let log_id = crate::readings::fixture::LOG_A;
        let mut snapshot = Snapshot::default();
        for install in ["one", "two"] {
            let flavor = tmp.path().join(install);
            let folder = flavor.join("WTF/Account/ADA#1");
            crate::readings::fixture::save(
                &folder,
                &crate::readings::fixture::pagle_lua_text(log_id, 1),
            );
            let addon = flavor.join("Interface/AddOns/Pagle");
            std::fs::create_dir_all(&addon).unwrap();
            std::fs::write(addon.join("Pagle.toc"), b"## Interface: 16001\n").unwrap();
            snapshot.folders.push(FolderReading {
                flavor: flavor.to_string_lossy().into_owned(),
                name: "ADA#1".into(),
                path: folder.to_string_lossy().into_owned(),
                polled: false,
                reading: read::read(&folder),
                last_log_id: Some(log_id.into()),
            });
        }
        let host = Parts::default();
        crate::companion::manage(
            &host,
            StateDir::open(tmp.path().join("state")).unwrap(),
            "1.0.0".into(),
            State {
                battle_tag: Some("Ada#1234".into()),
                ..State::default()
            },
            Box::new(crate::notify::Silent),
        )
        .unwrap();
        let companion = host.state::<Companion>();
        let logs = host.state::<CatchLogs>();
        logs.refresh(&companion.state, &companion.notifications, &snapshot);
        logs.answer(
            &companion.state,
            &companion.notifications,
            Some(&snapshot),
            log_id,
            true,
        )
        .unwrap();
        logs.pick(
            &companion.state,
            &companion.notifications,
            &snapshot,
            log_id,
            &snapshot.folders[0].path,
        )
        .unwrap();

        let (held_tx, held_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let blocking_host = host.clone();
        let blocker = std::thread::spawn(move || {
            blocking_host
                .state::<folders::Writer>()
                .stage_recovery("unused", b"", || {
                    held_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    false
                })
        });
        held_rx.await.unwrap();
        let env = AppEnv(host.clone());
        let stage = env.stage_recovery(
            log_id,
            &snapshot.folders[0].path,
            b"private package".to_vec(),
        );
        tokio::pin!(stage);
        std::future::poll_fn(|cx| {
            assert!(stage.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        logs.pick(
            &companion.state,
            &companion.notifications,
            &snapshot,
            log_id,
            &snapshot.folders[1].path,
        )
        .unwrap();
        release_tx.send(()).unwrap();
        assert!(blocker.join().unwrap().is_err());
        assert!(stage.await.is_err());
        for folder in &snapshot.folders {
            assert!(
                !std::path::Path::new(&folder.flavor)
                    .join("Interface/AddOns/Pagle_Recovery")
                    .exists()
            );
        }
        assert!(
            env.stage_recovery(
                log_id,
                &snapshot.folders[1].path,
                b"private package".to_vec(),
            )
            .await
            .unwrap()
        );
        assert_eq!(
            std::fs::read(
                std::path::Path::new(&snapshot.folders[1].flavor)
                    .join("Interface/AddOns/Pagle_Recovery/Recovery.lua"),
            )
            .unwrap(),
            b"private package"
        );
    }

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
                    uploaded_snapshot: None,
                    observed_snapshot: None,
                    recovery: None,
                    recovery_notice: None,
                    last_run: None,
                    last_uploaded_at: at,
                },
            );
        }
        assert_eq!(last_uploaded_at(&state), Some(30));
        assert_eq!(last_uploaded_at(&State::default()), None);
    }
}

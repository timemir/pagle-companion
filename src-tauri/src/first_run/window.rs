//! The first-run window's commands and the effects of its steps (CMP-R100),
//! in tray mode. The window itself is `ui/first-run.js`; the steps are
//! [`super::next`].

use tauri::AppHandle;

use super::{Effect, Event, FirstRun, Step, View};
use crate::autostart;
use crate::catch_logs::{self, CatchLogs};
use crate::companion::Companion;
use crate::host::Host;
use crate::installs::Scanner;
use crate::linking;
use crate::readings::Readings;
use crate::tray;
use crate::upload::{self, Uploads};

/// The event that carries a new [`View`] to the window.
pub const EVENT: &str = "first-run";

/// Sets up the first run of a tray-mode start, before anything is watched:
/// no notification is raised while its window is open (CMP-R100), and the
/// button reads **Continue** if a credential is already in its store.
/// Off a first run, the first-run window is over from the start.
pub fn manage(app: &AppHandle, first_run: bool) {
    app.manage(FirstRun::new(first_run));
    if !first_run {
        return;
    }
    log::info!("first run");
    app.state::<Companion>().notifications.hold();
    let app = app.clone();
    // The store may take a while to answer (the Secret Service).
    std::thread::spawn(move || {
        let stored = app.state::<Companion>().link.has_stored_credential();
        if stored {
            log::info!("first run with a credential already in its store");
        }
        app.state::<FirstRun>().set_credential_stored(stored);
        emit(&app);
    });
}

/// Whether the first-run window is open.
pub fn is_open(app: &AppHandle) -> bool {
    app.try_state::<FirstRun>().is_some_and(|f| f.is_open())
}

/// The window was closed (CMP-R100): before step 1 ends, every found log
/// stays awaiting; after, linking and uploading go on in the background.
pub fn window_closed(app: &AppHandle) {
    if is_open(app) {
        handle(app, Event::Close);
    }
}

/// The link was stored (CMP-R61): step 2 moves on to step 3.
pub fn linked(app: &AppHandle) {
    if is_open(app) {
        handle(app, Event::Linked);
    }
}

fn handle(app: &AppHandle, event: Event) {
    let effects = app.state::<FirstRun>().handle(event);
    for effect in effects {
        run(app, effect);
    }
    emit(app);
}

fn run(app: &AppHandle, effect: Effect) {
    match effect {
        Effect::EndStep1 { answer } => end_step_1(app, answer),
        Effect::StartLink => linking::begin_link(app),
        Effect::ContinueLink => {
            if !app.state::<Companion>().account().continue_link() {
                log::warn!("first run: the stored credential is gone; linking instead");
                handle(app, Event::NoCredential);
            }
        }
        Effect::WaitForRuns => wait_for_runs(app),
        Effect::Release => {
            let shown = app.state::<Companion>().notifications.release();
            log::info!("first-run window closed; {shown} held notifications shown");
        }
    }
}

/// Step 1's end (CMP-R100): the ticks become the answers (unless the window
/// closed early), local state is written with `firstRunDone`, the autostart
/// entry is created (CMP-R3), and the logs listed raise no "Upload it?"
/// notification in this process.
fn end_step_1(app: &AppHandle, answer: bool) {
    let companion = app.state::<Companion>();
    let snapshot = app.state::<Readings>().snapshot();
    let unticked = app.state::<FirstRun>().unticked();
    let written = companion.state.update(|s| {
        let answers = match (&snapshot, answer) {
            (Some(snapshot), true) => super::answers(s, snapshot, &unticked),
            _ => Default::default(),
        };
        for (log_id, answer) in &answers {
            log::info!("Catch log {log_id}: {answer:?} (first run)");
        }
        super::end_step_1(s, &answers);
        s.logs.keys().cloned().collect::<Vec<_>>()
    });
    let listed = match written {
        Ok(listed) => listed,
        Err(e) => {
            log::error!("couldn't write state.json: {e}");
            companion.state.read(|s| s.logs.keys().cloned().collect())
        }
    };
    log::info!(
        "first run: step 1 ended with {} Catch logs listed",
        listed.len()
    );
    autostart::window::create(app);
    app.state::<CatchLogs>()
        .list(listed, &companion.notifications);
    // The confirmed set takes the answers in.
    if let Some(snapshot) = snapshot {
        catch_logs::window::refresh(app, &snapshot);
    }
}

/// Moves on to step 4 once each post-link run has ended (CMP-R100). They
/// were queued before this, as the link was stored.
fn wait_for_runs(app: &AppHandle) {
    let app = app.clone();
    let mut runs = app.state::<Uploads>().post_link_runs();
    tauri::async_runtime::spawn(async move {
        let _ = runs.wait_for(|n| *n == 0).await;
        if app.state::<FirstRun>().step() == Step::Uploading {
            handle(&app, Event::RunsEnded);
        }
    });
}

fn current(app: &AppHandle) -> View {
    let first_run = app.state::<FirstRun>();
    let companion = app.state::<Companion>();
    let credential_stored = first_run.credential_stored();
    let catch_logs = catch_logs::window::current(app);
    let no_install = app
        .state::<Scanner>()
        .snapshot()
        .is_some_and(|s| s.installs.is_empty());
    let step = first_run.step();
    let status = if step == Step::Done {
        super::done_status(
            &companion.account().status().text,
            &upload::window::last_uploaded_text(
                companion.state.read(upload::window::last_uploaded_at),
                upload::window::now(),
            ),
            companion.state.read(|s| s.settings.start_at_login),
        )
    } else {
        Vec::new()
    };
    View {
        step,
        primary: super::primary_text(credential_stored),
        no_install,
        read: catch_logs.read,
        logs: super::tick_rows(catch_logs.logs, &first_run.unticked()),
        status,
    }
}

/// Shows the first-run window again.
pub fn emit(app: &AppHandle) {
    app.emit(EVENT, current(app));
}

#[tauri::command]
pub fn first_run(app: AppHandle) -> View {
    current(&app)
}

/// A tick in step 1's list. A `logId` without a folder pick can't be
/// ticked (CMP-R100).
#[tauri::command]
pub fn tick_catch_log(app: AppHandle, log_id: String, ticked: bool) -> Result<(), String> {
    let view = current(&app);
    let Some(row) = view.logs.iter().find(|r| r.row.log_id == log_id) else {
        return Err("Pagle Companion doesn't know this Catch log.".into());
    };
    if ticked && !row.can_tick {
        return Err("Pick the account folder you play on first.".into());
    }
    app.state::<FirstRun>().tick(&log_id, ticked);
    emit(&app);
    Ok(())
}

/// **Link to Pagle**, or **Continue** with a credential already in its
/// store (CMP-R100).
#[tauri::command]
pub fn first_run_link(app: AppHandle) -> Result<(), String> {
    let first_run = app.state::<FirstRun>();
    if first_run.credential_stored() && first_run.step() == Step::Scan {
        watcher(&app)?;
    } else {
        linking::may_link(&app)?;
    }
    handle(&app, Event::Primary);
    Ok(())
}

/// **Not now**: ends step 1 and skips steps 2 and 3 (CMP-R100).
#[tauri::command]
pub fn first_run_not_now(app: AppHandle) -> Result<(), String> {
    watcher(&app)?;
    if app.state::<FirstRun>().step() == Step::Link {
        // Leaving step 2 ends any attempt under way.
        linking::cancel_link(app.clone());
    }
    handle(&app, Event::NotNow);
    Ok(())
}

/// **Close**: closes the window to the tray (CMP-R100).
#[tauri::command]
pub fn first_run_close(app: AppHandle) {
    if let Some(window) = tauri::Manager::get_webview_window(&app, tray::MAIN_WINDOW) {
        let _ = window.hide();
    }
    window_closed(&app);
}

fn watcher(app: &AppHandle) -> Result<(), String> {
    if app.state::<Companion>().is_watcher() {
        Ok(())
    } else {
        // CMP-R2: the window of a process without the lock is read-only.
        Err("Another Pagle Companion is running for this user.".into())
    }
}

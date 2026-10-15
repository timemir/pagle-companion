//! Tray mode (CMP-R1): the tray icon and its menu, the window, the
//! single-instance channel and the watcher lock (CMP-R2).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};

use crate::api::{self, Api};
use crate::catch_logs::{self, CatchLogs};
use crate::cli::{self, Mode};
use crate::credential_store::Keychain;
use crate::installs::{self, Scanner};
use crate::link::{Account, Hooks, Link, LinkStatus};
use crate::linking::{self, Linking};
use crate::local_log::Logger;
use crate::notify::Notifications;
use crate::os_notify::OsNotifier;
use crate::readings::{self, Readings};
use crate::state::{self, Loaded, State, Store};
use crate::state_dir::{self, StateDir, WatcherLock};
use crate::upload::{self, Uploads, queue::Trigger};

pub const MAIN_WINDOW: &str = "main";

/// How long a start waits for another process to let go of the watcher lock,
/// and how often it tries again after that (CMP Tunables, lock wait).
pub const LOCK_WAIT: Duration = Duration::from_secs(10);

/// What a running tray-mode process does when another tray-mode start passes
/// it its arguments (CMP-R2).
#[derive(Debug, PartialEq, Eq)]
pub enum SecondStart {
    OpenWindow,
    Nothing,
    Quit,
}

/// `argv` is the second start's whole command line, the program first.
pub fn second_start(argv: &[String]) -> SecondStart {
    match cli::parse(argv.get(1..).unwrap_or_default()) {
        Mode::Quit => SecondStart::Quit,
        Mode::Tray { autostart: true } => SecondStart::Nothing,
        Mode::Tray { autostart: false } => SecondStart::OpenWindow,
        // These never pass through the channel: they run as their own
        // process (DST-R14, CMP-R1).
        Mode::Remove | Mode::Headless => SecondStart::Nothing,
    }
}

/// The window opens at start unless the start is `--autostart`, which keeps
/// it closed except on a first run (CMP Contracts, Command line; CMP-R100).
pub fn opens_window_at_start(autostart: bool, first_run: bool) -> bool {
    !autostart || first_run
}

/// What the window shows of this process (`status` command).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub version: String,
    /// Another process holds the watcher lock: this one watches nothing and
    /// the window is read-only (CMP-R2).
    pub read_only: bool,
}

/// The tray-mode process's shared parts, managed by Tauri for every later
/// feature to reach with `app.state::<Companion>()`.
pub struct Companion {
    pub state_dir: StateDir,
    pub version: String,
    pub api: Api,
    pub notifications: Notifications<OsNotifier>,
    /// Local state; change it only through `state.update` (CMP Data).
    pub state: Store,
    /// The link to a Player and its credential; use it through
    /// [`Companion::account`].
    pub link: Link,
    watcher: Mutex<Option<WatcherLock>>,
}

impl Companion {
    /// The link at work: requests that carry the credential
    /// (`account().send(…)`), Unlink, and losing the link (CMP-R49, CMP-R63,
    /// CMP-R65).
    pub fn account(&self) -> Account<'_, OsNotifier> {
        self.link
            .account(&self.api, &self.state, &self.notifications)
    }

    /// Whether this process holds the watcher lock and so may watch (CMP-R2).
    pub fn is_watcher(&self) -> bool {
        self.watcher_slot().is_some()
    }

    pub fn status(&self) -> Status {
        Status {
            version: self.version.clone(),
            read_only: !self.is_watcher(),
        }
    }

    fn watcher_slot(&self) -> MutexGuard<'_, Option<WatcherLock>> {
        self.watcher.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[tauri::command]
fn status(companion: tauri::State<'_, Companion>) -> Status {
    companion.status()
}

/// Runs tray mode (or `--quit`) until the process quits.
pub fn run(mode: Mode) {
    let builder = tauri::Builder::default()
        // Registered first, so a second start hands over and exits before
        // anything else runs.
        .plugin(tauri_plugin_single_instance::init(
            |app, argv, _cwd| match second_start(&argv) {
                SecondStart::OpenWindow => show_main_window(app),
                SecondStart::Quit => app.exit(0),
                SecondStart::Nothing => {}
            },
        ));

    if mode == Mode::Quit {
        // No tray-mode process was running to pass `--quit` to: on its own it
        // exits with status 0 (CMP Contracts, Command line).
        builder
            .setup(|_| std::process::exit(0))
            .run(tauri::generate_context!())
            .expect("the Tauri runtime failed");
        return;
    }
    let autostart = mode == (Mode::Tray { autostart: true });

    let app = builder
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            status,
            installs::window::installs,
            installs::window::add_wow_folder,
            installs::window::remove_wow_folder,
            linking::link_status,
            linking::linking_view,
            linking::start_link,
            linking::cancel_link,
            linking::open_link_page,
            linking::unlink,
            readings::window::readings,
            catch_logs::window::catch_logs,
            catch_logs::window::answer_catch_log,
            catch_logs::window::pick_folder,
            catch_logs::window::open_my_anglers,
            catch_logs::window::open_sign_in,
            upload::window::last_uploaded,
            upload::window::replace_catch_log,
        ])
        .setup(move |app| {
            setup(app.handle(), autostart)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps the process running (CMP-R1).
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("the Tauri runtime failed");

    app.run(|_, event| {
        // Only Quit (an explicit exit code) ends the process.
        if let RunEvent::ExitRequested {
            code: None, api, ..
        } = event
        {
            api.prevent_exit();
        }
    });
}

fn setup(app: &AppHandle, autostart: bool) -> Result<(), Box<dyn std::error::Error>> {
    let state_dir = StateDir::open(state_dir::default_path()?)?;
    Logger::install(state_dir.log_file());
    let version = app.package_info().version.to_string();
    log::info!("Pagle Companion {version} starting in tray mode");

    let state = match state::load(&state_dir.state_file()) {
        Ok(Loaded::State(state)) => state,
        Ok(Loaded::Missing) => State::default(),
        Ok(Loaded::Broken) => {
            log::warn!("state.json couldn't be read; renamed it state.json.broken");
            State::default()
        }
        Err(e) => {
            log::error!("couldn't read state.json: {e}");
            State::default()
        }
    };
    let first_run = !state.first_run_done;

    let hooks = Hooks {
        cancel_queue: {
            let app = app.clone();
            Box::new(move || app.state::<Uploads>().cancel())
        },
        start_post_link_runs: {
            let app = app.clone();
            // CMP-R101: a run for every confirmed logId. The in-game
            // downloads (B4) join here.
            Box::new(move || upload::window::trigger_all(&app, Trigger::PostLink))
        },
        changed: {
            let app = app.clone();
            Box::new(move || linking::link_changed(&app))
        },
        // The data addon folders (B4) plug in here.
        ..Hooks::default()
    };
    app.manage(Companion {
        api: Api::new(&version, api::base_url())?,
        notifications: Notifications::new(OsNotifier::new(app.clone())),
        state: Store::new(state_dir.state_file(), state),
        link: Link::new(Keychain::platform(&state_dir), hooks),
        state_dir,
        version,
        watcher: Mutex::new(None),
    });
    app.manage(Scanner::new());
    app.manage(Linking::default());
    app.manage(Readings::new());
    app.manage(Uploads::new());
    // The first-run window (#9) holds notifications with
    // `catch_logs::window::set_first_run_window`.
    app.manage(CatchLogs::new(catch_logs::Hooks {
        start_run: {
            let app = app.clone();
            Box::new(move |log_id| upload::window::trigger_one(&app, log_id, Trigger::SwitchOn))
        },
        stop_runs: {
            let app = app.clone();
            Box::new(move |log_id| app.state::<Uploads>().stop_runs(log_id))
        },
    }));

    build_tray(app)?;
    linking::start(app);
    if opens_window_at_start(autostart, first_run) {
        show_main_window(app);
    }
    take_watcher_lock(app.clone());
    Ok(())
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "Not linked", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open Pagle Companion", true, None::<&str>)?;
    // Disabled while not linked or during a stop (CMP UI, tray menu).
    let upload_now = MenuItem::with_id(app, "upload-now", "Upload now", false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&status, &open, &upload_now, &quit])?;
    app.manage(TrayItems {
        status,
        upload_now,
        linked: AtomicBool::new(false),
        stop: Mutex::new(None),
    });

    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Pagle Companion")
        .menu(&menu)
        // Menu-driven: Linux trays don't report clicks (CMP UI).
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main_window(app),
            // CMP-R45: every confirmed logId, held back or not, no skip.
            "upload-now" => upload::window::trigger_all(app, Trigger::UploadNow),
            "quit" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

/// The tray menu's items that follow the link.
struct TrayItems {
    status: MenuItem<tauri::Wry>,
    upload_now: MenuItem<tauri::Wry>,
    /// The link as last shown.
    linked: AtomicBool,
    /// The stop message as last shown.
    stop: Mutex<Option<String>>,
}

/// The tray's status line (CMP UI, tray menu): during a stop its stop
/// message, else "Not linked", or the window's "Last uploaded {relative
/// time}" ("Nothing uploaded yet" before anything is).
pub fn tray_status_line(
    linked: bool,
    stop: Option<&str>,
    last_uploaded: Option<i64>,
    now: i64,
) -> String {
    if let Some(stop) = stop {
        stop.to_owned()
    } else if linked {
        upload::window::last_uploaded_text(last_uploaded, now)
    } else {
        "Not linked".into()
    }
}

/// Shows the link in the tray: the status line, and **Upload now**, which is
/// disabled while not linked or during a stop (CMP UI, tray menu).
pub fn show_link(app: &AppHandle, status: &LinkStatus) {
    let Some(items) = app.try_state::<TrayItems>() else {
        return;
    };
    items.linked.store(status.linked, Ordering::SeqCst);
    *items.stop.lock().unwrap_or_else(|e| e.into_inner()) = status.stop.clone();
    let _ = items
        .upload_now
        .set_enabled(status.linked && status.stop.is_none());
    show_tray_status(app);
}

/// Shows the tray's status line again, for the link as last shown.
pub fn show_tray_status(app: &AppHandle) {
    let (Some(items), Some(companion)) =
        (app.try_state::<TrayItems>(), app.try_state::<Companion>())
    else {
        return;
    };
    let last = companion.state.read(upload::window::last_uploaded_at);
    let stop = items.stop.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let line = tray_status_line(
        items.linked.load(Ordering::SeqCst),
        stop.as_deref(),
        last,
        upload::window::now(),
    );
    let _ = items.status.set_text(line);
}

/// Opens the window, creating it on first use, with the webview's data in the
/// state directory (CMP-R1).
pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let Some(companion) = app.try_state::<Companion>() else {
        return;
    };
    let built = WebviewWindowBuilder::new(app, MAIN_WINDOW, WebviewUrl::App("index.html".into()))
        .title("Pagle Companion")
        .inner_size(460.0, 560.0)
        .min_inner_size(360.0, 400.0)
        .data_directory(companion.state_dir.webview_dir())
        .build();
    if let Err(e) = built {
        log::error!("couldn't open the window: {e}");
    }
}

/// Takes the watcher lock, or keeps trying every lock wait. A start that
/// can't take it within one lock wait shows its window read-only (CMP-R2).
fn take_watcher_lock(app: AppHandle) {
    std::thread::spawn(move || {
        let companion = app.state::<Companion>();
        let mut waited = false;
        loop {
            match companion.state_dir.try_lock_watcher() {
                Ok(Some(lock)) => {
                    *companion.watcher_slot() = Some(lock);
                    log::info!("took the watcher lock");
                    installs::window::start(&app);
                    readings::window::start(&app);
                    catch_logs::window::start(&app);
                    upload::window::start(&app);
                    let _ = app.emit("status", companion.status());
                    return;
                }
                Ok(None) => {
                    if waited {
                        log::info!("another Pagle Companion holds the watcher lock");
                        show_main_window(&app);
                        let _ = app.emit("status", companion.status());
                    }
                }
                Err(e) => log::error!("couldn't open watcher.lock: {e}"),
            }
            std::thread::sleep(LOCK_WAIT);
            waited = true;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("pagle-companion")
            .chain(args.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn an_argument_less_second_start_opens_the_window() {
        // CMP-R2: the running process opens its window for an argument-less
        // start
        assert_eq!(second_start(&argv(&[])), SecondStart::OpenWindow);
        assert_eq!(second_start(&argv(&["--unknown"])), SecondStart::OpenWindow);
    }

    #[test]
    fn a_second_autostart_does_nothing() {
        // CMP-R2: does nothing for --autostart
        assert_eq!(second_start(&argv(&["--autostart"])), SecondStart::Nothing);
    }

    #[test]
    fn a_second_start_with_quit_makes_the_running_one_quit() {
        // CMP-R2: quits for --quit
        assert_eq!(second_start(&argv(&["--quit"])), SecondStart::Quit);
        assert_eq!(
            second_start(&argv(&["--autostart", "--quit"])),
            SecondStart::Quit
        );
    }

    #[test]
    fn the_program_name_is_not_an_argument() {
        // CMP-R2: argv carries the program first; a program path that looks
        // like a flag changes nothing
        assert_eq!(
            second_start(&["--quit".to_owned()]),
            SecondStart::OpenWindow
        );
        assert_eq!(second_start(&[]), SecondStart::OpenWindow);
    }

    #[test]
    fn the_window_opens_at_start_unless_autostarted() {
        // CMP Contracts, Command line: none opens the window; --autostart
        // keeps it closed unless it is a first run (CMP-R100)
        assert!(opens_window_at_start(false, false));
        assert!(opens_window_at_start(false, true));
        assert!(!opens_window_at_start(true, false));
        assert!(opens_window_at_start(true, true));
    }

    #[test]
    fn the_tray_says_not_linked_until_linked() {
        // CMP UI, tray menu: the status line reads "Not linked" while not
        // linked
        assert_eq!(tray_status_line(false, None, None, 0), "Not linked");
        assert_eq!(tray_status_line(false, None, Some(0), 600), "Not linked");
        assert_eq!(
            tray_status_line(true, None, None, 0),
            "Nothing uploaded yet"
        );
    }

    #[test]
    fn the_tray_shows_the_stop_message_during_a_stop() {
        // CMP UI, tray menu: a status line "Last uploaded …", "Not linked",
        // or the stop message (CMP-R50)
        let stop = "This Player is suspended. Contact support@pagle.app.";
        assert_eq!(tray_status_line(true, Some(stop), Some(0), 600), stop);
        assert_eq!(tray_status_line(false, Some(stop), None, 0), stop);
    }

    #[test]
    fn the_tray_shows_last_uploaded_once_there_is_one() {
        // CMP UI, tray menu: a status line "Last uploaded {relative time}"
        assert_eq!(
            tray_status_line(true, None, Some(1_000), 1_000 + 2 * 3_600),
            "Last uploaded 2 hours ago"
        );
    }

    #[test]
    fn the_lock_wait_is_ten_seconds() {
        // CMP Tunables: lock wait, 10 s
        assert_eq!(LOCK_WAIT, Duration::from_secs(10));
    }
}

//! Tray mode (CMP-R1): the tray icon and its menu, the window, the
//! single-instance channel and the watcher lock (CMP-R2). The parts it
//! shares with headless mode are `companion`'s.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};

use crate::autostart;
use crate::cli::{self, Mode};
use crate::companion::{self, Companion, Status};
use crate::handover;
use crate::link::LinkStatus;
use crate::local_log::Logger;
use crate::os_notify::OsNotifier;
use crate::state::State;
use crate::state_dir::{self, StateDir};
use crate::upload::{self, queue::Trigger};
use crate::{appimage, catch_logs, in_game, installs, linking, readings, removal};

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
        // DST-R50: its configuration is plugins.updater. Checks and downloads
        // are `updater`'s, in both modes, through the plugin's types.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            status,
            autostart::window::start_at_login,
            autostart::window::set_start_at_login,
            installs::window::installs,
            installs::window::add_wow_folder,
            installs::window::remove_wow_folder,
            in_game::folders::window::in_game_folders,
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
            removal::window::remove_companion,
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

    app.run(|app, event| match event {
        // Only Quit (an explicit exit code) ends the process.
        RunEvent::ExitRequested {
            code: None, api, ..
        } => api.prevent_exit(),
        RunEvent::Exit => {
            if let Some(companion) = app.try_state::<Companion>() {
                companion.release_watcher();
                handover::after_releasing_lock(&companion);
            }
        }
        _ => {}
    });
}

fn setup(app: &AppHandle, autostart: bool) -> Result<(), Box<dyn std::error::Error>> {
    let state_dir = StateDir::open(state_dir::default_path()?)?;
    Logger::install(state_dir.log_file());
    let version = app.package_info().version.to_string();
    log::info!("Pagle Companion {version} starting in tray mode");

    let state = companion::load_state(&state_dir);
    ask_then_start(app, state_dir, version, state, autostart)
}

/// DST-R32: on Linux, the question about moving out of Downloads comes before
/// anything else, first run included (CMP-R100). The rest of the start
/// follows its answer, unless **Move** starts the moved copy and exits.
#[cfg(target_os = "linux")]
fn ask_then_start(
    app: &AppHandle,
    state_dir: StateDir,
    version: String,
    state: State,
    autostart: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let state_file = state_dir.state_file();
    let mode = Mode::Tray { autostart };
    appimage::window::ask_then(app, mode, state_file, state, move |app, state| {
        if let Err(e) = start(app, state_dir, version, state, autostart) {
            log::error!("couldn't start tray mode: {e}");
            app.exit(1);
        }
    });
    Ok(())
}

/// No AppImage, so no question, off Linux.
#[cfg(not(target_os = "linux"))]
fn ask_then_start(
    app: &AppHandle,
    state_dir: StateDir,
    version: String,
    state: State,
    autostart: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    start(app, state_dir, version, state, autostart)
}

/// The rest of a tray-mode start, once DST-R32's question (if any) is
/// answered.
fn start(
    app: &AppHandle,
    state_dir: StateDir,
    version: String,
    state: State,
    autostart: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // DST-R31: at every start.
    appimage::menu::at_start();

    let first_run = !state.first_run_done;
    companion::manage(
        app,
        state_dir,
        version,
        state,
        Box::new(OsNotifier::new(app.clone())),
    )?;

    // CMP-R3, CMP-R91: at every tray-mode start. (`autostart` here is the
    // command line's flag, hence the full path.)
    crate::autostart::window::at_start(&app.state::<Companion>());

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
    let menu = Menu::with_items(app, &[&status, &open, &upload_now])?;
    // Linux only, before Quit (CMP-R71, CMP UI).
    if removal::TRAY_ITEM_OFFERED {
        let remove = MenuItem::with_id(app, "remove", removal::window::ITEM, true, None::<&str>)?;
        menu.append(&remove)?;
    }
    menu.append(&quit)?;
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
            "remove" => removal::window::ask(app),
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
        handover::before_taking_lock(&companion);
        let mut waited = false;
        loop {
            // Removal takes the lock itself (CMP-R71).
            if removal::window::is_removing() {
                return;
            }
            match companion.state_dir.try_lock_watcher() {
                Ok(Some(lock)) => {
                    companion.hold_watcher(lock);
                    log::info!("took the watcher lock");
                    companion::start_watching(&app);
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

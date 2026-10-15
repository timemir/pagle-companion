//! Tray mode (CMP-R1): the tray icon and its menu, the window, the
//! single-instance channel and the watcher lock (CMP-R2). The parts it
//! shares with headless mode are `companion`'s.
//!
//! It installs a waiting update (DST-R54) from **Restart to update**, shown
//! while one waits, and at start once it holds the watcher lock, before
//! anything is watched (`updater::Updater::install`); either way it then
//! exits, without starting the Game Mode unit (CMP-R92).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};

use crate::autostart;
use crate::cli::{self, Mode};
use crate::companion::{self, Companion, Status};
use crate::handover::{self, Exit};
use crate::link::LinkStatus;
use crate::local_log::Logger;
use crate::os_notify::OsNotifier;
use crate::state::State;
use crate::state_dir::{self, StateDir};
use crate::updater::install::{CommandLine, Launch, Launcher, SystemLauncher};
use crate::updater::{Installed, Updater};
use crate::upload::{self, queue::Trigger};
use crate::{
    appimage, catch_logs, first_run, game_mode, in_game, installs, linking, readings, removal,
};

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
                SecondStart::Quit => {
                    // CMP-R92: quitting for --quit doesn't start the unit.
                    handover::note_exit(Exit::QuitArgument);
                    app.exit(0);
                }
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
            quit_companion,
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
            first_run::window::first_run,
            first_run::window::tick_catch_log,
            first_run::window::first_run_link,
            first_run::window::first_run_not_now,
            first_run::window::first_run_close,
            upload::window::last_uploaded,
            upload::window::replace_catch_log,
            removal::window::remove_companion,
            game_mode::window::game_mode,
            game_mode::window::set_game_mode,
            game_mode::window::open_steam_deck_report,
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
                if window.label() == MAIN_WINDOW {
                    // CMP-R100: closing the first-run window.
                    first_run::window::window_closed(window.app_handle());
                }
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
                let exit = if removal::window::is_removing() {
                    Exit::Removal
                } else {
                    handover::noted_exit()
                };
                handover::after_releasing_lock(&companion, exit);
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
    #[cfg(target_os = "linux")]
    quit_on_signals(app);

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
    // CMP-R100: before anything is watched, so nothing notifies while the
    // first-run window is open.
    first_run::window::manage(app, first_run);

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

/// **Quit**: exits, and while the Game Mode toggle is on starts the unit
/// (CMP-R92).
pub fn quit(app: &AppHandle) {
    handover::note_exit(Exit::Quit);
    app.exit(0);
}

/// **Quit** in the window's Settings (CMP UI), the same as the tray's: where
/// no tray is available, Settings is the only way to quit.
#[tauri::command]
fn quit_companion(app: AppHandle) {
    quit(&app);
}

/// CMP-R92: `SIGTERM` or `SIGHUP` (logout, the switch to Game Mode) quits
/// through the same exit path as **Quit**, so the lock is released and the
/// unit started. The handlers are the process's own: a child process starts
/// with the default ones.
#[cfg(target_os = "linux")]
fn quit_on_signals(app: &AppHandle) {
    use tokio::signal::unix::{SignalKind, signal};
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let (mut term, mut hup) = match (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) {
            (Ok(term), Ok(hup)) => (term, hup),
            (Err(e), _) | (_, Err(e)) => {
                log::error!("couldn't listen for SIGTERM and SIGHUP: {e}");
                return;
            }
        };
        let name = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = hup.recv() => "SIGHUP",
        };
        log::info!("quitting on {name}");
        handover::note_exit(Exit::Signal);
        app.exit(0);
    });
}

/// The tray menu's item that installs a waiting update (DST UI, CMP UI).
pub const RESTART_TO_UPDATE: &str = "Restart to update";
const RESTART_TO_UPDATE_ID: &str = "restart-to-update";

/// The tray menu's items by id, in order (CMP UI, tray menu): the status
/// line, **Open**, **Upload now**, **Restart to update** while an update
/// waits (DST-R54), **Remove** where offered (Linux, CMP-R71), **Quit**.
pub fn menu_ids(update_waiting: bool, remove_offered: bool) -> Vec<&'static str> {
    let mut ids = vec!["status", "open", "upload-now"];
    if update_waiting {
        ids.push(RESTART_TO_UPDATE_ID);
    }
    if remove_offered {
        ids.push("remove");
    }
    ids.push("quit");
    ids
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "Not linked", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open Pagle Companion", true, None::<&str>)?;
    // Disabled while not linked or during a stop (CMP UI, tray menu).
    let upload_now = MenuItem::with_id(app, "upload-now", "Upload now", false, None::<&str>)?;
    let restart_item = MenuItem::with_id(
        app,
        RESTART_TO_UPDATE_ID,
        RESTART_TO_UPDATE,
        true,
        None::<&str>,
    )?;
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
        menu: menu.clone(),
        restart_to_update: restart_item,
        update_shown: Mutex::new(false),
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
            RESTART_TO_UPDATE_ID => restart_to_update(app),
            "remove" => removal::window::ask(app),
            "quit" => self::quit(app),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

/// The tray menu's items that change.
struct TrayItems {
    status: MenuItem<tauri::Wry>,
    upload_now: MenuItem<tauri::Wry>,
    /// The link as last shown.
    linked: AtomicBool,
    /// The stop message as last shown.
    stop: Mutex<Option<String>>,
    menu: Menu<tauri::Wry>,
    restart_to_update: MenuItem<tauri::Wry>,
    /// Whether **Restart to update** is in the menu.
    update_shown: Mutex<bool>,
}

/// Shows **Restart to update** in the tray menu while a verified update
/// waits, and takes it out when none does (DST-R54).
pub fn show_waiting_update(app: &AppHandle) {
    let (Some(items), Some(updater)) = (app.try_state::<TrayItems>(), app.try_state::<Updater>())
    else {
        return;
    };
    let waiting = updater.waiting().is_some();
    let mut shown = items.update_shown.lock().unwrap_or_else(|e| e.into_inner());
    if *shown == waiting {
        return;
    }
    let changed = if waiting {
        let at = menu_ids(true, removal::TRAY_ITEM_OFFERED)
            .iter()
            .position(|id| *id == RESTART_TO_UPDATE_ID)
            .unwrap_or_default();
        items.menu.insert(&items.restart_to_update, at)
    } else {
        items.menu.remove(&items.restart_to_update)
    };
    match changed {
        Ok(()) => *shown = waiting,
        Err(e) => log::error!("couldn't change the tray menu: {e}"),
    }
}

/// **Restart to update** (DST-R54): installs the waiting update and exits,
/// abandoning an Upload in flight, which the next start resumes from the
/// server's cursor (DST-R100). An installer that fails its second
/// verification is deleted and the item goes away.
fn restart_to_update(app: &AppHandle) {
    let app = app.clone();
    // Off the main thread: verifying reads the whole installer.
    std::thread::spawn(move || {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let installed = app
            .state::<Updater>()
            .install(&TrayLauncher(app.clone()), &args);
        exit_if_started(&app, &installed);
        show_waiting_update(&app);
    });
}

/// DST-R54, DST-R100: at start, before anything is scanned or uploaded, a
/// waiting update is installed. Returns whether the process is exiting.
fn install_at_start(app: &AppHandle) -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let installed = app
        .state::<Updater>()
        .install_at_start(&TrayLauncher(app.clone()), &args);
    exit_if_started(app, &installed)
}

fn exit_if_started(app: &AppHandle, installed: &Installed) -> bool {
    let Installed::Started(version) = installed else {
        return false;
    };
    log::info!("exiting for update {version}");
    // CMP-R92: the new version starts itself in tray mode, so the unit
    // isn't started.
    handover::note_exit(Exit::Update);
    app.exit(0);
    true
}

/// Starts the new version for real. On Linux it first lets go of the
/// single-instance channel, or the new AppImage would hand over to this
/// process and exit (CMP-R2). On Windows the channel stays, so the
/// installer's `--quit` reaches this process if it hasn't exited yet.
struct TrayLauncher(AppHandle);

impl Launcher for TrayLauncher {
    fn launch(&self, launch: &Launch) -> std::io::Result<()> {
        if let CommandLine::Args(_) = launch.command_line {
            tauri_plugin_single_instance::destroy(&self.0);
        }
        SystemLauncher.launch(launch)
    }
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
        // CMP-R91, then CMP-R92's stop: the unit runs this AppImage.
        game_mode::window::at_start(&companion);
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
                    if install_at_start(&app) {
                        return;
                    }
                    companion::start_watching(&app);
                    let _ = app.emit("status", companion.status());
                    companion.finish_credential_moves();
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
        // CMP UI, tray menu: a status line "Last uploaded â€¦", "Not linked",
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
    fn restart_to_update_shows_only_while_an_update_waits() {
        // DST-R54, DST UI, CMP UI tray menu: Restart to update after Upload
        // now and before Remove and Quit, only while an update waits
        assert_eq!(
            menu_ids(false, false),
            ["status", "open", "upload-now", "quit"]
        );
        assert_eq!(
            menu_ids(true, false),
            ["status", "open", "upload-now", "restart-to-update", "quit"]
        );
        assert_eq!(
            menu_ids(true, true),
            [
                "status",
                "open",
                "upload-now",
                "restart-to-update",
                "remove",
                "quit"
            ]
        );
        assert_eq!(RESTART_TO_UPDATE, "Restart to update");
    }

    #[test]
    fn the_lock_wait_is_ten_seconds() {
        // CMP Tunables: lock wait, 10 s
        assert_eq!(LOCK_WAIT, Duration::from_secs(10));
    }
}

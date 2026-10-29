//! Tray mode (CMP-R1): the tray icon and its menu, the window, the
//! single-instance channel and the watcher lock (CMP-R2). The parts it
//! shares with headless mode are `companion`'s.
//!
//! It installs a waiting update (DST-R54) from **Restart to update**, shown
//! while one waits, in the tray menu and the window (DST-R78), and at start
//! once it holds the watcher lock, before anything is watched, both passive
//! (`updater::Updater::install`, DST-R13); and quietly when idle or while
//! update required (DST-R76, DST-R56: `update_policy`, `update_required`).
//! Each way it then exits, without starting the Game Mode unit (CMP-R92).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::webview::NewWindowResponse;
use tauri::{
    AppHandle, Emitter, Manager, RunEvent, Url, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

use crate::autostart;
use crate::cli::{self, Mode};
use crate::companion::{self, Companion, Status};
use crate::handover::{self, Exit};
use crate::link::LinkStatus;
use crate::local_log::Logger;
use crate::os_notify::OsNotifier;
use crate::state::State;
use crate::state_dir::{self, StateDir};
use crate::update_policy::{self, Policy};
use crate::updater::install::{CommandLine, Launch, Launcher, SystemLauncher, Ui};
use crate::updater::{Installed, Updater};
use crate::upload::{self, queue::Trigger};
use crate::{
    appimage, catch_logs, first_run, game_mode, in_game, installs, linking, readings, recovery,
    removal,
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
            recovery::window::recover_catch_log,
            removal::window::remove_companion,
            game_mode::window::game_mode,
            game_mode::window::set_game_mode,
            game_mode::window::open_steam_deck_report,
            update_policy::window::update_view,
            update_policy::window::restart_to_update,
            update_policy::window::install_updates_automatically,
            update_policy::window::set_install_updates_automatically,
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
    // DST-R76, DST-R56: the installs the Player didn't choose run quietly.
    app.state::<Policy>().install_with({
        let app = app.clone();
        move || install_quietly(&app)
    });
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
        keyring_locked: AtomicBool::new(false),
        catch_logs_need_player: AtomicBool::new(false),
        icon: Mutex::new(None),
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
    let size = tray_icon_size(scale_factor(app));
    match Image::from_bytes(tray_icon_png(Glyph::Normal, size)) {
        Ok(icon) => {
            tray = tray.icon(icon);
            if let Some(items) = app.try_state::<TrayItems>() {
                *items.icon.lock().unwrap_or_else(|e| e.into_inner()) = Some((Glyph::Normal, size));
            }
        }
        Err(e) => {
            log::error!("couldn't read the tray icon: {e}");
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
        }
    }
    tray.build(app)?;
    Ok(())
}

/// The tray glyph's states (CMP UI, tray menu; the design kit's tray set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Normal,
    /// The keyring is locked (CMP-R62), or a Catch log needs the Player.
    Attention,
    /// During a stop (CMP-R50).
    Stopped,
}

/// The tray glyph for what the window shows: a stop before a locked keyring
/// (the Status part's seal, `ui/linking.js`) before a Catch log that needs
/// the Player ([`catch_logs::Line::needs_player`]).
pub fn tray_glyph(stopped: bool, keyring_locked: bool, catch_logs_need_player: bool) -> Glyph {
    if stopped {
        Glyph::Stopped
    } else if keyring_locked || catch_logs_need_player {
        Glyph::Attention
    } else {
        Glyph::Normal
    }
}

/// The tray glyph's size in pixels for a display scale factor: each size is
/// fitted to its own pixel grid, so the nearest is used rather than scaled.
pub fn tray_icon_size(scale: f64) -> u32 {
    if scale >= 1.75 {
        32
    } else if scale >= 1.25 {
        24
    } else {
        16
    }
}

/// The tray glyph's PNG, from `icons/tray/`. A size other than 24 or 32 is
/// 16.
pub fn tray_icon_png(glyph: Glyph, size: u32) -> &'static [u8] {
    match (glyph, size) {
        (Glyph::Normal, 32) => include_bytes!("../icons/tray/tray-32.png"),
        (Glyph::Normal, 24) => include_bytes!("../icons/tray/tray-24.png"),
        (Glyph::Normal, _) => include_bytes!("../icons/tray/tray-16.png"),
        (Glyph::Attention, 32) => include_bytes!("../icons/tray/tray-32-attention.png"),
        (Glyph::Attention, 24) => include_bytes!("../icons/tray/tray-24-attention.png"),
        (Glyph::Attention, _) => include_bytes!("../icons/tray/tray-16-attention.png"),
        (Glyph::Stopped, 32) => include_bytes!("../icons/tray/tray-32-stopped.png"),
        (Glyph::Stopped, 24) => include_bytes!("../icons/tray/tray-24-stopped.png"),
        (Glyph::Stopped, _) => include_bytes!("../icons/tray/tray-16-stopped.png"),
    }
}

/// The primary monitor's scale factor, 1 when there's none to ask.
fn scale_factor(app: &AppHandle) -> f64 {
    match app.primary_monitor() {
        Ok(Some(monitor)) => monitor.scale_factor(),
        _ => 1.0,
    }
}

/// The tray menu's items that change.
struct TrayItems {
    status: MenuItem<tauri::Wry>,
    upload_now: MenuItem<tauri::Wry>,
    /// The link as last shown.
    linked: AtomicBool,
    /// The stop message as last shown.
    stop: Mutex<Option<String>>,
    /// The keyring as last shown (CMP-R62).
    keyring_locked: AtomicBool,
    /// Whether a Catch log needed the Player in the list as last shown.
    catch_logs_need_player: AtomicBool,
    /// The icon's glyph and size, so it's set only when either changes.
    icon: Mutex<Option<(Glyph, u32)>>,
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

/// **Restart to update** (DST-R54), from the tray menu or the window
/// (DST-R78): installs the waiting update passive (DST-R13) and exits,
/// abandoning an Upload in flight, which the next start resumes from the
/// server's cursor (DST-R100). An installer that fails its second
/// verification is deleted and the item and the window's notice go away.
pub fn restart_to_update(app: &AppHandle) {
    let app = app.clone();
    // Off the main thread: verifying reads the whole installer.
    std::thread::spawn(move || {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let installed =
            app.state::<Updater>()
                .install(&TrayLauncher(app.clone()), Ui::Passive, &args);
        exit_if_started(&app, &installed);
        show_waiting_update(&app);
        update_policy::window::refresh(&app);
    });
}

/// The idle install and the install at once under update required (DST-R76,
/// DST-R56): the waiting update, installed quietly with this process's
/// arguments plus `--autostart`, then exit. Called on the updater's thread.
fn install_quietly(app: &AppHandle) -> Installed {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let installed = app
        .state::<Updater>()
        .install(&TrayLauncher(app.clone()), Ui::Quiet, &args);
    exit_if_started(app, &installed);
    show_waiting_update(app);
    installed
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
/// message, else DST-R78's `update` line when it applies, else "Not
/// linked", or the window's "Last uploaded {relative time}" ("Nothing
/// uploaded yet" before anything is).
pub fn tray_status_line(
    linked: bool,
    stop: Option<&str>,
    update: Option<&str>,
    last_uploaded: Option<i64>,
    now: i64,
) -> String {
    if let Some(stop) = stop {
        stop.to_owned()
    } else if let Some(update) = update {
        update.to_owned()
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
    items
        .keyring_locked
        .store(status.keyring_locked, Ordering::SeqCst);
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
        update_policy::window::tray_line(app),
        last,
        upload::window::now(),
    );
    let _ = items.status.set_text(line);
    show_tray_icon(app, &items, stop.is_some());
}

/// Shows in the tray icon whether a Catch log in `view` needs the Player.
pub fn show_catch_logs(app: &AppHandle, view: &catch_logs::View) {
    let Some(items) = app.try_state::<TrayItems>() else {
        return;
    };
    items
        .catch_logs_need_player
        .store(view.needing_player() > 0, Ordering::SeqCst);
    let stopped = items
        .stop
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    show_tray_icon(app, &items, stopped);
}

/// Sets the tray icon's glyph for the link and the list as last shown, at
/// the size for the primary monitor's scale factor; only when either
/// changes, so it doesn't flicker.
fn show_tray_icon(app: &AppHandle, items: &TrayItems, stopped: bool) {
    let glyph = tray_glyph(
        stopped,
        items.keyring_locked.load(Ordering::SeqCst),
        items.catch_logs_need_player.load(Ordering::SeqCst),
    );
    let size = tray_icon_size(scale_factor(app));
    let mut shown = items.icon.lock().unwrap_or_else(|e| e.into_inner());
    if *shown == Some((glyph, size)) {
        return;
    }
    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    let set =
        Image::from_bytes(tray_icon_png(glyph, size)).and_then(|icon| tray.set_icon(Some(icon)));
    match set {
        Ok(()) => *shown = Some((glyph, size)),
        Err(e) => log::error!("couldn't change the tray icon: {e}"),
    }
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
    // `tauri dev` serves the pages from its own server instead; no other
    // build has a dev URL.
    let dev_url = if cfg!(dev) {
        app.config().build.dev_url.clone()
    } else {
        None
    };
    let built = WebviewWindowBuilder::new(app, MAIN_WINDOW, WebviewUrl::App("index.html".into()))
        .title("Pagle Companion")
        .inner_size(460.0, 560.0)
        .min_inner_size(360.0, 400.0)
        .data_directory(companion.state_dir.webview_dir())
        // The window can call the Companion's commands, so it shows only the
        // Companion's own pages; Pagle's open in the browser (CMP-R60).
        .on_navigation(move |url| is_own_page(url) || is_dev_page(dev_url.as_ref(), url))
        .on_new_window(|_, _| NewWindowResponse::Deny)
        .build();
    if let Err(e) = built {
        log::error!("couldn't open the window: {e}");
    }
}

/// Whether the window is open, minimized included: shown, and not closed to
/// the tray (DST-R76). A window whose state can't be read counts as open,
/// so nothing installs under the Player.
pub fn window_open(app: &AppHandle) -> bool {
    app.get_webview_window(MAIN_WINDOW)
        .is_some_and(|window| window.is_visible().unwrap_or(true))
}

/// Whether `url` is a page of `dev_url`'s server, which `tauri dev` serves
/// the window from.
fn is_dev_page(dev_url: Option<&Url>, url: &Url) -> bool {
    dev_url.is_some_and(|dev| dev.origin() == url.origin())
}

/// Whether `url` is one of the Companion's own pages, as Tauri serves them:
/// `tauri://localhost` on Linux, `http://tauri.localhost` (or `https`) on
/// Windows. A page from anywhere else never shows in the window.
pub fn is_own_page(url: &Url) -> bool {
    match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "http" | "https" => url.host_str() == Some("tauri.localhost") && url.port().is_none(),
        _ => false,
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

    #[test]
    fn the_window_shows_only_the_companions_own_pages() {
        let own = |url: &str| is_own_page(&Url::parse(url).unwrap());
        assert!(own("tauri://localhost/index.html"));
        assert!(own("http://tauri.localhost/index.html"));
        assert!(own("https://tauri.localhost/index.html#catch-logs"));
        for other in [
            "https://pagle.app/link",
            "http://tauri.localhost.evil.example/",
            "http://tauri.localhost:8080/",
            "http://localhost/",
            "file:///C:/Windows/System32/calc.exe",
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "about:blank",
        ] {
            assert!(!own(other), "{other}");
        }
    }

    #[test]
    fn tauri_dev_shows_only_its_own_servers_pages() {
        let dev = Url::parse("http://127.0.0.1:1430").unwrap();
        let page = |url: &str| Url::parse(url).unwrap();
        assert!(is_dev_page(
            Some(&dev),
            &page("http://127.0.0.1:1430/index.html")
        ));
        assert!(!is_dev_page(Some(&dev), &page("http://127.0.0.1:1431/")));
        assert!(!is_dev_page(Some(&dev), &page("https://pagle.app/")));
        assert!(!is_dev_page(
            None,
            &page("http://127.0.0.1:1430/index.html")
        ));
    }

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
        assert_eq!(tray_status_line(false, None, None, None, 0), "Not linked");
        assert_eq!(
            tray_status_line(false, None, None, Some(0), 600),
            "Not linked"
        );
        assert_eq!(
            tray_status_line(true, None, None, None, 0),
            "Nothing uploaded yet"
        );
    }

    #[test]
    fn the_tray_shows_the_stop_message_during_a_stop() {
        // CMP UI, tray menu: a status line "Last uploaded â€¦", "Not linked",
        // or the stop message (CMP-R50)
        let stop = "This Player is suspended. Contact support@pagle.app.";
        assert_eq!(tray_status_line(true, Some(stop), None, Some(0), 600), stop);
        assert_eq!(tray_status_line(false, Some(stop), None, None, 0), stop);
    }

    #[test]
    fn the_tray_shows_last_uploaded_once_there_is_one() {
        // CMP UI, tray menu: a status line "Last uploaded {relative time}"
        assert_eq!(
            tray_status_line(true, None, None, Some(1_000), 1_000 + 2 * 3_600),
            "Last uploaded 2 hours ago"
        );
    }

    #[test]
    fn the_update_line_takes_the_place_of_last_uploaded_but_never_of_a_stop() {
        // DST-R78: the tray status line shows "Update ready: restart to
        // update" in place of "Last uploaded …" or "Not linked", never over
        // a stop message (CMP-R50)
        let update = Some(update_policy::TRAY_LINE);
        assert_eq!(update, Some("Update ready: restart to update"));
        assert_eq!(
            tray_status_line(true, None, update, Some(1_000), 2_000),
            "Update ready: restart to update"
        );
        assert_eq!(
            tray_status_line(false, None, update, None, 0),
            "Update ready: restart to update"
        );
        let stop = "This Player is suspended. Contact support@pagle.app.";
        assert_eq!(tray_status_line(true, Some(stop), update, None, 0), stop);
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
    fn the_tray_glyph_shows_a_stop_then_what_needs_the_player() {
        // CMP-R50: stopped during any stop; attention while the keyring is
        // locked (CMP-R62) or a Catch log needs the Player; else normal
        assert_eq!(tray_glyph(false, false, false), Glyph::Normal);
        assert_eq!(tray_glyph(false, true, false), Glyph::Attention);
        assert_eq!(tray_glyph(false, false, true), Glyph::Attention);
        assert_eq!(tray_glyph(false, true, true), Glyph::Attention);
        assert_eq!(tray_glyph(true, false, false), Glyph::Stopped);
        assert_eq!(tray_glyph(true, true, true), Glyph::Stopped);
    }

    #[test]
    fn the_tray_glyph_size_follows_the_scale_factor() {
        // 16 px below 125 %, 24 px below 175 %, then 32 px
        assert_eq!(tray_icon_size(1.0), 16);
        assert_eq!(tray_icon_size(1.24), 16);
        assert_eq!(tray_icon_size(1.25), 24);
        assert_eq!(tray_icon_size(1.5), 24);
        assert_eq!(tray_icon_size(1.75), 32);
        assert_eq!(tray_icon_size(2.0), 32);
        assert_eq!(tray_icon_size(3.0), 32);
        assert_eq!(tray_icon_size(0.0), 16);
    }

    #[test]
    fn each_tray_glyph_is_a_png_of_its_size() {
        // The IHDR's width and height, big-endian at bytes 16 and 20.
        let dimension =
            |png: &[u8], at: usize| u32::from_be_bytes(png[at..at + 4].try_into().unwrap());
        for glyph in [Glyph::Normal, Glyph::Attention, Glyph::Stopped] {
            for size in [16, 24, 32] {
                let png = tray_icon_png(glyph, size);
                assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "{glyph:?} {size}");
                assert_eq!(dimension(png, 16), size, "{glyph:?} {size}");
                assert_eq!(dimension(png, 20), size, "{glyph:?} {size}");
            }
        }
        // Each state has its own glyph.
        assert_ne!(
            tray_icon_png(Glyph::Normal, 16),
            tray_icon_png(Glyph::Attention, 16)
        );
        assert_ne!(
            tray_icon_png(Glyph::Attention, 16),
            tray_icon_png(Glyph::Stopped, 16)
        );
    }

    #[test]
    fn the_lock_wait_is_ten_seconds() {
        // CMP Tunables: lock wait, 10 s
        assert_eq!(LOCK_WAIT, Duration::from_secs(10));
    }
}

//! **Remove Pagle Companion** in tray mode, on Linux only (CMP-R71, CMP
//! UI): the tray menu item and the window's Settings item, each behind
//! CMP-R71's question with **Remove** and **Cancel**. The tray-mode process
//! then runs removal itself ([`Origin::TrayItem`]): it stops watching and
//! the queue, runs CMP-R70's steps holding the watcher lock, deletes the
//! menu entry and, last, the AppImage (DST-R34), and exits.
//!
//! On Windows there is no such item: the uninstaller, which runs
//! `--remove`, is the only way to remove the Companion (DST-R14).

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

use super::{EXIT_STATUS, Origin, OsCommands, System, TRAY_ITEM_OFFERED, execute, plan};
use crate::companion::Companion;
use crate::in_game::folders::Writer;
use crate::linking::Linking;
use crate::upload::Uploads;

/// The item's text, in the tray menu and in Settings (CMP UI).
pub const ITEM: &str = "Remove Pagle Companion";

/// CMP-R71's question, asked before anything is removed.
pub const QUESTION: &str =
    "Remove Pagle Companion? This unlinks this computer and deletes Pagle Companion's settings.";

/// Set once removal has started in this process; it never ends but in exit.
static REMOVING: AtomicBool = AtomicBool::new(false);

/// Whether this process is removing the Companion: it then takes no watcher
/// lock and starts nothing.
pub fn is_removing() -> bool {
    REMOVING.load(Ordering::SeqCst)
}

/// The tray menu's item: asks CMP-R71's question, and removes on **Remove**.
/// **Cancel** changes nothing.
pub fn ask(app: &AppHandle) {
    let handle = app.clone();
    app.dialog()
        .message(QUESTION)
        .title(ITEM)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Remove".into(),
            "Cancel".into(),
        ))
        .show(move |remove| {
            if remove {
                start(&handle);
            }
        });
}

/// The window's **Remove**, once Settings has asked CMP-R71's question.
#[tauri::command]
pub fn remove_companion(app: AppHandle) -> Result<(), String> {
    if !TRAY_ITEM_OFFERED {
        return Err("Uninstall Pagle Companion from Installed apps to remove it.".into());
    }
    start(&app);
    Ok(())
}

/// Runs removal in a thread of its own, once per process, then exits with
/// status 0. It exits directly, not through the tray's exit path, which
/// would start the Game Mode unit (CMP-R92: not when quitting for removal).
fn start(app: &AppHandle) {
    if REMOVING.swap(true, Ordering::SeqCst) {
        return;
    }
    log::info!("removing Pagle Companion from the tray");
    let app = app.clone();
    std::thread::spawn(move || {
        let mut system = for_tray(&app);
        execute(
            &plan(Origin::TrayItem, cfg!(target_os = "linux")),
            &mut system,
        );
        std::process::exit(EXIT_STATUS);
    });
}

/// This process's machine: its state directory, local state, backend and
/// watcher lock, with this OS user's stores, unit, menu entry and AppImage.
fn for_tray(app: &AppHandle) -> System {
    let companion = app.state::<Companion>();
    let state = companion.state.read(Clone::clone);
    let keychain = super::system::platform_keychain(Some(&companion.state_dir));
    let mut system =
        System::new(state, keychain, Box::new(OsCommands::new())).with_this_users_folders();
    system.state_dir = Some(companion.state_dir.clone());
    system.api = Some(companion.api.clone());
    system.lock = companion.take_watcher();
    let handle = app.clone();
    system.stop_watching = Some(Box::new(move || stop_watching(&handle)));
    system
}

/// Stops the queue, the in-game writes and a link under way (CMP-R71).
/// Finding installs and reading go on until exit, but nothing they find is
/// uploaded or written.
fn stop_watching(app: &AppHandle) {
    app.state::<Uploads>().cancel();
    app.state::<Writer>().stop();
    app.state::<Linking>().cancel();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_question_is_cmp_r71s_with_remove_and_cancel_in_settings_too() {
        // CMP-R71, CMP UI: "Remove Pagle Companion? This unlinks this
        // computer and deletes Pagle Companion's settings." with Remove and
        // Cancel, in the tray menu and in the window's Settings
        assert_eq!(
            QUESTION,
            "Remove Pagle Companion? This unlinks this computer and deletes Pagle Companion's settings."
        );
        let page = include_str!("../../../ui/index.html");
        assert!(page.contains(QUESTION));
        assert!(page.contains(&format!(">{ITEM}</button>")));
        assert!(page.contains(">Remove</button>"));
        assert!(page.contains(">Cancel</button>"));
    }

    #[test]
    fn only_linux_offers_the_item() {
        // CMP-R71: on Windows the uninstaller is the only way to remove it
        assert_eq!(TRAY_ITEM_OFFERED, cfg!(target_os = "linux"));
        assert!(!is_removing());
    }
}

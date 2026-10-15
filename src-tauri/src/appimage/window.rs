//! DST-R32's question in tray mode, on Linux (DST UI "Move dialog"): before
//! anything else of the start, first run included (CMP-R100), a dialog asks
//! "Move Pagle Companion to ~/Applications?" with **Move** and **Keep
//! here**. Closing the dialog counts as **Keep here**: on Linux the dialog
//! can't tell the two apart.
//!
//! It runs in Tauri's `setup`, after the single-instance plugin, so a second
//! tray-mode start has already handed over and exited and never asks
//! (CMP-R2). Headless mode never comes here.

use std::path::{Path, PathBuf};
use std::process::Command;

use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

use super::downloads::{self, KEEP_HERE, MOVE, MOVE_FAILED, QUESTION};
use crate::cli::Mode;
use crate::state::State;
use crate::xdg;

/// The dialogs' title.
const TITLE: &str = "Pagle Companion";

/// `$APPIMAGE`, when this start asks DST-R32's question about it: the
/// AppImage is in the XDG download directory (`xdg-user-dir DOWNLOAD`, or
/// `~/Downloads`) or below it, and its path wasn't kept.
pub fn to_ask(mode: Mode, state: &State) -> Option<String> {
    let appimage = xdg::appimage()?;
    let home = dirs::home_dir()?;
    let dir = downloads::download_dir(xdg_user_dir_download().as_deref(), &home);
    // Compared with symbolic links resolved, e.g. a Downloads that links to
    // another disk.
    let dir = dunce::canonicalize(&dir).unwrap_or(dir);
    let canonical = dunce::canonicalize(&appimage).unwrap_or_else(|_| PathBuf::from(&appimage));
    downloads::asks(mode, Some(&appimage), Some(&canonical), &dir, state).then_some(appimage)
}

/// What `xdg-user-dir DOWNLOAD` prints, or `None` when it isn't there or
/// fails.
fn xdg_user_dir_download() -> Option<String> {
    let out = Command::new("xdg-user-dir").arg("DOWNLOAD").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// A tray-mode start: asks DST-R32's question if [`to_ask`] says so, then
/// calls `carry_on` with local state, on the main thread. **Keep here**
/// writes its answer to `state_file` first. **Move** that works starts the
/// moved copy and exits this process instead; a failed move shows
/// [`MOVE_FAILED`] and carries on.
pub fn ask_then<F>(app: &AppHandle, mode: Mode, state_file: PathBuf, state: State, carry_on: F)
where
    F: FnOnce(&AppHandle, State) + Send + 'static,
{
    let Some(appimage) = to_ask(mode, &state) else {
        carry_on(app, state);
        return;
    };
    log::info!("started from Downloads: asking to move to ~/Applications");
    let handle = app.clone();
    app.dialog()
        .message(QUESTION)
        .title(TITLE)
        .buttons(MessageDialogButtons::OkCancelCustom(
            MOVE.into(),
            KEEP_HERE.into(),
        ))
        // On a thread of the dialog's own, so the dialogs below may block.
        .show(move |move_it| {
            let mut state = state;
            if move_it {
                if move_and_restart(&handle, &appimage) {
                    return;
                }
            } else {
                log::info!("Keep here: {appimage}");
                if let Err(e) = downloads::keep_here(&state_file, &mut state, &appimage) {
                    log::error!("couldn't write state.json: {e}");
                }
            }
            let app = handle.clone();
            if let Err(e) = handle.run_on_main_thread(move || carry_on(&app, state)) {
                log::error!("couldn't carry on starting: {e}");
            }
        });
}

/// **Move**: moves the AppImage to `~/Applications`, starts it from there
/// with this start's arguments, and exits. Returns `true` when it is exiting,
/// `false` when this process carries on: the move failed (the Player is told)
/// or the moved copy couldn't be started.
fn move_and_restart(app: &AppHandle, appimage: &str) -> bool {
    let moved = dirs::home_dir()
        .ok_or_else(|| std::io::Error::other("no home folder"))
        .and_then(|home| {
            downloads::move_to(
                Path::new(appimage),
                &downloads::applications_dir(&home),
                |from, to| std::fs::rename(from, to),
            )
        });
    let moved = match moved {
        Ok(moved) => moved,
        Err(e) => {
            log::error!("couldn't move {appimage} to ~/Applications: {e}");
            app.dialog()
                .message(MOVE_FAILED)
                .title(TITLE)
                .kind(MessageDialogKind::Warning)
                .blocking_show();
            return false;
        }
    };
    log::info!("moved to {}; starting it from there", moved.display());
    // Let go of the single-instance channel first, or the moved copy would
    // hand over to this process and exit (CMP-R2).
    tauri_plugin_single_instance::destroy(app);
    let started = Command::new(&moved)
        .args(std::env::args_os().skip(1))
        .current_dir(moved.parent().unwrap_or(Path::new("/")))
        .spawn();
    match started {
        Ok(_) => {
            app.exit(0);
            true
        }
        Err(e) => {
            log::error!("couldn't start {}: {e}", moved.display());
            false
        }
    }
}

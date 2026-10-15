//! The AppImage on a Linux desktop (`companion/distribution.md`, "Linux
//! AppImage"): its menu entry and icon ([`menu`], DST-R31), the question
//! about moving out of Downloads ([`downloads`], DST-R32, asked by
//! `window`), and the WebKit workaround (DST-R33).
//!
//! The decisions (paths, quoting, "is it under Downloads", the move itself)
//! are plain functions over paths, tested over temp folders on every OS.
//! Only `window`, which asks with a dialog and starts the moved copy, is
//! Linux-only. Without `$APPIMAGE` (a development run, or
//! `--appimage-extract-and-run`), and so always on Windows, none of it does
//! anything.
//!
//! Who calls what:
//! - the very start of the process, before any thread: [`webkit_workaround`]
//!   (DST-R33);
//! - a tray-mode start, before anything else, first run included: the move
//!   question, `window::ask_then` (DST-R32, CMP-R100);
//! - every tray-mode and headless start: [`menu::at_start`] (DST-R31);
//! - removal's Linux tray item: [`menu::MenuEntry::delete`] (DST-R34).

use std::ffi::OsStr;

pub mod downloads;
pub mod menu;
#[cfg(target_os = "linux")]
pub mod window;

/// The variable DST-R33 sets, against the EGL failures of Ubuntu-built Tauri
/// AppImages on Arch-based hosts.
pub const DMABUF_VAR: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";

/// What to set [`DMABUF_VAR`] to, given its current value: `1`, unless it is
/// already set, in which case it keeps its value (DST-R33).
pub fn dmabuf_value(current: Option<&OsStr>) -> Option<&'static str> {
    match current {
        Some(_) => None,
        None => Some("1"),
    }
}

/// DST-R33: sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` for this process unless
/// it is already set, so the webview, which starts later, reads it.
///
/// Call it first thing in `main`, before any thread starts: changing the
/// environment while another thread reads it is undefined behaviour.
#[cfg(target_os = "linux")]
pub fn webkit_workaround() {
    if let Some(value) = dmabuf_value(std::env::var_os(DMABUF_VAR).as_deref()) {
        // SAFETY: called from `run` before any thread is started, so nothing
        // reads the environment meanwhile.
        unsafe { std::env::set_var(DMABUF_VAR, value) };
    }
}

/// DST-R33 is Linux only.
#[cfg(not(target_os = "linux"))]
pub fn webkit_workaround() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dmabuf_renderer_is_turned_off_unless_already_set() {
        // DST-R33: WEBKIT_DISABLE_DMABUF_RENDERER=1 unless the variable is
        // already set, in which case it keeps its value
        assert_eq!(DMABUF_VAR, "WEBKIT_DISABLE_DMABUF_RENDERER");
        assert_eq!(dmabuf_value(None), Some("1"));
        assert_eq!(dmabuf_value(Some(OsStr::new("0"))), None);
        assert_eq!(dmabuf_value(Some(OsStr::new(""))), None);
    }
}

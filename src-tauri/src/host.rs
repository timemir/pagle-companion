//! Where the watcher's parts live and where their changes are shown. Tray
//! mode keeps the parts in Tauri's managed state and shows changes in its
//! window and tray. Headless mode (CMP-R1) keeps them in [`Parts`] and shows
//! nothing:
//! it starts no Tauri runtime at all, because Tauri's Linux runtime starts
//! GTK, and GTK can't start without a display, which a `systemd --user` unit
//! in Game Mode or after logout doesn't have (CMP-R90).
//!
//! The method names are Tauri's own, so the glue that starts and joins the
//! parts (`installs::window`, `upload::window` and the rest) reads the same
//! in both modes. Headless code never names a Tauri window or tray type,
//! which on Windows also keeps them out of the test binary: Tauri's tray
//! menu needs the app's manifest to load.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::Serialize;
use tauri::AppHandle;

use crate::link::LinkStatus;
use crate::tray;

/// The process the watcher's parts run in.
pub trait Host: Clone + Send + Sync + 'static {
    /// Keeps `part` for the life of the process, unless one of its type is
    /// kept already. Returns whether it was kept.
    fn manage<T: Send + Sync + 'static>(&self, part: T) -> bool;

    /// The part of type `T`, if this process keeps one.
    fn try_state<T: Send + Sync + 'static>(&self) -> Option<&T>;

    /// The part of type `T`.
    ///
    /// # Panics
    ///
    /// If this process keeps none, as Tauri's `state` does.
    fn state<T: Send + Sync + 'static>(&self) -> &T {
        self.try_state()
            .unwrap_or_else(|| panic!("no {} kept in this process", std::any::type_name::<T>()))
    }

    /// Shows a change in the window. Without a window it does nothing.
    fn emit<S: Serialize + Clone>(&self, event: &str, payload: S);

    /// Shows the link in the tray. Without a tray it does nothing.
    fn show_link(&self, status: &LinkStatus);

    /// Shows the tray's status line again. Without a tray it does nothing.
    fn show_tray_status(&self);
}

impl Host for AppHandle {
    fn manage<T: Send + Sync + 'static>(&self, part: T) -> bool {
        tauri::Manager::manage(self, part)
    }

    fn try_state<T: Send + Sync + 'static>(&self) -> Option<&T> {
        tauri::Manager::try_state::<T>(self).map(|state| state.inner())
    }

    fn emit<S: Serialize + Clone>(&self, event: &str, payload: S) {
        // A window that isn't open misses nothing: it reads the parts anew
        // when it opens.
        let _ = tauri::Emitter::emit(self, event, payload);
    }

    fn show_link(&self, status: &LinkStatus) {
        tray::show_link(self, status);
    }

    fn show_tray_status(&self) {
        tray::show_tray_status(self);
    }
}

/// Headless mode's parts: one of each type, kept until the process exits,
/// and no window or tray to show changes in (CMP-R1).
#[derive(Clone, Default)]
pub struct Parts {
    kept: Arc<RwLock<HashMap<TypeId, &'static (dyn Any + Send + Sync)>>>,
}

impl Host for Parts {
    fn manage<T: Send + Sync + 'static>(&self, part: T) -> bool {
        let mut kept = self.kept.write().unwrap_or_else(|e| e.into_inner());
        if kept.contains_key(&TypeId::of::<T>()) {
            return false;
        }
        // Kept for the life of the process, as Tauri keeps managed state.
        kept.insert(TypeId::of::<T>(), Box::leak(Box::new(part)));
        true
    }

    fn try_state<T: Send + Sync + 'static>(&self) -> Option<&T> {
        let kept = self.kept.read().unwrap_or_else(|e| e.into_inner());
        let part: &'static (dyn Any + Send + Sync) = *kept.get(&TypeId::of::<T>())?;
        part.downcast_ref()
    }

    fn emit<S: Serialize + Clone>(&self, _event: &str, _payload: S) {}

    fn show_link(&self, _status: &LinkStatus) {}

    fn show_tray_status(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_parts_keep_one_of_each_type() {
        // CMP-R1: headless mode runs the same parts as tray mode, kept in the
        // process instead of in Tauri's state
        let parts = Parts::default();
        assert!(parts.try_state::<String>().is_none());
        assert!(parts.manage(String::from("first")));
        assert!(!parts.manage(String::from("second")));
        assert!(parts.manage(7_u32));

        let copy = parts.clone();
        assert_eq!(copy.state::<String>(), "first");
        assert_eq!(*copy.state::<u32>(), 7);
    }

    #[test]
    fn headless_mode_shows_nothing() {
        // CMP-R1: no window in headless mode; showing a change does nothing
        Parts::default().emit("status", "anything");
    }
}

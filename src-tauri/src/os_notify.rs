//! OS notifications whose click opens the window (CMP-R110): Windows toasts
//! and freedesktop notifications on Linux.

use tauri::AppHandle;

use crate::notify::Notifier;
use crate::tray;

const TITLE: &str = "Pagle Companion";

pub struct OsNotifier {
    app: AppHandle,
}

impl OsNotifier {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[cfg(windows)]
impl Notifier for OsNotifier {
    fn show(&self, text: &str) {
        use tauri_winrt_notification::Toast;

        // A toast needs an app user model ID. The installer gives the Start
        // menu shortcut the identifier; a development run has no shortcut and
        // borrows PowerShell's, as Tauri's notification plugin does.
        let app_id = if cfg!(debug_assertions) {
            Toast::POWERSHELL_APP_ID.to_owned()
        } else {
            self.app.config().identifier.clone()
        };
        let app = self.app.clone();
        let shown = Toast::new(&app_id)
            .title(TITLE)
            .text1(text)
            .on_activated(move |_| {
                tray::show_main_window(&app);
                Ok(())
            })
            .show();
        if let Err(e) = shown {
            log::warn!("couldn't show a notification: {e}");
        }
    }
}

#[cfg(target_os = "linux")]
impl Notifier for OsNotifier {
    fn show(&self, text: &str) {
        let app = self.app.clone();
        let text = text.to_owned();
        // Waiting for the click blocks, so each notification gets a thread.
        std::thread::spawn(move || {
            let shown = notify_rust::Notification::new()
                .appname(TITLE)
                .summary(TITLE)
                .body(&text)
                .action("default", "Open Pagle Companion")
                .show();
            match shown {
                Ok(handle) => handle.wait_for_action(|action| {
                    if action == "default" {
                        tray::show_main_window(&app);
                    }
                }),
                Err(e) => log::warn!("couldn't show a notification: {e}"),
            }
        });
    }
}

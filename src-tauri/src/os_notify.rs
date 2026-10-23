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
            let markup = notify_rust::get_capabilities()
                .is_ok_and(|caps| caps.iter().any(|cap| cap == "body-markup"));
            let body = if markup { escape_markup(&text) } else { text };
            let shown = notify_rust::Notification::new()
                .appname(TITLE)
                .summary(TITLE)
                .body(&body)
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

/// `text` as the body of a notification on a server that reads markup in it
/// (the freedesktop `body-markup` capability): `&`, `<` and `>` as entities.
/// The text holds character names read from `Pagle.lua` and the server's
/// refusal messages (CMP-R111), which show as written, never as a link or
/// an image someone else chose.
#[cfg(any(test, target_os = "linux"))]
fn escape_markup(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            c => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_in_a_notification_shows_as_written() {
        // CMP-R111: "Found a Catch log: {characters}. Upload it?"
        assert_eq!(
            escape_markup(
                r#"Found a Catch log: <a href="https://evil.example">Ada</a> & Bo. Upload it?"#
            ),
            r#"Found a Catch log: &lt;a href="https://evil.example"&gt;Ada&lt;/a&gt; &amp; Bo. Upload it?"#
        );
        assert_eq!(escape_markup("Ada Brook"), "Ada Brook");
    }
}

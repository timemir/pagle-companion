//! The update policy in tray and headless mode alike: the updater's hooks
//! into [`Policy`], what its decisions act on in this process, and the
//! window's commands: the nudge's view, its **Restart to update**, the
//! "Install updates automatically" switch and **Check for updates**
//! (`ui/update.js`, `ui/settings.js`). Like the updater, the hooks run only in the process
//! that holds the watcher lock (CMP-R2).

use semver::Version;
use tauri::AppHandle;

use super::{Busy, CheckLine, CheckView, Env, Install, Nudge, Policy, UpdateView};
use crate::companion::Companion;
use crate::host::Host;
use crate::linking::Linking;
use crate::notify::Cause;
use crate::updater::{Checked, Installed, Updater};
use crate::upload::Uploads;

/// The window's event for a changed [`UpdateView`].
pub const EVENT: &str = "update";

/// The window's event for a changed [`CheckView`] (DST-R102).
pub const CHECK_EVENT: &str = "update-check";

/// The updater's hook after every check (DST-R51).
pub fn checked<H: Host>(host: &H, checked: &Checked) {
    if let Some(policy) = host.try_state::<Policy>() {
        policy.checked(&AppEnv(host), checked);
    }
}

/// The updater's hook at every update tick (DST-R51): the idle install and
/// the nudge.
pub fn tick<H: Host>(host: &H) {
    if let Some(policy) = host.try_state::<Policy>() {
        policy.tick(&AppEnv(host));
    }
}

/// Shows the nudge again, after the waiting update changed outside a tick
/// (**Restart to update** whose installer failed its second verification).
pub fn refresh<H: Host>(host: &H) {
    if let Some(policy) = host.try_state::<Policy>() {
        policy.show(&AppEnv(host));
    }
}

/// The forced install under update required (DST-R56), for
/// `update_required`. Returns whether the process is exiting for it.
pub fn install_now<H: Host>(host: &H) -> bool {
    host.try_state::<Policy>()
        .is_some_and(|policy| matches!(policy.install(Install::Forced), Installed::Started(_)))
}

/// A config read's `recommendedCompanionVersion` (DST-R63).
pub fn recommend<H: Host>(host: &H, version: &Version) {
    if let Some(policy) = host.try_state::<Policy>() {
        policy.recommend(version);
    }
}

/// The tray's status line for the nudge as last shown, if it has one.
pub fn tray_line<H: Host>(host: &H) -> Option<&'static str> {
    host.try_state::<Policy>()
        .and_then(|policy| policy.shown().tray_line())
}

struct AppEnv<'a, H>(&'a H);

impl<H: Host> AppEnv<'_, H> {
    fn companion(&self) -> &Companion {
        self.0.state::<Companion>()
    }
}

impl<H: Host> Env for AppEnv<'_, H> {
    fn update_waiting(&self) -> bool {
        self.0
            .try_state::<Updater>()
            .is_some_and(|u| u.waiting().is_some())
    }

    fn update_required(&self) -> bool {
        self.companion().account().update_required()
    }

    fn installs_automatically(&self) -> bool {
        self.companion()
            .state
            .read(|s| s.settings.install_updates_automatically)
    }

    fn busy(&self) -> Busy {
        Busy {
            upload_run: self
                .0
                .try_state::<Uploads>()
                .is_some_and(Uploads::run_under_way),
            linking: self
                .0
                .try_state::<Linking>()
                .is_some_and(Linking::under_way),
            window_open: self.0.window_open(),
        }
    }

    fn install(&self, why: Install) -> Installed {
        self.0
            .try_state::<Policy>()
            .map_or(Installed::NothingWaiting, |policy| policy.install(why))
    }

    fn show(&self, nudge: Nudge) {
        self.0.emit(EVENT, UpdateView::from(nudge));
        self.0.show_tray_status();
    }

    fn notify(&self) {
        // Held while the first-run window is open (CMP-R100).
        self.companion()
            .notifications
            .raise(&Cause::UpdateRecommended);
    }

    fn show_check(&self, line: &CheckLine) {
        self.0.emit(CHECK_EVENT, CheckView::from(line));
    }
}

/// The window's notice in Status: what to show of a waiting update.
#[tauri::command]
pub fn update_view(policy: tauri::State<'_, Policy>) -> UpdateView {
    policy.shown().into()
}

/// The window's **Restart to update**: the tray item's path, passive
/// (DST-R54, DST-R13).
#[tauri::command]
pub fn restart_to_update(app: AppHandle) {
    crate::tray::restart_to_update(&app);
}

/// **Check for updates** (DST-R102): an update check at the next update
/// tick, the same check as the scheduled one, which starts the update
/// interval again. Returns the button's view as it now is; the error is the
/// text to show.
#[tauri::command]
pub fn check_for_updates(app: AppHandle) -> Result<CheckView, String> {
    if !app.state::<Companion>().is_watcher() {
        // CMP-R2: the window of a process without the lock is read-only, and
        // only the process with the lock checks for updates.
        return Err("Another Pagle Companion is running for this user.".into());
    }
    let (Some(policy), Some(updater)) = (app.try_state::<Policy>(), app.try_state::<Updater>())
    else {
        return Err(super::CHECK_FAILED_TEXT.into());
    };
    if policy.ask_check(&AppEnv(&app)) {
        log::info!("the Player asked for an update check");
        updater.check_soon();
    }
    Ok(CheckView::from(&policy.check_line()))
}

/// The line beside **Check for updates**, as last shown.
#[tauri::command]
pub fn update_check_view(policy: tauri::State<'_, Policy>) -> CheckView {
    CheckView::from(&policy.check_line())
}

/// The "Install updates automatically" switch: `settings
/// .installUpdatesAutomatically`.
#[tauri::command]
pub fn install_updates_automatically(companion: tauri::State<'_, Companion>) -> bool {
    companion
        .state
        .read(|s| s.settings.install_updates_automatically)
}

/// The window's "Install updates automatically" switch. Returns the switch
/// as it now is; the error is the text to show. A write that fails keeps
/// the setting in memory, so it holds for this run and the next write takes
/// it along (CMP Data).
#[tauri::command]
pub fn set_install_updates_automatically(
    companion: tauri::State<'_, Companion>,
    on: bool,
) -> Result<bool, String> {
    if !companion.is_watcher() {
        // CMP-R2: the window of a process without the lock is read-only.
        return Err("Another Pagle Companion is running for this user.".into());
    }
    let written = companion
        .state
        .update(|s| s.settings.install_updates_automatically = on);
    if let Err(e) = written {
        log::error!("couldn't write state.json: {e}");
    }
    log::info!(
        "Install updates automatically turned {}",
        if on { "on" } else { "off" }
    );
    Ok(on)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_has_the_switch_under_start_at_login_and_the_notice_in_status() {
        // DST-R77, DST-R78: "Install updates automatically", a switch in
        // Settings under "Start at login"; the notice and its Restart to
        // update in Status; both scripts call their commands
        let page = include_str!("../../../ui/index.html");
        let at_login = page.find(">Start at login<").unwrap();
        let setting = page
            .find(&format!(">{}<", super::super::SETTING_TEXT))
            .unwrap();
        let game_mode = page.find("id=\"game-mode-setting\"").unwrap();
        assert!(at_login < setting && setting < game_mode);
        assert!(page.contains("id=\"install-updates-automatically\""));

        let status = page.find("id=\"status\"").unwrap();
        let notice = page.find("id=\"update-ready\"").unwrap();
        let linking = page.find("id=\"linking\"").unwrap();
        assert!(status < notice && notice < linking);
        assert!(page.contains(&format!(">{}</button>", crate::tray::RESTART_TO_UPDATE)));
        assert!(page.contains("<script src=\"update.js\" defer></script>"));

        let script = include_str!("../../../ui/update.js");
        assert!(script.contains("\"update_view\""));
        assert!(script.contains("\"restart_to_update\""));
        assert!(script.contains(&format!("\"{EVENT}\"")));
        let settings = include_str!("../../../ui/settings.js");
        assert!(settings.contains("\"set_install_updates_automatically\""));
    }

    #[test]
    fn check_for_updates_is_the_last_thing_in_settings() {
        // DST-R102: a button at the bottom of Settings, after the switches
        // and before Quit, with its line beside it; settings.js calls its
        // commands and hears its event
        let page = include_str!("../../../ui/index.html");
        let game_mode = page.find("id=\"game-mode-setting\"").unwrap();
        let button = page
            .find(&format!(">{}</button>", super::super::CHECK_BUTTON))
            .unwrap();
        let line = page.find("id=\"update-check-line\"").unwrap();
        let error = page.find("id=\"settings-error\"").unwrap();
        let quit = page.find("id=\"quit\"").unwrap();
        assert!(game_mode < button && button < line && line < error && error < quit);
        assert!(page.contains("id=\"check-for-updates\""));

        let settings = include_str!("../../../ui/settings.js");
        assert!(settings.contains("\"check_for_updates\""));
        assert!(settings.contains("\"update_check_view\""));
        assert!(settings.contains(&format!("\"{CHECK_EVENT}\"")));
    }
}

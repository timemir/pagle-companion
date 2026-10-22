//! What tray mode and headless mode share (CMP-R1): the [`Companion`] with
//! local state, the link and the notifications; the parts that find
//! installs, read, upload and write in-game files, and the updater, set up
//! by [`manage`]; and [`start_watching`], which only the holder of the
//! watcher lock runs (CMP-R2). Each mode keeps the parts in its own [`Host`].

use std::sync::{Mutex, MutexGuard};

use serde::Serialize;

use crate::api::{self, Api};
use crate::catch_logs::{self, CatchLogs};
use crate::credential_store::Keychain;
use crate::host::Host;
use crate::in_game::{self, folders::Writer};
use crate::installs::{self, Scanner};
use crate::link::{Account, Hooks, Link};
use crate::linking::{self, Linking};
use crate::notify::{Notifications, Notifier};
use crate::readings::{self, Readings};
use crate::state::{self, Loaded, State, Store};
use crate::state_dir::{StateDir, WatcherLock};
use crate::update_required::{self, window::Required};
use crate::updater::{self, Updater};
use crate::upload::{self, Uploads, queue::Trigger};

/// What the window shows of this process (`status` command).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub version: String,
    /// Another process holds the watcher lock: this one watches nothing and
    /// the window is read-only (CMP-R2).
    pub read_only: bool,
    /// Settings offer **Remove Pagle Companion**: on Linux only (CMP-R71).
    pub can_remove: bool,
}

/// The process's shared parts, for every feature to reach with
/// `host.state::<Companion>()`.
pub struct Companion {
    pub state_dir: StateDir,
    pub version: String,
    pub api: Api,
    /// The OS notifications in tray mode, none in headless mode (CMP-R1).
    pub notifications: Notifications<Box<dyn Notifier>>,
    /// Local state; change it only through `state.update` (CMP Data).
    pub state: Store,
    /// The link to a Player and its credential; use it through
    /// [`Companion::account`].
    pub link: Link,
    watcher: Mutex<Option<WatcherLock>>,
}

impl Companion {
    /// The link at work: requests that carry the credential
    /// (`account().send(…)`), Unlink, and losing the link (CMP-R49, CMP-R63,
    /// CMP-R65).
    pub fn account(&self) -> Account<'_, Box<dyn Notifier>> {
        self.link
            .account(&self.api, &self.state, &self.notifications)
    }

    /// Whether this process holds the watcher lock and so may watch (CMP-R2).
    pub fn is_watcher(&self) -> bool {
        self.watcher_slot().is_some()
    }

    /// Keeps the watcher lock this process took. Only now does it write
    /// local state (CMP-R2).
    pub fn hold_watcher(&self, lock: WatcherLock) {
        *self.watcher_slot() = Some(lock);
        if let Err(e) = self.state.take_writes() {
            log::error!("couldn't write state.json: {e}");
        }
    }

    /// Takes the watcher lock out of this process's keeping, for removal on
    /// Linux, which holds it until it deletes the state directory (CMP-R71).
    pub fn take_watcher(&self) -> Option<WatcherLock> {
        self.state.give_up_writes();
        self.watcher_slot().take()
    }

    /// Lets go of the watcher lock, so another process can take it (CMP-R2,
    /// CMP-R92).
    pub fn release_watcher(&self) {
        self.state.give_up_writes();
        if self.watcher_slot().take().is_some() {
            log::info!("released the watcher lock");
        }
    }

    /// CMP-R62: at a start, a delete from the store the credential moved
    /// out of that failed is tried again. The watcher runs it: only it links
    /// or moves the credential (CMP-R2).
    pub fn finish_credential_moves(&self) {
        let kind = self.state.read(|s| s.credential_store);
        for failed in self.link.keychain().delete_from_other_stores(kind) {
            log::info!(
                "couldn't delete the credential from its old store; trying again at the next start: {failed}"
            );
        }
    }

    pub fn status(&self) -> Status {
        Status {
            version: self.version.clone(),
            read_only: !self.is_watcher(),
            can_remove: crate::removal::TRAY_ITEM_OFFERED,
        }
    }

    fn watcher_slot(&self) -> MutexGuard<'_, Option<WatcherLock>> {
        self.watcher.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Local state as the start finds it: a missing or broken `state.json`
/// starts from nothing (CMP Data).
pub fn load_state(state_dir: &StateDir) -> State {
    match state::load(&state_dir.state_file()) {
        Ok(Loaded::State(state)) => state,
        Ok(Loaded::Missing) => State::default(),
        Ok(Loaded::Broken) => {
            log::warn!("state.json couldn't be read; renamed it state.json.broken");
            State::default()
        }
        Err(e) => {
            log::error!("couldn't read state.json: {e}");
            State::default()
        }
    }
}

/// Sets up every part in `host`: the [`Companion`] and the parts that find,
/// read, upload and write. Nothing starts until [`start_watching`].
pub fn manage<H: Host>(
    host: &H,
    state_dir: StateDir,
    version: String,
    state: State,
    notifier: Box<dyn Notifier>,
) -> Result<(), api::ApiError> {
    let hooks = Hooks {
        cancel_queue: {
            let host = host.clone();
            Box::new(move || host.state::<Uploads>().cancel())
        },
        start_post_link_runs: {
            let host = host.clone();
            // CMP-R101: a run for every confirmed logId, and both in-game
            // downloads, which queue behind the runs (CMP-R46).
            Box::new(move || {
                upload::window::trigger_all(&host, Trigger::PostLink);
                in_game::timer::window::after_link(&host);
            })
        },
        changed: {
            let host = host.clone();
            Box::new(move || linking::link_changed(&host))
        },
        // CMP-R85: both folders, in every install it knows.
        delete_data_addon_folders: {
            let host = host.clone();
            Box::new(move || in_game::folders::window::delete_folders(&host))
        },
        // DST-R56: an update check at once, and linking stops.
        entered_update_required: {
            let host = host.clone();
            Box::new(move || update_required::window::entered(&host))
        },
    };
    let updater_hooks = updater::Hooks {
        // Restart to update shows while an update waits (DST-R54); then the
        // config read that follows each check (DST-R51).
        after_check: {
            let host = host.clone();
            Box::new(move |checked| {
                host.show_waiting_update();
                update_required::window::after_check(&host, checked);
            })
        },
        // DST-R56: the required-state config reads, by the update tick.
        ticked: {
            let host = host.clone();
            Box::new(move |now| update_required::window::tick(&host, now))
        },
    };
    host.manage(Updater::new(&state_dir, &version, updater_hooks).map_err(api::ApiError::Client)?);
    host.manage(Required::new(&version));
    host.manage(Companion {
        api: Api::new(&version, api::base_url())?,
        notifications: Notifications::new(notifier),
        // Written only while this process holds the watcher lock (CMP-R2).
        state: Store::until_watcher(state_dir.state_file(), state),
        link: Link::new(Keychain::platform(&state_dir), hooks),
        state_dir,
        version,
        watcher: Mutex::new(None),
    });
    host.manage(Scanner::new());
    host.manage(Writer::new());
    host.manage(Linking::default());
    host.manage(Readings::new());
    host.manage(Uploads::new());
    host.manage(CatchLogs::new(catch_logs::Hooks {
        start_run: {
            let host = host.clone();
            Box::new(move |log_id| upload::window::trigger_one(&host, log_id, Trigger::SwitchOn))
        },
        stop_runs: {
            let host = host.clone();
            Box::new(move |log_id| host.state::<Uploads>().stop_runs(log_id))
        },
    }));
    Ok(())
}

/// Starts finding installs, watching, reading, uploading and writing in-game
/// files, for this process, which holds the watcher lock (CMP-R2).
pub fn start_watching<H: Host>(host: &H) {
    installs::window::start(host);
    in_game::folders::window::start(host);
    readings::window::start(host);
    catch_logs::window::start(host);
    upload::window::start(host);
    in_game::timer::window::start(host);
    // DST-R51: update checks in every mode that watches, tray and headless.
    updater::start(host);
}

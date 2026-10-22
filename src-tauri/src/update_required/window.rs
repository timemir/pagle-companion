//! Update required in tray and headless mode alike: the [`Required`] part,
//! the updater's and the link's hooks into it, and what its decisions act on
//! in this process. "Every mode" (DST-R51) is the process that holds the
//! watcher lock: the updater, and so these hooks, run only there (CMP-R2).

use std::sync::OnceLock;
use std::time::SystemTime;

use super::{Config, Enforcer, Env, Text, config};
use crate::companion::Companion;
use crate::device_flow::UPDATE_REQUIRED_TEXT;
use crate::host::Host;
use crate::installs::{self, Scanner};
use crate::link::Stop;
use crate::linking;
use crate::notify::Cause;
use crate::state::ConfigRead;
use crate::updater::{Checked, Updater};
use crate::upload::{self, queue::Trigger};

/// The process's update-required part.
pub struct Required {
    enforcer: Enforcer,
    exit: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl Required {
    pub fn new(version: &str) -> Self {
        Self {
            enforcer: Enforcer::new(version),
            exit: OnceLock::new(),
        }
    }

    /// Headless mode: what DST-R58's exit does. Tray mode sets none.
    pub fn exit_with(&self, exit: impl Fn() + Send + Sync + 'static) {
        let _ = self.exit.set(Box::new(exit));
    }
}

/// The updater's hook after every check (DST-R51).
pub fn after_check<H: Host>(host: &H, checked: &Checked) {
    if let Some(required) = host.try_state::<Required>() {
        required
            .enforcer
            .after_check(&AppEnv(host), checked, SystemTime::now());
    }
}

/// The updater's hook at every update tick (DST-R56's required-state reads).
pub fn tick<H: Host>(host: &H, now: SystemTime) {
    if let Some(required) = host.try_state::<Required>() {
        required.enforcer.tick(&AppEnv(host), now);
    }
}

/// The link's hook once update required is entered (DST-R56).
pub fn entered<H: Host>(host: &H) {
    if let Some(required) = host.try_state::<Required>() {
        required.enforcer.entered(&AppEnv(host));
    }
}

struct AppEnv<'a, H>(&'a H);

impl<H: Host> AppEnv<'_, H> {
    fn companion(&self) -> &Companion {
        self.0.state::<Companion>()
    }

    fn updater(&self) -> Option<&Updater> {
        self.0.try_state::<Updater>()
    }
}

impl<H: Host> Env for AppEnv<'_, H> {
    fn read_config(&self) -> Option<Config> {
        let api = &self.companion().api;
        match tauri::async_runtime::block_on(config::read(api)) {
            Ok(config) => Some(config),
            Err(e) => {
                log::warn!("config read failed: {e}");
                None
            }
        }
    }

    fn keep_config(&self, config: &Config) {
        let companion = self.companion();
        let before = companion.state.read(installs::forever_product_uid);
        let uid = config.forever_product_uid.clone();
        // During first run, in memory only (CMP-R100, `Store`).
        let kept = companion.state.update(|s| {
            s.config = Some(ConfigRead {
                forever_product_uid: uid.clone(),
                read_at: upload::window::now(),
            });
        });
        if let Err(e) = kept {
            log::error!("couldn't write state.json: {e}");
        }
        if before != uid {
            log::info!("the Forever product uid is now {uid:?}; scanning");
            if let Some(scanner) = self.0.try_state::<Scanner>() {
                scanner.rescan();
            }
        }
    }

    fn update_required(&self) -> bool {
        self.companion().account().update_required()
    }

    fn enter(&self) {
        self.companion().account().enter_stop(Stop::UpdateRequired {
            message: UPDATE_REQUIRED_TEXT.into(),
        });
    }

    fn leave(&self) {
        if self.companion().account().leave_update_required() {
            upload::window::trigger_all(self.0, Trigger::Start);
            crate::in_game::timer::window::check(self.0);
        }
    }

    fn update_waiting(&self) -> bool {
        self.updater().is_some_and(|u| u.waiting().is_some())
    }

    fn failed_verifications(&self) -> u32 {
        self.updater().map_or(0, Updater::failed_verifications)
    }

    fn show(&self, text: Text) {
        let companion = self.companion();
        companion.account().show_update_required_text(text.text());
        // Held while the first-run window is open (CMP-R100).
        companion.notifications.raise(&Cause::UpdateRequired {
            text: text.text().into(),
        });
    }

    fn check_soon(&self) {
        if let Some(updater) = self.updater() {
            updater.check_soon();
        }
    }

    fn stop_linking(&self) {
        linking::stop_for_update_required(self.0);
    }

    fn exit_for_update(&self) {
        if let Some(exit) = self.0.try_state::<Required>().and_then(|r| r.exit.get()) {
            exit();
        }
    }
}

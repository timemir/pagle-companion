//! Removal (CMP-R70): the one routine that undoes everything the Companion
//! made outside its program files. It runs from `pagle-companion --remove`
//! (CMP-R1), which the Windows uninstaller calls (DST-R14), and on Linux from
//! **Remove Pagle Companion** in the tray menu and the window's Settings
//! (CMP-R71), which also deletes the menu entry and, last, the AppImage
//! (DST-R34).
//!
//! The routine is a [`plan`]: the steps in order, picked by where removal
//! started and the OS. [`execute`] runs every step of it through
//! [`Effects`], even when an earlier one failed (CMP-R70), so the order and
//! that rule are tested over a fake. [`System`] is the real [`Effects`].
//!
//! `--remove` runs without Tauri, like headless mode: the uninstaller starts
//! it with no window, and on Linux it may run with no display. It always
//! exits with status 0 (CMP Contracts, Command line).

use std::fmt;
use std::panic::{self, AssertUnwindSafe};

mod system;
pub mod window;

pub use system::{Commands, OsCommands, System};

/// Where removal started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `--remove`, a process of its own (CMP-R70, DST-R14).
    Command,
    /// **Remove Pagle Companion** in a Linux tray-mode process, which holds
    /// the watcher lock (CMP-R71).
    TrayItem,
}

/// One step of removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The tray-mode process stops watching and the queue (CMP-R71).
    StopWatching,
    /// Starts `pagle-companion --quit` as a child process, which makes a
    /// running tray-mode process quit through the single-instance channel
    /// (CMP-R70, CMP-R2).
    QuitOthers,
    /// Linux: `systemctl --user disable --now pagle-companion.service`, so a
    /// headless process of the Game Mode unit exits (CMP-R70).
    DisableUnit,
    /// Waits for the watcher lock, at most the lock wait, carrying on either
    /// way (CMP-R70).
    WaitForLock,
    /// A scan without network, with the Forever product uid in local state
    /// or the built-in one (CMP-R70).
    Scan,
    /// Step 1: self-revoke, one attempt within the revoke timeout (CMP-R63
    /// step 2).
    SelfRevoke,
    /// Step 2: deletes the credential from every store it may be in.
    DeleteCredential,
    /// Step 3, Linux: deletes the Game Mode unit (CMP-R90).
    DeleteUnit,
    /// Step 3, Linux: `systemctl --user daemon-reload`.
    DaemonReload,
    /// Step 3: deletes the autostart entry (CMP-R3).
    DeleteAutostart,
    /// Step 4: deletes both data addon folders in every install the scan
    /// found or local state names (CMP-R85).
    DeleteDataAddonFolders,
    /// Step 5: deletes the state directory, with local state, the local log
    /// file and the webview's data (CMP-R1).
    DeleteStateDir,
    /// Linux tray item: deletes the menu entry and its icon (DST-R34).
    DeleteMenuEntry,
    /// Linux tray item, last: deletes the AppImage file (DST-R34).
    DeleteAppImage,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self {
            Step::StopWatching => "stop watching and the queue",
            Step::QuitOthers => "make other Companion processes quit",
            Step::DisableUnit => "disable and stop the Game Mode unit",
            Step::WaitForLock => "wait for the watcher lock",
            Step::Scan => "scan for installs",
            Step::SelfRevoke => "self-revoke the credential",
            Step::DeleteCredential => "delete the credential",
            Step::DeleteUnit => "delete the Game Mode unit",
            Step::DaemonReload => "reload the systemd user units",
            Step::DeleteAutostart => "delete the autostart entry",
            Step::DeleteDataAddonFolders => "delete the data addon folders",
            Step::DeleteStateDir => "delete the state directory",
            Step::DeleteMenuEntry => "delete the menu entry",
            Step::DeleteAppImage => "delete the AppImage",
        };
        f.write_str(what)
    }
}

/// The steps of removal, in order (CMP-R70, CMP-R71, DST-R34). `linux`
/// adds the Game Mode unit's steps. The tray item holds the watcher lock,
/// so it makes no other process quit except the Game Mode unit; its lock
/// wait ends at once while it holds the lock.
pub fn plan(origin: Origin, linux: bool) -> Vec<Step> {
    let mut steps = Vec::new();
    match origin {
        Origin::Command => steps.push(Step::QuitOthers),
        Origin::TrayItem => steps.push(Step::StopWatching),
    }
    if linux {
        steps.push(Step::DisableUnit);
    }
    steps.extend([
        Step::WaitForLock,
        Step::Scan,
        Step::SelfRevoke,
        Step::DeleteCredential,
    ]);
    if linux {
        steps.extend([Step::DeleteUnit, Step::DaemonReload]);
    }
    steps.extend([
        Step::DeleteAutostart,
        Step::DeleteDataAddonFolders,
        Step::DeleteStateDir,
    ]);
    if origin == Origin::TrayItem {
        steps.extend([Step::DeleteMenuEntry, Step::DeleteAppImage]);
    }
    steps
}

/// What each step does to the machine.
pub trait Effects {
    /// Runs `step`. An `Err` says why it failed; removal carries on.
    fn run(&mut self, step: Step) -> Result<(), String>;
}

/// Runs every step of `plan` in order, each even if an earlier one failed
/// or panicked (CMP-R70), and returns each step's result. Each step and
/// failure goes to the local log file until step 5 deletes it.
pub fn execute(plan: &[Step], effects: &mut dyn Effects) -> Vec<(Step, Result<(), String>)> {
    plan.iter()
        .map(|&step| {
            log::info!("removal: {step}");
            let result = panic::catch_unwind(AssertUnwindSafe(|| effects.run(step)))
                .unwrap_or_else(|_| Err("it panicked".to_owned()));
            if let Err(e) = &result {
                log::warn!("removal couldn't {step}: {e}");
            }
            (step, result)
        })
        .collect()
}

/// Whether tray mode offers **Remove Pagle Companion**: on Linux only. On
/// Windows the uninstaller is the only way to remove it (CMP-R71).
pub const TRAY_ITEM_OFFERED: bool = cfg!(target_os = "linux");

/// The exit status of `--remove`, whatever its steps did (CMP-R70; CMP
/// Contracts, Command line).
pub const EXIT_STATUS: i32 = 0;

/// Runs `--remove` in this process, without Tauri, and returns its exit
/// status, always 0 (CMP-R70).
///
/// It writes nothing to stdout or stderr, not even for a panic: the
/// uninstaller's `nsExec` counts its removal wait from the last output
/// (DST-R14). Everything goes to the local log file until step 5. Its waits
/// keep it well inside the removal wait: the lock wait (with the `--quit`
/// child in it) and the revoke timeout, 10 s each.
pub fn run() -> i32 {
    panic::set_hook(Box::new(|info| log::error!("removal panicked: {info}")));
    let mut system = System::for_command();
    execute(
        &plan(Origin::Command, cfg!(target_os = "linux")),
        &mut system,
    );
    EXIT_STATUS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the steps run and fails the ones asked to.
    #[derive(Default)]
    struct Fake {
        ran: Vec<Step>,
        fail: Vec<Step>,
        panic: Vec<Step>,
    }

    impl Effects for Fake {
        fn run(&mut self, step: Step) -> Result<(), String> {
            self.ran.push(step);
            if self.panic.contains(&step) {
                panic!("{step}");
            }
            if self.fail.contains(&step) {
                return Err(format!("{step} failed"));
            }
            Ok(())
        }
    }

    #[test]
    fn remove_on_windows_quits_others_waits_scans_then_steps_1_to_5() {
        // CMP-R70: --quit, the lock wait and the scan, then in order:
        // self-revoke, the credential, the autostart entry, the data addon
        // folders, the state directory. No Game Mode unit on Windows
        assert_eq!(
            plan(Origin::Command, false),
            [
                Step::QuitOthers,
                Step::WaitForLock,
                Step::Scan,
                Step::SelfRevoke,
                Step::DeleteCredential,
                Step::DeleteAutostart,
                Step::DeleteDataAddonFolders,
                Step::DeleteStateDir,
            ]
        );
    }

    #[test]
    fn remove_on_linux_also_disables_and_deletes_the_game_mode_unit() {
        // CMP-R70: on Linux also systemctl --user disable --now before the
        // lock wait; step 3 deletes the unit and runs daemon-reload before
        // deleting the autostart entry
        assert_eq!(
            plan(Origin::Command, true),
            [
                Step::QuitOthers,
                Step::DisableUnit,
                Step::WaitForLock,
                Step::Scan,
                Step::SelfRevoke,
                Step::DeleteCredential,
                Step::DeleteUnit,
                Step::DaemonReload,
                Step::DeleteAutostart,
                Step::DeleteDataAddonFolders,
                Step::DeleteStateDir,
            ]
        );
    }

    #[test]
    fn the_linux_tray_item_stops_watching_and_ends_with_the_menu_entry_and_the_appimage() {
        // CMP-R71: the tray-mode process stops watching and the queue, makes
        // no other process quit except the Game Mode unit, runs CMP-R70's
        // steps, then deletes the menu entry and, last, the AppImage
        // (DST-R34)
        let steps = plan(Origin::TrayItem, true);
        assert_eq!(
            steps,
            [
                Step::StopWatching,
                Step::DisableUnit,
                Step::WaitForLock,
                Step::Scan,
                Step::SelfRevoke,
                Step::DeleteCredential,
                Step::DeleteUnit,
                Step::DaemonReload,
                Step::DeleteAutostart,
                Step::DeleteDataAddonFolders,
                Step::DeleteStateDir,
                Step::DeleteMenuEntry,
                Step::DeleteAppImage,
            ]
        );
        assert!(!steps.contains(&Step::QuitOthers));
    }

    #[test]
    fn remove_is_never_handed_over_and_quits_others_first() {
        // CMP-R2, DST-R14: --remove runs as its own process; the first thing
        // it does is make others quit
        assert_eq!(plan(Origin::Command, false)[0], Step::QuitOthers);
        assert_eq!(plan(Origin::Command, true)[0], Step::QuitOthers);
    }

    #[test]
    fn every_step_runs_with_each_step_failing_in_turn() {
        // CMP-R70: each step runs even if an earlier one fails
        for linux in [false, true] {
            for origin in [Origin::Command, Origin::TrayItem] {
                let steps = plan(origin, linux);
                for &failing in &steps {
                    let mut fake = Fake {
                        fail: vec![failing],
                        ..Fake::default()
                    };
                    let results = execute(&steps, &mut fake);
                    assert_eq!(fake.ran, steps, "{failing:?} failing");
                    for (step, result) in results {
                        assert_eq!(result.is_err(), step == failing, "{step:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn every_step_runs_when_all_fail_or_one_panics() {
        // CMP-R70: each step runs even if an earlier one fails, however it
        // fails
        let steps = plan(Origin::Command, true);
        let mut fake = Fake {
            fail: steps.clone(),
            ..Fake::default()
        };
        assert!(execute(&steps, &mut fake).iter().all(|(_, r)| r.is_err()));
        assert_eq!(fake.ran, steps);

        let mut fake = Fake {
            panic: vec![Step::SelfRevoke],
            ..Fake::default()
        };
        let results = execute(&steps, &mut fake);
        assert_eq!(fake.ran, steps);
        assert_eq!(
            results
                .iter()
                .filter(|(_, r)| r.is_err())
                .map(|(s, _)| *s)
                .collect::<Vec<_>>(),
            [Step::SelfRevoke]
        );
    }

    #[test]
    fn remove_exits_with_status_0() {
        // CMP-R70; CMP Contracts, Command line: --remove exits with status 0
        assert_eq!(EXIT_STATUS, 0);
    }
}

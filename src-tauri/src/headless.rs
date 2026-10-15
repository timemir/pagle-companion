//! Headless mode (CMP-R1): `--headless`, on Linux only, runs the same
//! watching, uploading and in-game writes as tray mode, with no tray, no
//! window, no notifications and no questions. It is what the Game Mode unit
//! runs (CMP-R90, CMP Contracts "Game Mode unit").
//!
//! It starts no Tauri runtime. Tauri's Linux runtime starts GTK, which
//! fails without a display, and a `systemd --user` unit in Game Mode or after
//! logout has none. So the parts live in a [`Parts`] host (`host`), there is
//! no webview and so no `webview/` data, and no single-instance channel,
//! which only tray mode registers (CMP-R2).
//!
//! It takes the watcher lock without waiting: if another process holds it,
//! it exits at once with status 3, which the unit doesn't restart (CMP-R2).
//! Otherwise it watches until `SIGTERM`, `SIGHUP` or `SIGINT`, then releases
//! the lock and exits 0.
//!
//! Update checks run here as in tray mode (DST-R51, `updater`, started with
//! the other parts): on Linux the AppImage is replaced as soon as an update
//! verifies (DST-R54). Not here yet: config reads (DST-R63) and DST-R58's
//! exit once the new file is in place while update required; "Read the
//! Companion config and enforce update required" adds them through the
//! updater's `after_check` hook.

use std::error::Error;

use crate::companion::{self, Companion};
use crate::host::{Host, Parts};
use crate::linking;
use crate::notify::Silent;
use crate::state_dir::StateDir;

/// The exit status of `--headless` where headless mode doesn't exist, on
/// Windows (CMP Contracts, Command line).
pub const NOT_ON_THIS_PLATFORM: i32 = 2;

/// The exit status when another process holds the watcher lock. The Game
/// Mode unit doesn't restart it (`RestartPreventExitStatus=3`, CMP-R2).
pub const LOCK_HELD: i32 = 3;

/// How a headless process ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Stopped by a signal, the lock released.
    Stopped,
    /// Another process holds the watcher lock (CMP-R2).
    LockHeld,
    /// The state directory or a part couldn't be set up. The unit restarts
    /// it after its restart delay, as after a crash.
    CouldntStart,
}

impl Outcome {
    pub fn exit_status(self) -> i32 {
        match self {
            Outcome::Stopped => 0,
            Outcome::LockHeld => LOCK_HELD,
            Outcome::CouldntStart => 1,
        }
    }
}

/// A headless start: watching, or the lock was held.
pub enum Start {
    Watching(Watching),
    LockHeld,
}

/// A headless process that holds the watcher lock and watches.
pub struct Watching {
    parts: Parts,
}

impl Watching {
    /// Releases the watcher lock, so the tray-mode process that stopped this
    /// one can take it (CMP-R92). What is still running ends with the
    /// process; local state and the in-game files are only ever replaced
    /// whole, so nothing is left half-written.
    pub fn stop(self) {
        self.parts.state::<Companion>().release_watcher();
    }
}

/// Takes the watcher lock without waiting, then sets up and starts every
/// part, with notifications that show nothing (CMP-R1). A held lock starts
/// nothing and writes nothing.
pub fn start(state_dir: StateDir, version: &str) -> Result<Start, Box<dyn Error>> {
    let Some(lock) = state_dir.try_lock_watcher()? else {
        log::info!("another Pagle Companion holds the watcher lock; exiting");
        return Ok(Start::LockHeld);
    };
    let state = companion::load_state(&state_dir);
    let parts = Parts::default();
    companion::manage(
        &parts,
        state_dir,
        version.to_owned(),
        state,
        Box::new(Silent),
    )?;
    parts.state::<Companion>().hold_watcher(lock);
    log::info!("took the watcher lock");
    linking::start(&parts);
    companion::start_watching(&parts);
    // CMP-R62: a failed delete from a store the credential moved out of.
    parts.state::<Companion>().finish_credential_moves();
    Ok(Start::Watching(Watching { parts }))
}

/// Runs headless mode until a signal stops it, and returns the exit status.
#[cfg(target_os = "linux")]
pub fn run() -> i32 {
    use crate::local_log::Logger;
    use crate::state_dir;

    // First, before any thread starts, so every thread inherits the mask.
    let signals = match signals::Signals::block() {
        Ok(signals) => signals,
        Err(e) => {
            eprintln!("couldn't block signals: {e}");
            return Outcome::CouldntStart.exit_status();
        }
    };
    let state_dir = match state_dir::default_path().and_then(StateDir::open) {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("couldn't open the state directory: {e}");
            return Outcome::CouldntStart.exit_status();
        }
    };
    Logger::install(state_dir.log_file());
    let version = env!("CARGO_PKG_VERSION");
    log::info!("Pagle Companion {version} starting in headless mode");
    // DST-R31: at every start. DST-R32's question isn't asked here.
    crate::appimage::menu::at_start();

    let outcome = match start(state_dir, version) {
        Ok(Start::LockHeld) => Outcome::LockHeld,
        Ok(Start::Watching(watching)) => {
            let signal = signals.wait();
            log::info!("stopping on {}", signals::name(signal));
            watching.stop();
            Outcome::Stopped
        }
        Err(e) => {
            log::error!("couldn't start headless mode: {e}");
            Outcome::CouldntStart
        }
    };
    outcome.exit_status()
}

/// Headless mode is Linux only (CMP-R1).
#[cfg(not(target_os = "linux"))]
pub fn run() -> i32 {
    NOT_ON_THIS_PLATFORM
}

/// The signals that stop headless mode: `SIGTERM` from `systemctl --user
/// stop` (CMP-R92), `SIGHUP`, and `SIGINT` from a terminal.
#[cfg(target_os = "linux")]
mod signals {
    use std::io;

    const STOPPING: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGHUP, libc::SIGINT];

    /// The stopping signals, blocked so that only [`Signals::wait`] takes
    /// them. Children would inherit the block; headless mode starts none.
    pub struct Signals(libc::sigset_t);

    impl Signals {
        /// Blocks the stopping signals in this thread and every thread it
        /// starts later. Call it before any thread starts.
        pub fn block() -> io::Result<Self> {
            // SAFETY: the set is initialised by sigemptyset before use, and
            // pthread_sigmask only reads it.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                for signal in STOPPING {
                    libc::sigaddset(&mut set, signal);
                }
                let failed = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
                if failed != 0 {
                    return Err(io::Error::from_raw_os_error(failed));
                }
                Ok(Self(set))
            }
        }

        /// Waits for one of the stopping signals and returns it.
        pub fn wait(&self) -> libc::c_int {
            loop {
                let mut signal = 0;
                // SAFETY: the set was built by `block`; sigwait writes one
                // int.
                if unsafe { libc::sigwait(&self.0, &mut signal) } == 0 {
                    return signal;
                }
            }
        }
    }

    pub fn name(signal: libc::c_int) -> &'static str {
        match signal {
            libc::SIGTERM => "SIGTERM",
            libc::SIGHUP => "SIGHUP",
            libc::SIGINT => "SIGINT",
            _ => "a signal",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_statuses() {
        // CMP-R2 and CMP Contracts, Game Mode unit: status 3 for a held
        // lock, which isn't restarted; a stop exits 0. CMP Contracts,
        // Command line: status 2 on Windows
        assert_eq!(Outcome::Stopped.exit_status(), 0);
        assert_eq!(Outcome::LockHeld.exit_status(), 3);
        assert_ne!(Outcome::CouldntStart.exit_status(), 0);
        assert_ne!(Outcome::CouldntStart.exit_status(), LOCK_HELD);
        assert_eq!(NOT_ON_THIS_PLATFORM, 2);
    }

    #[test]
    fn a_held_lock_starts_nothing_and_writes_nothing() {
        // CMP-R2: a headless start that finds the lock held exits at once
        // with status 3. (A start that takes the lock runs the real stores
        // and scans, so it is tested from the binary: tests/headless.rs.)
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::open(tmp.path()).unwrap();
        let _tray = dir.try_lock_watcher().unwrap().unwrap();

        let started = std::time::Instant::now();
        let outcome = match start(dir.clone(), "1.0.0").unwrap() {
            Start::LockHeld => Outcome::LockHeld,
            Start::Watching(_) => panic!("watching without the lock"),
        };
        assert_eq!(outcome.exit_status(), LOCK_HELD);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(!dir.state_file().exists());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn headless_mode_is_linux_only() {
        // CMP Contracts, Command line: on Windows --headless exits with
        // status 2
        assert_eq!(run(), NOT_ON_THIS_PLATFORM);
    }
}

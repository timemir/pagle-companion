//! The binary started with `--headless` (CMP-R1, CMP-R2), as the Game Mode
//! unit starts it: on Linux with no display, in a sandbox of its own (its
//! own `XDG_CONFIG_HOME` and `HOME`, no D-Bus session and so no Secret
//! Service, no reachable backend), so it finds no install and no credential.

const BIN: &str = env!("CARGO_BIN_EXE_pagle-companion");

#[cfg(not(target_os = "linux"))]
#[test]
fn headless_mode_exits_with_status_2_off_linux() {
    // CMP Contracts, Command line: on Windows --headless exits with status 2
    let status = std::process::Command::new(BIN)
        .arg("--headless")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2));
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{self, File, TryLockError};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use super::BIN;

    /// Long enough for a slow CI runner; a working start takes well under a
    /// second.
    const PATIENCE: Duration = Duration::from_secs(60);

    struct Sandbox {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        config: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("home");
            let config = tmp.path().join("config");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&config).unwrap();
            Self {
                _tmp: tmp,
                home,
                config,
            }
        }

        fn state_dir(&self) -> PathBuf {
            self.config.join("pagle-companion")
        }

        fn log(&self) -> String {
            fs::read_to_string(self.state_dir().join("companion.log")).unwrap_or_default()
        }

        fn headless(&self) -> Child {
            Command::new(BIN)
                .arg("--headless")
                .env("HOME", &self.home)
                .env("XDG_CONFIG_HOME", &self.config)
                .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
                .env("PAGLE_COMPANION_BASE_URL", "http://127.0.0.1:9")
                .env_remove("DISPLAY")
                .env_remove("WAYLAND_DISPLAY")
                .stdin(Stdio::null())
                .spawn()
                .unwrap()
        }

        /// Waits until the log holds `line`, failing if `child` exits first.
        fn wait_for_log(&self, child: &mut Child, line: &str) {
            let deadline = Instant::now() + PATIENCE;
            while !self.log().contains(line) {
                if let Some(status) = child.try_wait().unwrap() {
                    panic!("exited with {status} before {line:?}:\n{}", self.log());
                }
                assert!(Instant::now() < deadline, "no {line:?}:\n{}", self.log());
                sleep(Duration::from_millis(50));
            }
        }
    }

    fn wait(child: &mut Child) -> ExitStatus {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("still running");
            }
            sleep(Duration::from_millis(50));
        }
    }

    /// Whether another process holds `watcher.lock`. A free lock is let go
    /// at once.
    fn lock_held(state_dir: &Path) -> bool {
        let file = File::open(state_dir.join("watcher.lock")).unwrap();
        match file.try_lock() {
            Ok(()) => false,
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Error(e)) => panic!("{e}"),
        }
    }

    #[test]
    fn headless_mode_runs_without_a_display_and_stops_on_sigterm() {
        // CMP-R1: headless mode runs with no tray, window or webview, here
        // with no display at all; CMP-R2: it holds watcher.lock, and a second
        // headless start exits at once with status 3; CMP Contracts, Game
        // Mode unit: SIGTERM (systemctl stop) releases the lock and exits 0
        let sandbox = Sandbox::new();
        let mut first = sandbox.headless();
        sandbox.wait_for_log(&mut first, "took the watcher lock");
        assert!(lock_held(&sandbox.state_dir()));
        assert!(sandbox.log().contains("starting in headless mode"));

        let mut second = sandbox.headless();
        assert_eq!(wait(&mut second).code(), Some(3));
        assert!(first.try_wait().unwrap().is_none(), "the first one exited");

        let pid = libc::pid_t::try_from(first.id()).unwrap();
        // SAFETY: kill only sends a signal to the child started above.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        assert_eq!(wait(&mut first).code(), Some(0));
        assert!(!lock_held(&sandbox.state_dir()));
        let log = sandbox.log();
        assert!(log.contains("stopping on SIGTERM"), "{log}");
        assert!(log.contains("released the watcher lock"), "{log}");
        assert!(!sandbox.state_dir().join("webview").exists());
    }
}

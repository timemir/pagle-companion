//! `systemctl --user`, which the Game Mode unit is driven with (CMP-R90,
//! CMP-R91, CMP-R92) and which removal disables and reloads it with
//! (CMP-R70). Everything that runs it goes through [`Systemctl`], so tests
//! use a fake and never this machine's real `systemctl`.

use std::ffi::OsString;
use std::io;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// How long a `systemctl` call may take before it is ended and counted as
/// failed. Not a spec tunable: it only keeps a hung `systemctl` from holding
/// up a start, an exit, the toggle or the removal wait (DST Tunables).
pub const COMMAND_WAIT: Duration = Duration::from_secs(10);

/// How often a running `systemctl` is checked on.
const POLL: Duration = Duration::from_millis(50);

/// Runs `systemctl --user <args>`.
pub trait Systemctl: Send + Sync {
    /// `Ok` when it ran and succeeded. No `systemctl` at all is a failure.
    fn run(&self, args: &[&str]) -> Result<(), String>;
}

/// The real `systemctl`, found on `PATH`.
pub struct OsSystemctl {
    program: OsString,
}

impl OsSystemctl {
    pub fn new() -> Self {
        Self::with_program("systemctl")
    }

    /// Another program in `systemctl`'s place: tests use a fake one.
    pub fn with_program(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// Runs `systemctl --user <args>`, ending it after [`COMMAND_WAIT`].
    /// `Ok(false)` when there is no `systemctl` at all.
    pub fn call(&self, args: &[&str]) -> Result<bool, String> {
        let spawned = Command::new(&self.program)
            .arg("--user")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(format!("couldn't run systemctl: {e}")),
        };
        match wait_until(&mut child, Instant::now() + COMMAND_WAIT) {
            Some(status) if status.success() => Ok(true),
            Some(status) => Err(format!(
                "systemctl --user {} exited with {status}",
                args.join(" ")
            )),
            None => Err(format!("systemctl --user {} took too long", args.join(" "))),
        }
    }
}

impl Default for OsSystemctl {
    fn default() -> Self {
        Self::new()
    }
}

impl Systemctl for OsSystemctl {
    fn run(&self, args: &[&str]) -> Result<(), String> {
        match self.call(args)? {
            true => Ok(()),
            false => Err("there is no systemctl".into()),
        }
    }
}

/// Waits for `child` until `deadline`, then kills it. `None` if it had to be
/// killed.
pub fn wait_until(child: &mut Child, deadline: Instant) -> Option<ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => sleep(POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A `systemctl` that records its calls and fails the ones asked to.

    use std::sync::{Arc, Mutex};

    use super::Systemctl;

    #[derive(Clone, Default)]
    pub struct FakeSystemctl {
        calls: Arc<Mutex<Vec<String>>>,
        fail: Arc<Mutex<Vec<String>>>,
    }

    impl FakeSystemctl {
        /// Every call as `systemctl --user …`, in order.
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        /// Makes every later call whose arguments start with `args` fail.
        pub fn fail(&self, args: &str) {
            self.fail.lock().unwrap().push(args.to_owned());
        }
    }

    impl Systemctl for FakeSystemctl {
        fn run(&self, args: &[&str]) -> Result<(), String> {
            let args = args.join(" ");
            self.calls
                .lock()
                .unwrap()
                .push(format!("systemctl --user {args}"));
            if self
                .fail
                .lock()
                .unwrap()
                .iter()
                .any(|f| args.starts_with(f))
            {
                return Err(format!("systemctl --user {args} failed"));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_systemctl_is_reported_as_missing() {
        // CMP-R70: a missing systemctl is no failure for removal; CMP-R90:
        // the toggle can't work without one
        let missing = OsSystemctl::with_program("pagle-companion-no-such-systemctl");
        assert_eq!(missing.call(&["daemon-reload"]), Ok(false));
        assert!(missing.run(&["daemon-reload"]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_real_runner_passes_user_and_the_arguments_and_reads_the_status() {
        // CMP-R90: systemctl --user daemon-reload, then systemctl --user
        // enable pagle-companion.service; a failing systemctl is a failure.
        // A fake script stands in for systemctl
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls");
        let script = tmp.path().join("systemctl");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\n[ \"$2\" != fail ]\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let systemctl = OsSystemctl::with_program(&script);

        assert_eq!(
            systemctl.run(&["enable", "pagle-companion.service"]),
            Ok(())
        );
        assert!(systemctl.run(&["fail"]).is_err());
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "--user enable pagle-companion.service\n--user fail\n"
        );
    }
}

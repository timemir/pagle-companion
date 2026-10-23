//! The binary started with `--remove` (CMP-R1, CMP-R70), on Linux only, in a
//! sandbox of its own: its own `XDG_CONFIG_HOME` and `HOME`, no display, no
//! D-Bus session and so no Secret Service, a fake `systemctl` first on
//! `PATH`, a stub backend, and an install under a temp folder. So it removes
//! nothing of the machine running the tests.
//!
//! Not on Windows: there the state directory (`dirs::config_dir`, a known
//! folder rather than `%APPDATA%`), the `Run` value and the Credential
//! Manager entry can't be pointed elsewhere, so `--remove` is tested through
//! its seams (`removal`) instead.

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const BIN: &str = env!("CARGO_BIN_EXE_pagle-companion");
    const CREDENTIAL: &str = "pgc_sandboxed_removal_credential";

    /// Long enough for a slow CI runner; removal waits at most the lock wait
    /// for its `--quit` child.
    const PATIENCE: Duration = Duration::from_secs(60);

    struct Sandbox {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        config: PathBuf,
        bin: PathBuf,
        flavor: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("home");
            let config = tmp.path().join("config");
            let bin = tmp.path().join("bin");
            let flavor = tmp.path().join("WoW/_classic_beta_");
            for dir in [&home, &config, &bin] {
                fs::create_dir_all(dir).unwrap();
            }
            // A systemctl that only notes how it was called.
            let systemctl = bin.join("systemctl");
            fs::write(
                &systemctl,
                format!(
                    "#!/bin/sh\necho \"$*\" >> '{}'\n",
                    bin.join("systemctl.log").display()
                ),
            )
            .unwrap();
            fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
            Self {
                _tmp: tmp,
                home,
                config,
                bin,
                flavor,
            }
        }

        fn state_dir(&self) -> PathBuf {
            self.config.join("pagle-companion")
        }

        fn unit(&self) -> PathBuf {
            self.config.join("systemd/user/pagle-companion.service")
        }

        fn autostart(&self) -> PathBuf {
            self.config.join("autostart/app.pagle.companion.desktop")
        }

        fn folder(&self, name: &str) -> PathBuf {
            self.flavor.join("Interface/AddOns").join(name)
        }

        /// A linked Companion with its credential in the credential file, a
        /// Game Mode unit, an autostart entry, and both data addon folders in
        /// an install local state names.
        fn set_up(&self) {
            let state_dir = self.state_dir();
            fs::create_dir_all(state_dir.join("webview")).unwrap();
            fs::write(state_dir.join("webview/data"), "x").unwrap();
            fs::write(state_dir.join("companion.log"), "").unwrap();
            fs::write(state_dir.join("credential"), format!("{CREDENTIAL}\n")).unwrap();
            let state = serde_json::json!({
                "v": 1,
                "manualInstalls": [self.flavor.to_str().unwrap()],
                "battleTag": "Ada#1234",
                "credentialStore": "file",
            });
            fs::write(state_dir.join("state.json"), state.to_string()).unwrap();
            for path in [self.unit(), self.autostart()] {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, "x\n").unwrap();
            }
            fs::create_dir_all(self.flavor.join("Interface/AddOns/Pagle")).unwrap();
            for name in ["Pagle_Following", "Pagle_Anglers", "Pagle_Content"] {
                fs::create_dir_all(self.folder(name)).unwrap();
                fs::write(self.folder(name).join("x.lua"), "x").unwrap();
            }
        }

        fn remove(&self, base_url: &str, args: &[&str]) -> std::process::ExitStatus {
            let path = format!(
                "{}:{}",
                self.bin.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let mut child = Command::new(BIN)
                .args(args)
                .env("HOME", &self.home)
                .env("XDG_CONFIG_HOME", &self.config)
                .env("PATH", path)
                .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
                .env("PAGLE_COMPANION_BASE_URL", base_url)
                .env_remove("APPIMAGE")
                .env_remove("DISPLAY")
                .env_remove("WAYLAND_DISPLAY")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + PATIENCE;
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    // DST-R14: the uninstaller's nsExec counts its removal
                    // wait from the last output, so --remove writes none.
                    let mut output = String::new();
                    child
                        .stdout
                        .take()
                        .unwrap()
                        .read_to_string(&mut output)
                        .unwrap();
                    child
                        .stderr
                        .take()
                        .unwrap()
                        .read_to_string(&mut output)
                        .unwrap();
                    assert_eq!(output, "", "--remove wrote to stdout or stderr");
                    return status;
                }
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("--remove still running");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        fn systemctl_calls(&self) -> String {
            fs::read_to_string(self.bin.join("systemctl.log")).unwrap_or_default()
        }
    }

    fn exists(path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }

    #[test]
    fn remove_with_other_arguments_removes_everything_and_exits_0() {
        // CMP-R1, CMP Contracts "Command line": --remove wins over every
        // other argument and exits 0; CMP-R70: systemctl --user disable
        // --now, one self-revoke with the stored credential, the credential
        // file, the unit and daemon-reload, the autostart entry, both data
        // addon folders, the state directory with webview/
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = rt.block_on(MockServer::start());
        rt.block_on(
            Mock::given(method("DELETE"))
                .and(path("/api/v1/companion/credential"))
                .respond_with(ResponseTemplate::new(204))
                .mount(&server),
        );
        let sandbox = Sandbox::new();
        sandbox.set_up();

        let status = sandbox.remove(&server.uri(), &["--autostart", "--remove", "--quit"]);
        assert_eq!(status.code(), Some(0));

        let revokes: Vec<_> = rt
            .block_on(server.received_requests())
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.url.path() == "/api/v1/companion/credential")
            .collect();
        assert_eq!(revokes.len(), 1);
        assert_eq!(
            revokes[0].headers.get("authorization").unwrap(),
            &format!("Bearer {CREDENTIAL}")
        );
        assert!(!exists(&sandbox.state_dir()));
        assert!(!exists(&sandbox.unit()));
        assert!(!exists(&sandbox.autostart()));
        assert!(!exists(&sandbox.folder("Pagle_Following")));
        assert!(!exists(&sandbox.folder("Pagle_Anglers")));
        assert!(!exists(&sandbox.folder("Pagle_Content")));
        assert!(exists(&sandbox.flavor.join("Interface/AddOns/Pagle")));
        let calls = sandbox.systemctl_calls();
        assert!(
            calls.contains("--user disable --now pagle-companion.service\n"),
            "{calls}"
        );
        assert!(calls.contains("--user daemon-reload\n"), "{calls}");
        let disable = calls.find("disable").unwrap();
        assert!(calls.find("daemon-reload").unwrap() > disable, "{calls}");
    }

    #[test]
    fn remove_on_a_machine_without_the_companion_creates_nothing_and_exits_0() {
        // CMP-R70: every step runs even if it has nothing to do or fails
        // (here the backend can't be reached), and the exit status is 0
        // (DST-R14: well inside the 30 s removal wait.)
        let sandbox = Sandbox::new();
        let started = Instant::now();
        let status = sandbox.remove("http://127.0.0.1:9", &["--remove"]);
        assert_eq!(status.code(), Some(0));
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(!exists(&sandbox.state_dir()));
    }
}

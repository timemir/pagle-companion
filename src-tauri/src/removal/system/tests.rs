//! Removal's real steps over temp folders, in-memory stores, a stub server
//! and fake commands. Nothing here touches this machine's real state
//! directory, credential, autostart entry, unit or installs.

use std::sync::{Arc, Mutex};

use wiremock::ResponseTemplate;

use super::*;
use crate::credential_store::testing::MemoryStore;
use crate::credential_store::{CredentialStore, FileStore};
use crate::in_game::folders::fixture::install;
use crate::removal::{Origin, execute, plan};
use crate::state::CredentialStore as StoreKind;
use crate::stub_server;

const CREDENTIAL: &str = "pgc_removal_test_credential";
const TOC: &[u8] = b"## Interface: 11508\n";

/// The commands removal ran, as text.
#[derive(Clone, Default)]
struct FakeCommands {
    ran: Arc<Mutex<Vec<String>>>,
    /// `None`: no `systemctl`; `Some(false)`: it fails.
    systemctl: Option<bool>,
}

impl FakeCommands {
    fn working() -> Self {
        Self {
            systemctl: Some(true),
            ..Self::default()
        }
    }

    fn ran(&self) -> Vec<String> {
        self.ran.lock().unwrap().clone()
    }
}

impl Commands for FakeCommands {
    fn start_quit(&mut self) -> Result<(), String> {
        self.ran.lock().unwrap().push("--quit".into());
        Ok(())
    }

    fn finish_quit(&mut self, _deadline: Instant) {
        self.ran.lock().unwrap().push("finish --quit".into());
    }

    fn systemctl(&mut self, args: &[&str]) -> Result<bool, String> {
        self.ran
            .lock()
            .unwrap()
            .push(format!("systemctl --user {}", args.join(" ")));
        match self.systemctl {
            None => Ok(false),
            Some(true) => Ok(true),
            Some(false) => Err("systemctl failed".into()),
        }
    }
}

/// A runtime for the stub server, outside which removal blocks on its own.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

/// A machine that is all temp folders: the state directory with every file
/// CMP Data names, the credential file, a Game Mode unit, an autostart
/// entry, a menu entry and icon, an AppImage, and one install with both data
/// addon folders, named by local state.
struct Machine {
    _tmp: tempfile::TempDir,
    state_dir: PathBuf,
    config: PathBuf,
    home: PathBuf,
    appimage: PathBuf,
    autostart: PathBuf,
    flavor: String,
    secret_service: MemoryStore,
}

impl Machine {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        let home = tmp.path().join("home");
        let state_dir = config.join("pagle-companion");
        fs::create_dir_all(state_dir.join("webview/EBWebView")).unwrap();
        for file in [
            "state.json",
            "watcher.lock",
            "companion.log",
            "webview/EBWebView/x",
        ] {
            fs::write(state_dir.join(file), "x").unwrap();
        }
        FileStore::in_state_dir(&StateDir::at(&state_dir))
            .write(CREDENTIAL)
            .unwrap();
        let unit = xdg::unit_path(&config);
        fs::create_dir_all(unit.parent().unwrap()).unwrap();
        fs::write(&unit, "[Unit]\n").unwrap();
        let autostart = config.join("autostart/app.pagle.companion.desktop");
        fs::create_dir_all(autostart.parent().unwrap()).unwrap();
        fs::write(&autostart, "[Desktop Entry]\n").unwrap();
        for path in [
            xdg::menu_entry_path(&home.join(".local/share")),
            xdg::menu_icon_path(&home.join(".local/share")),
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "x").unwrap();
        }
        let appimage = home.join("Applications/pagle-companion.AppImage");
        fs::create_dir_all(appimage.parent().unwrap()).unwrap();
        fs::write(&appimage, "ELF").unwrap();
        let flavor = install(tmp.path(), "WoW/_classic_beta_", Some(TOC));
        for folder in ["Pagle_Following", "Pagle_Anglers"] {
            let dir = Path::new(&flavor).join("Interface/AddOns").join(folder);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("x.lua"), "x").unwrap();
        }
        Self {
            _tmp: tmp,
            state_dir,
            config,
            home,
            appimage,
            autostart,
            flavor,
            secret_service: MemoryStore::new("your keyring"),
        }
    }

    /// The machine's [`System`], linked with the credential in the file.
    fn system(&self, commands: FakeCommands) -> System {
        let state = State {
            manual_installs: vec![self.flavor.clone()],
            battle_tag: Some("Ada#1234".into()),
            credential_store: Some(StoreKind::File),
            ..State::default()
        };
        let dir = StateDir::at(&self.state_dir);
        let keychain = Keychain::new(
            Box::new(self.secret_service.clone()),
            Some(Box::new(FileStore::in_state_dir(&dir))),
        );
        let mut system = System::new(state, keychain, Box::new(commands));
        system.state_dir = Some(dir);
        system.config_home = Some(self.config.clone());
        system.data_home = Some(self.home.join(".local/share"));
        system.appimage = Some(self.appimage.clone());
        let autostart = self.autostart.clone();
        system.delete_autostart = Box::new(move || match fs::remove_file(&autostart) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        });
        system
    }

    fn data_addon_folders(&self) -> Vec<PathBuf> {
        ["Pagle_Following", "Pagle_Anglers"]
            .iter()
            .map(|f| Path::new(&self.flavor).join("Interface/AddOns").join(f))
            .collect()
    }
}

#[test]
fn the_linux_tray_item_deletes_everything_and_last_the_appimage() {
    // CMP-R70 steps 1 to 5, CMP-R71, DST-R34: self-revoke once; the
    // credential from every store; the unit, daemon-reload and the
    // autostart entry; both data addon folders; the state directory with
    // local state, the log and webview/; then the menu entry, its icon and
    // the AppImage
    let machine = Machine::new();
    machine.secret_service.write(CREDENTIAL).unwrap();
    let rt = runtime();
    let server = rt.block_on(stub_server::start());
    rt.block_on(stub_server::script(
        &server,
        "DELETE",
        "companion/credential",
        vec![ResponseTemplate::new(204)],
    ));
    let commands = FakeCommands::working();
    let mut system = machine.system(commands.clone());
    system.api = Some(stub_server::api(&server));
    let stopped = Arc::new(Mutex::new(false));
    let flag = stopped.clone();
    system.stop_watching = Some(Box::new(move || *flag.lock().unwrap() = true));

    let results = execute(&plan(Origin::TrayItem, true), &mut system);
    for (step, result) in &results {
        assert!(result.is_ok(), "{step:?}: {result:?}");
    }

    assert!(*stopped.lock().unwrap());
    let revokes = rt.block_on(stub_server::requests(&server, "companion/credential"));
    assert_eq!(revokes.len(), 1);
    assert_eq!(
        revokes[0].headers.get("authorization").unwrap(),
        &format!("Bearer {CREDENTIAL}")
    );
    assert_eq!(machine.secret_service.credential(), None);
    assert!(!machine.state_dir.exists());
    assert!(!xdg::unit_path(&machine.config).exists());
    assert!(!machine.autostart.exists());
    for folder in machine.data_addon_folders() {
        assert!(!folder.exists(), "{}", folder.display());
    }
    assert!(
        Path::new(&machine.flavor)
            .join("Interface/AddOns/Pagle/Pagle.toc")
            .is_file()
    );
    assert!(!xdg::menu_entry_path(&machine.home.join(".local/share")).exists());
    assert!(!xdg::menu_icon_path(&machine.home.join(".local/share")).exists());
    assert!(!machine.appimage.exists());
    assert_eq!(
        commands.ran(),
        [
            "systemctl --user disable --now pagle-companion.service",
            "finish --quit",
            "systemctl --user daemon-reload",
        ]
    );
}

#[test]
fn remove_quits_others_takes_the_lock_and_deletes_the_state_directory_holding_it() {
    // CMP-R70: --remove starts --quit, waits for the lock, carries on; its
    // own hold on watcher.lock never keeps step 5 from deleting the folder
    let machine = Machine::new();
    let commands = FakeCommands::working();
    let mut system = machine.system(commands.clone());

    let results = execute(&plan(Origin::Command, false), &mut system);
    let failed: Vec<_> = results.iter().filter(|(_, r)| r.is_err()).collect();
    // No backend: only the self-revoke fails, and removal carries on.
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].0, Step::SelfRevoke);
    assert_eq!(commands.ran(), ["--quit", "finish --quit"]);
    assert!(!machine.state_dir.exists());
    assert!(!machine.autostart.exists());
    for folder in machine.data_addon_folders() {
        assert!(!folder.exists());
    }
    // --remove leaves the unit to Linux and the menu entry and AppImage to
    // the tray item.
    assert!(xdg::menu_entry_path(&machine.home.join(".local/share")).exists());
    assert!(machine.appimage.exists());
}

#[test]
fn the_lock_wait_gives_up_after_the_lock_wait_and_carries_on() {
    // CMP-R70: waits for the lock, at most the lock wait, carrying on either
    // way; the --quit child is waited for within the same wait
    let machine = Machine::new();
    let commands = FakeCommands::working();
    let mut system = machine.system(commands.clone());
    system.lock_wait = Duration::from_millis(300);
    let other = StateDir::at(&machine.state_dir)
        .try_lock_watcher()
        .unwrap()
        .unwrap();

    let started = Instant::now();
    let held = system.run(Step::WaitForLock);
    assert!(held.is_err());
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(system.lock.is_none());
    assert_eq!(commands.ran(), ["finish --quit"]);

    drop(other);
    assert_eq!(system.run(Step::WaitForLock), Ok(()));
    assert!(system.lock.is_some());
}

#[test]
fn self_revoke_is_one_attempt_given_up_on_after_the_revoke_timeout() {
    // CMP-R70 step 1, CMP-R63 step 2: one DELETE /api/v1/companion/credential
    // within the revoke timeout; a server that doesn't answer is given up on
    let machine = Machine::new();
    let rt = runtime();
    let server = rt.block_on(stub_server::start());
    rt.block_on(stub_server::script(
        &server,
        "DELETE",
        "companion/credential",
        vec![ResponseTemplate::new(204).set_delay(Duration::from_secs(30))],
    ));
    let mut system = machine.system(FakeCommands::working());
    system.api = Some(stub_server::api(&server));
    system.revoke_timeout = Duration::from_millis(300);

    let started = Instant::now();
    assert!(system.run(Step::SelfRevoke).is_err());
    assert!(started.elapsed() < Duration::from_secs(10));
    let revokes = rt.block_on(stub_server::requests(&server, "companion/credential"));
    assert_eq!(revokes.len(), 1);
    assert_eq!(REVOKE_TIMEOUT, Duration::from_secs(10));
}

#[test]
fn a_refused_self_revoke_is_not_tried_again() {
    // CMP-R70 step 1: one attempt; a failure is ignored (ACC-R54)
    let machine = Machine::new();
    let rt = runtime();
    let server = rt.block_on(stub_server::start());
    rt.block_on(stub_server::script(
        &server,
        "DELETE",
        "companion/credential",
        vec![ResponseTemplate::new(500), ResponseTemplate::new(204)],
    ));
    let mut system = machine.system(FakeCommands::working());
    system.api = Some(stub_server::api(&server));

    assert!(system.run(Step::SelfRevoke).is_err());
    let revokes = rt.block_on(stub_server::requests(&server, "companion/credential"));
    assert_eq!(revokes.len(), 1);
}

#[test]
fn no_credential_is_nothing_to_revoke() {
    // CMP-R70 step 1: an unlinked Companion sends nothing
    let machine = Machine::new();
    let mut system = machine.system(FakeCommands::working());
    system.run(Step::DeleteCredential).unwrap();
    assert_eq!(system.run(Step::SelfRevoke), Ok(()));
}

#[test]
fn a_credential_in_both_linux_stores_is_deleted_from_both() {
    // CMP-R70 step 2: the Secret Service and the credential file
    let secret_service = MemoryStore::new("your keyring");
    let file = MemoryStore::new("its settings folder");
    let keychain = Keychain::new(
        Box::new(secret_service.clone()),
        Some(Box::new(file.clone())),
    );
    for kind in [None, Some(StoreKind::SecretService), Some(StoreKind::File)] {
        secret_service.write(CREDENTIAL).unwrap();
        file.write(CREDENTIAL).unwrap();
        let state = State {
            credential_store: kind,
            ..State::default()
        };
        let mut system = System::new(
            state,
            Keychain::new(
                Box::new(secret_service.clone()),
                Some(Box::new(file.clone())),
            ),
            Box::new(FakeCommands::working()),
        );
        assert_eq!(system.run(Step::DeleteCredential), Ok(()));
        assert_eq!(secret_service.credential(), None, "{kind:?}");
        assert_eq!(file.credential(), None, "{kind:?}");
    }
    assert_eq!(keychain.every_store(None).len(), 2);
}

#[test]
fn a_store_that_fails_to_delete_leaves_the_others_deleted() {
    // CMP-R70 step 2: every store it may be in; a failure is reported
    struct Failing;
    impl CredentialStore for Failing {
        fn place(&self) -> &'static str {
            "your keyring"
        }
        fn read(&self) -> Result<Option<String>, crate::credential_store::StoreError> {
            Ok(None)
        }
        fn write(&self, _: &str) -> Result<(), crate::credential_store::StoreError> {
            Ok(())
        }
        fn delete(&self) -> Result<(), crate::credential_store::StoreError> {
            Err(crate::credential_store::StoreError::Unavailable(
                "locked".into(),
            ))
        }
    }
    let file = MemoryStore::new("its settings folder");
    file.write(CREDENTIAL).unwrap();
    let mut system = System::new(
        State::default(),
        Keychain::new(Box::new(Failing), Some(Box::new(file.clone()))),
        Box::new(FakeCommands::working()),
    );
    let error = system.run(Step::DeleteCredential).unwrap_err();
    assert!(error.contains("your keyring"), "{error}");
    assert_eq!(file.credential(), None);
}

#[test]
fn a_missing_systemctl_or_unit_is_no_failure() {
    // CMP-R70 step 3: the unit and daemon-reload, on a machine where Game
    // Mode was never set up (CMP-R90) or that has no systemd
    let machine = Machine::new();
    fs::remove_file(xdg::unit_path(&machine.config)).unwrap();
    let mut system = machine.system(FakeCommands::default());
    assert_eq!(system.run(Step::DisableUnit), Ok(()));
    assert_eq!(system.run(Step::DeleteUnit), Ok(()));
    assert_eq!(system.run(Step::DaemonReload), Ok(()));

    let failing = FakeCommands {
        systemctl: Some(false),
        ..FakeCommands::default()
    };
    let mut system = machine.system(failing);
    assert_eq!(system.run(Step::DisableUnit), Ok(()));
    assert_eq!(system.run(Step::DaemonReload), Ok(()));
}

#[test]
fn a_failing_systemctl_with_a_unit_is_a_failure() {
    // CMP-R70 step 3: a unit that couldn't be disabled is reported, and
    // removal carries on (the unit file is still deleted)
    let machine = Machine::new();
    let failing = FakeCommands {
        systemctl: Some(false),
        ..FakeCommands::default()
    };
    let mut system = machine.system(failing);
    assert!(system.run(Step::DisableUnit).is_err());
    assert_eq!(system.run(Step::DeleteUnit), Ok(()));
    assert!(!xdg::unit_path(&machine.config).exists());
    assert!(system.run(Step::DaemonReload).is_err());
}

#[test]
fn a_missing_state_directory_stays_missing() {
    // CMP-R70: removal creates nothing; with no state directory there is no
    // lock to wait for and nothing to delete
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("pagle-companion");
    let mut system = System::new(
        State::default(),
        Keychain::new(Box::new(MemoryStore::new("x")), None),
        Box::new(FakeCommands::working()),
    );
    system.state_dir = Some(StateDir::at(&missing));
    system.lock_wait = Duration::from_millis(100);
    assert_eq!(system.run(Step::WaitForLock), Ok(()));
    assert_eq!(system.run(Step::DeleteStateDir), Ok(()));
    assert!(!missing.exists());

    system.state_dir = None;
    assert!(system.run(Step::DeleteStateDir).is_err());
}

#[test]
fn without_an_appimage_there_is_nothing_to_delete() {
    // DST-R34: the file $APPIMAGE names; a development run has none
    let machine = Machine::new();
    let mut system = machine.system(FakeCommands::working());
    system.appimage = None;
    assert_eq!(system.run(Step::DeleteAppImage), Ok(()));
    assert!(machine.appimage.exists());
}

#[test]
fn the_scan_adds_found_installs_to_those_local_state_names() {
    // CMP-R70: a scan without network, with the Forever product uid in
    // local state or the built-in one; step 4 covers every install it found
    // or local state names
    let machine = Machine::new();
    let mut system = machine.system(FakeCommands::working());
    assert_eq!(system.flavors(), std::slice::from_ref(&machine.flavor));
    // A manual install that no longer holds WoW Forever is still named.
    system
        .state
        .manual_installs
        .push("Z:/gone/_classic_beta_".into());
    system.run(Step::Scan).unwrap();
    assert!(
        system
            .flavors()
            .contains(&"Z:/gone/_classic_beta_".to_owned())
    );
    assert!(system.flavors().contains(&machine.flavor));
}

#[cfg(windows)]
#[test]
fn the_windows_credential_is_deleted_from_credential_manager() {
    // CMP-R70 step 2: Credential Manager. A test entry of its own, never the
    // real pagle-companion:credential
    use crate::credential_store::windows::{CredentialManager, TARGET};
    let target = format!("pagle-companion-test:removal-{}", std::process::id());
    assert_ne!(target, TARGET);
    let entry = CredentialManager::with_target(&target);
    entry.write(CREDENTIAL).unwrap();

    let mut system = System::new(
        State::default(),
        Keychain::new(Box::new(CredentialManager::with_target(&target)), None),
        Box::new(FakeCommands::working()),
    );
    let deleted = system.run(Step::DeleteCredential);
    let left = entry.read();
    let _ = entry.delete();
    assert_eq!(deleted, Ok(()));
    assert_eq!(left, Ok(None));
}

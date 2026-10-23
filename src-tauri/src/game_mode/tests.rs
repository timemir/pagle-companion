//! The toggle's decisions over temp folders, in-memory credential stores and
//! a fake `systemctl`. Nothing here runs this machine's real `systemctl` or
//! touches its real unit or credential.

use std::fs;
use std::path::PathBuf;

use super::*;
use crate::credential_store::testing::MemoryStore;
use crate::credential_store::{CredentialStore, StoreError};
use crate::state::State;
use crate::systemctl::testing::FakeSystemctl;

const CREDENTIAL: &str = "pgc_game_mode_test_credential";
const APPIMAGE: &str = "/home/deck/Applications/Pagle Companion.AppImage";
const MOVED: &str = "/home/deck/Apps/Pagle Companion.AppImage";

const RELOAD: &str = "systemctl --user daemon-reload";
const ENABLE: &str = "systemctl --user enable pagle-companion.service";
const DISABLE: &str = "systemctl --user disable --now pagle-companion.service";

const STEAMOS: &str =
    "NAME=\"SteamOS\"\nPRETTY_NAME=\"SteamOS\"\nVERSION_CODENAME=holo\nID=steamos\nID_LIKE=arch\n";

/// A Steam Deck in a temp folder: the unit's folder, the state directory,
/// both credential stores and a fake `systemctl`.
struct Deck {
    tmp: tempfile::TempDir,
    secret_service: MemoryStore,
    file: MemoryStore,
    keychain: Keychain,
    systemctl: FakeSystemctl,
    store: Store,
}

impl Deck {
    fn new(state: State) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let secret_service = MemoryStore::new("your keyring");
        let file = MemoryStore::new("its settings folder");
        let keychain = Keychain::new(
            Box::new(secret_service.clone()),
            Some(Box::new(file.clone())),
        );
        let store = Store::new(tmp.path().join("state.json"), state);
        Self {
            tmp,
            secret_service,
            file,
            keychain,
            systemctl: FakeSystemctl::default(),
            store,
        }
    }

    /// Linked, with the credential in the Secret Service.
    fn linked() -> Self {
        let deck = Self::new(State {
            first_run_done: true,
            battle_tag: Some("Ada#1234".into()),
            credential_store: Some(StoreKind::SecretService),
            ..State::default()
        });
        deck.secret_service.write(CREDENTIAL).unwrap();
        deck
    }

    /// Linked, with the toggle on: the credential in the credential file
    /// and the unit written.
    fn on() -> Self {
        let deck = Self::new(State {
            first_run_done: true,
            battle_tag: Some("Ada#1234".into()),
            credential_store: Some(StoreKind::File),
            ..State::default()
        });
        deck.store.update(|s| s.settings.game_mode = true).unwrap();
        deck.file.write(CREDENTIAL).unwrap();
        fs::create_dir_all(deck.unit().parent().unwrap()).unwrap();
        fs::write(deck.unit(), unit_text(APPIMAGE, None)).unwrap();
        deck
    }

    fn unit(&self) -> PathBuf {
        xdg::unit_path(&self.tmp.path().join("config"))
    }

    fn machine(&self, appimage: &str) -> Machine {
        Machine {
            unit: Some(self.unit()),
            appimage: Some(appimage.into()),
            xdg_config_home: None,
            systemctl: Box::new(self.systemctl.clone()),
        }
    }

    fn turn_on(&self) -> Result<(), String> {
        self.machine(APPIMAGE).turn_on(&self.store, &self.keychain)
    }

    fn turn_off(&self) -> Result<(), String> {
        self.machine(APPIMAGE).turn_off(&self.store, &self.keychain)
    }

    fn game_mode(&self) -> bool {
        self.store.read(|s| s.settings.game_mode)
    }

    fn credential_store(&self) -> Option<StoreKind> {
        self.store.read(|s| s.credential_store)
    }

    fn saved(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.tmp.path().join("state.json")).unwrap()).unwrap()
    }
}

fn locked() -> StoreError {
    StoreError::Unavailable("locked".into())
}

#[test]
fn the_toggle_is_offered_only_on_steamos_as_an_appimage() {
    // CMP-R90: shown only on SteamOS (ID=steamos in /etc/os-release) when
    // running as an AppImage ($APPIMAGE is set)
    assert!(offered(Some(STEAMOS), Some(APPIMAGE)));
    assert!(!offered(Some(STEAMOS), None));
    assert!(!offered(Some(STEAMOS), Some("")));
    assert!(!offered(None, Some(APPIMAGE)));
    let ubuntu = "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n";
    assert!(!offered(Some(ubuntu), Some(APPIMAGE)));
}

#[test]
fn steamos_is_its_id_quoted_or_not_and_never_id_like() {
    // CMP-R90: ID=steamos in /etc/os-release
    assert!(is_steamos("ID=steamos\n"));
    assert!(is_steamos("NAME=x\nID=\"steamos\"\n"));
    assert!(is_steamos("ID='steamos'\r\n"));
    assert!(!is_steamos("ID=arch\nID_LIKE=steamos\n"));
    assert!(!is_steamos("VARIANT_ID=steamos\n"));
    assert!(!is_steamos("ID=steamos-fork\n"));
}

#[test]
fn the_unit_is_the_contract_text() {
    // CMP Contracts, Game Mode unit: the exact text, ExecStart is $APPIMAGE
    // quoted as DST-R31 quotes Exec, then --headless; Restart=always,
    // RestartPreventExitStatus=3, RestartSec the unit restart delay (30 s,
    // CMP Tunables); no Environment= line when XDG_CONFIG_HOME is unset
    assert_eq!(
        unit_text(APPIMAGE, None),
        "[Unit]\n\
         Description=Pagle Companion (uploads Catch logs in the background)\n\
         \n\
         [Service]\n\
         ExecStart=\"/home/deck/Applications/Pagle Companion.AppImage\" --headless\n\
         Restart=always\n\
         RestartPreventExitStatus=3\n\
         RestartSec=30\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    );
    assert_eq!(RESTART_DELAY, Duration::from_secs(30));
}

#[test]
fn the_unit_passes_xdg_config_home_on_when_it_is_set() {
    // CMP Contracts, Game Mode unit: the Environment= line is written only
    // when XDG_CONFIG_HOME is set, so the headless process uses the same
    // state directory and lock
    let unit = unit_text(APPIMAGE, Some("/home/deck/cfg"));
    assert!(
        unit.contains(
            "[Service]\nEnvironment=XDG_CONFIG_HOME=/home/deck/cfg\nExecStart=\"/home/deck/"
        ),
        "{unit}"
    );
    assert!(!unit_text(APPIMAGE, Some("")).contains("Environment="));
}

#[test]
fn an_unusual_xdg_config_home_survives_systemds_parsing() {
    // CMP Contracts, Game Mode unit: Environment=XDG_CONFIG_HOME=<value>;
    // systemd splits at whitespace and expands % specifiers, so such a value
    // is quoted and % doubled
    assert!(
        unit_text(APPIMAGE, Some("/home/deck/My Config"))
            .contains("\nEnvironment=\"XDG_CONFIG_HOME=/home/deck/My Config\"\n")
    );
    assert!(
        unit_text(APPIMAGE, Some("/home/deck/100%"))
            .contains("\nEnvironment=XDG_CONFIG_HOME=/home/deck/100%%\n")
    );
    assert!(
        unit_text(APPIMAGE, Some(r#"/a"b\c"#))
            .contains("\nEnvironment=\"XDG_CONFIG_HOME=/a\\\"b\\\\c\"\n")
    );
}

#[test]
fn a_line_break_in_xdg_config_home_never_starts_a_directive() {
    // CMP Contracts, Game Mode unit: the unit is read line by line, so a
    // control character is written as a C escape inside the quotes
    let unit = unit_text(APPIMAGE, Some("/home/deck/x\nExecStartPre=/bin/sh -c evil"));
    assert!(
        unit.contains(
            "\nEnvironment=\"XDG_CONFIG_HOME=/home/deck/x\\x0aExecStartPre=/bin/sh -c evil\"\n"
        ),
        "{unit}"
    );
    assert!(!unit.lines().any(|line| line.starts_with("ExecStartPre")));
    assert_eq!(
        unit.lines().filter(|l| l.starts_with("ExecStart")).count(),
        1
    );
}

#[test]
fn execstart_quotes_the_appimage_as_dst_r31_quotes_exec() {
    // CMP Contracts, Game Mode unit: <$APPIMAGE> is quoted as DST-R31 quotes
    // Exec
    let path = r#"/home/deck/100% "odd"/$x.AppImage"#;
    assert_eq!(
        exec_start(path),
        r#""/home/deck/100%% \"odd\"/\$x.AppImage" --headless"#
    );
    assert_eq!(
        exec_start_of(&unit_text(path, None)),
        Some(exec_start(path).as_str())
    );
}

#[test]
fn turning_it_on_moves_the_credential_writes_the_unit_and_enables_it() {
    // CMP-R90: turning it on moves the credential to the credential file
    // (CMP-R62), writes the unit, runs daemon-reload, then enable; CMP-R62:
    // the new store is written first, then the old one deleted, and
    // credentialStore is file
    let deck = Deck::linked();
    deck.turn_on().unwrap();

    assert_eq!(deck.file.credential().as_deref(), Some(CREDENTIAL));
    assert_eq!(deck.secret_service.credential(), None);
    assert_eq!(
        fs::read_to_string(deck.unit()).unwrap(),
        unit_text(APPIMAGE, None)
    );
    assert_eq!(deck.systemctl.calls(), [RELOAD, ENABLE]);
    assert!(deck.game_mode());
    assert_eq!(deck.credential_store(), Some(StoreKind::File));
    let saved = deck.saved();
    assert_eq!(saved["settings"]["gameMode"], true);
    assert_eq!(saved["credentialStore"], "file");
}

#[test]
fn turning_it_on_never_starts_the_unit() {
    // CMP-R92: the tray-mode process holds the lock; it starts the unit when
    // it exits, not before
    let deck = Deck::linked();
    deck.turn_on().unwrap();
    assert!(
        deck.systemctl
            .calls()
            .iter()
            .all(|call| !call.contains(" start") && !call.contains("--now"))
    );
}

#[test]
fn a_failed_step_undoes_the_ones_before_and_leaves_the_toggle_off() {
    // CMP-R90: if a step fails, the steps before it are undone and the
    // toggle stays off ("Couldn't set up Game Mode uploading.")
    for failing in ["move", "write", "daemon-reload", "enable"] {
        let deck = Deck::linked();
        let mut machine = deck.machine(APPIMAGE);
        match failing {
            "move" => deck.secret_service.fail_reads(Some(locked())),
            "write" => {
                // A file where the unit's folder should be.
                let config = deck.tmp.path().join("config");
                fs::write(&config, "x").unwrap();
                machine.unit = Some(xdg::unit_path(&config));
            }
            step => deck.systemctl.fail(step),
        }

        assert!(
            machine.turn_on(&deck.store, &deck.keychain).is_err(),
            "{failing}"
        );
        assert!(!deck.game_mode(), "{failing}");
        assert_eq!(
            deck.credential_store(),
            Some(StoreKind::SecretService),
            "{failing}"
        );
        deck.secret_service.fail_reads(None);
        assert_eq!(
            deck.secret_service.credential().as_deref(),
            Some(CREDENTIAL),
            "{failing}"
        );
        assert_eq!(deck.file.credential(), None, "{failing}");
        assert!(!deck.unit().exists(), "{failing}");
        let calls: &[&str] = match failing {
            "move" | "write" => &[],
            "daemon-reload" => &[RELOAD],
            _ => &[RELOAD, ENABLE, RELOAD],
        };
        assert_eq!(deck.systemctl.calls(), calls, "{failing}");
        assert!(!deck.tmp.path().join("state.json").exists(), "{failing}");
    }
}

#[test]
fn a_failed_delete_from_the_old_store_is_retried_at_the_next_start() {
    // CMP-R62: moving writes the new store first, then deletes the old one;
    // a delete that fails is retried at the next start
    let deck = Deck::linked();
    deck.secret_service.fail_deletes(Some(locked()));
    deck.turn_on().unwrap();
    assert!(deck.game_mode());
    assert_eq!(
        deck.secret_service.credential().as_deref(),
        Some(CREDENTIAL)
    );

    // The next start, with the keyring answering.
    deck.secret_service.fail_deletes(None);
    let kind = deck.credential_store();
    assert!(deck.keychain.delete_from_other_stores(kind).is_empty());
    assert_eq!(deck.secret_service.credential(), None);
    assert_eq!(deck.file.credential().as_deref(), Some(CREDENTIAL));
}

#[test]
fn turning_it_on_unlinked_moves_nothing() {
    // CMP-R62: with the toggle on, a later link stores the credential in the
    // credential file (store_new with Game Mode on); nothing to move now
    let deck = Deck::new(State {
        first_run_done: true,
        ..State::default()
    });
    deck.secret_service.fail_reads(Some(locked()));
    deck.turn_on().unwrap();
    assert!(deck.game_mode());
    assert_eq!(deck.credential_store(), None);
    assert_eq!(deck.file.writes(), 0);
}

#[test]
fn turning_it_off_disables_deletes_reloads_and_moves_the_credential_back() {
    // CMP-R90: turning it off runs disable --now, deletes the unit, runs
    // daemon-reload and moves the credential back to the Secret Service when
    // one answers (CMP-R62)
    let deck = Deck::on();
    deck.turn_off().unwrap();

    assert_eq!(deck.systemctl.calls(), [DISABLE, RELOAD]);
    assert!(!deck.unit().exists());
    assert!(!deck.game_mode());
    assert_eq!(
        deck.secret_service.credential().as_deref(),
        Some(CREDENTIAL)
    );
    assert_eq!(deck.file.credential(), None);
    assert_eq!(deck.credential_store(), Some(StoreKind::SecretService));
    let saved = deck.saved();
    assert_eq!(saved["settings"]["gameMode"], false);
    assert_eq!(saved["credentialStore"], "secret-service");
}

#[test]
fn turning_it_off_with_a_locked_keyring_keeps_the_credential_file() {
    // CMP-R90: back to the Secret Service only when one answers; CMP-R62:
    // otherwise the credential file
    let deck = Deck::on();
    deck.secret_service.fail_writes(Some(locked()));
    deck.turn_off().unwrap();

    assert!(!deck.game_mode());
    assert!(!deck.unit().exists());
    assert_eq!(deck.file.credential().as_deref(), Some(CREDENTIAL));
    assert_eq!(deck.secret_service.credential(), None);
    assert_eq!(deck.credential_store(), Some(StoreKind::File));
}

#[test]
fn a_failing_systemctl_keeps_the_toggle_on() {
    // CMP-R90: if systemctl fails, the toggle stays on ("Couldn't turn off
    // Game Mode uploading.")
    for failing in ["disable", "daemon-reload"] {
        let deck = Deck::on();
        deck.systemctl.fail(failing);
        assert!(deck.turn_off().is_err(), "{failing}");
        assert!(deck.game_mode(), "{failing}");
        assert_eq!(deck.credential_store(), Some(StoreKind::File), "{failing}");
        assert_eq!(
            deck.file.credential().as_deref(),
            Some(CREDENTIAL),
            "{failing}"
        );
        assert_eq!(deck.secret_service.writes(), 0, "{failing}");
        // The unit stays until disable worked.
        assert_eq!(deck.unit().exists(), failing == "disable", "{failing}");
    }
}

#[test]
fn a_start_rewrites_the_unit_when_the_appimage_moved() {
    // CMP-R91: while the toggle is on and $APPIMAGE is set, every tray-mode
    // start rewrites the unit if its ExecStart path is no longer $APPIMAGE,
    // and runs daemon-reload
    let deck = Deck::on();
    assert_eq!(deck.machine(APPIMAGE).at_start(true), Ok(Done::Unchanged));
    assert!(deck.systemctl.calls().is_empty());

    assert_eq!(deck.machine(MOVED).at_start(true), Ok(Done::Rewrote));
    assert_eq!(
        fs::read_to_string(deck.unit()).unwrap(),
        unit_text(MOVED, None)
    );
    assert_eq!(deck.systemctl.calls(), [RELOAD]);

    assert_eq!(deck.machine(MOVED).at_start(true), Ok(Done::Unchanged));
    assert_eq!(deck.systemctl.calls(), [RELOAD]);
}

#[test]
fn a_start_rewrites_a_missing_unit_but_never_one_with_the_toggle_off() {
    // CMP-R91: a unit whose ExecStart isn't $APPIMAGE, here none at all, is
    // rewritten; CMP-R93: without the toggle nothing is written
    let deck = Deck::on();
    fs::remove_file(deck.unit()).unwrap();
    assert_eq!(deck.machine(APPIMAGE).at_start(false), Ok(Done::Off));
    assert!(!deck.unit().exists());

    assert_eq!(deck.machine(APPIMAGE).at_start(true), Ok(Done::Rewrote));
    assert_eq!(
        fs::read_to_string(deck.unit()).unwrap(),
        unit_text(APPIMAGE, None)
    );
}

#[test]
fn a_start_without_appimage_leaves_the_unit() {
    // CMP-R91: only while $APPIMAGE is set
    let deck = Deck::on();
    let mut machine = deck.machine(MOVED);
    machine.appimage = None;
    assert_eq!(machine.at_start(true), Ok(Done::NoPath));
    assert_eq!(
        fs::read_to_string(deck.unit()).unwrap(),
        unit_text(APPIMAGE, None)
    );
    assert!(deck.systemctl.calls().is_empty());
}

#[test]
fn the_texts_are_cmp_r90s() {
    // CMP-R90: "Couldn't set up Game Mode uploading." and "Couldn't turn off
    // Game Mode uploading."
    assert_eq!(TURN_ON_FAILED, "Couldn't set up Game Mode uploading.");
    assert_eq!(TURN_OFF_FAILED, "Couldn't turn off Game Mode uploading.");
}

#[cfg(unix)]
#[test]
fn turning_it_on_runs_the_real_runner_against_a_fake_systemctl() {
    // CMP-R90, CMP Contracts "Game Mode unit": the unit at
    // $XDG_CONFIG_HOME/systemd/user/pagle-companion.service, then systemctl
    // --user daemon-reload and enable, run as processes. A fake script
    // stands in for systemctl
    use std::os::unix::fs::PermissionsExt;

    use crate::systemctl::OsSystemctl;

    let deck = Deck::linked();
    let log = deck.tmp.path().join("systemctl.log");
    let script = deck.tmp.path().join("systemctl");
    fs::write(
        &script,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\n", log.display()),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let config = deck.tmp.path().join("config");
    let machine = Machine {
        unit: Some(xdg::unit_path(&config)),
        appimage: Some(APPIMAGE.into()),
        xdg_config_home: Some(config.to_str().unwrap().into()),
        systemctl: Box::new(OsSystemctl::with_program(&script)),
    };

    machine.turn_on(&deck.store, &deck.keychain).unwrap();
    let unit = fs::read_to_string(config.join("systemd/user/pagle-companion.service")).unwrap();
    assert_eq!(unit, unit_text(APPIMAGE, Some(config.to_str().unwrap())));
    assert!(unit.contains(&format!(
        "\nEnvironment=XDG_CONFIG_HOME={}\n",
        config.display()
    )));
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "--user daemon-reload\n--user enable pagle-companion.service\n"
    );
}

//! Checks `tauri.conf.json` and the NSIS hooks against DST Contracts,
//! "Tauri configuration", for the rows the Windows installer owns.

use std::path::Path;

use serde_json::Value;

const CONF: &str = include_str!("../tauri.conf.json");
const HOOKS: &str = include_str!("../windows/hooks.nsh");

fn conf() -> Value {
    serde_json::from_str(CONF).expect("tauri.conf.json is JSON")
}

fn nsis(conf: &Value) -> &Value {
    &conf["bundle"]["windows"]["nsis"]
}

#[test]
fn the_packages_are_the_nsis_installer_and_the_appimage_only() {
    // DST-R1: bundle.targets is ["nsis", "appimage"], no MSI, .deb or .rpm
    assert_eq!(
        conf()["bundle"]["targets"],
        serde_json::json!(["nsis", "appimage"])
    );
}

#[test]
fn the_installer_is_per_user() {
    // DST-R10: installMode currentUser, so no UAC prompt and an install
    // under %LOCALAPPDATA%; DST-R12: one independent install per OS user
    assert_eq!(nsis(&conf())["installMode"], "currentUser");
}

#[test]
fn the_publisher_is_pagle() {
    // DST-R10: the Installed apps entry's publisher (bundle.publisher)
    assert_eq!(conf()["bundle"]["publisher"], "Pagle");
}

#[test]
fn the_installer_is_in_english_only() {
    // DST-R4, DST UI "Installer": English pages, no language selector
    let conf = conf();
    assert_eq!(nsis(&conf)["languages"], serde_json::json!(["English"]));
    assert_ne!(nsis(&conf)["displayLanguageSelector"], true);
}

#[test]
fn webview2_is_installed_the_default_way() {
    // DST-R11: Tauri's default webviewInstallMode, the download bootstrapper
    assert!(
        conf()["bundle"]["windows"]
            .get("webviewInstallMode")
            .is_none()
    );
}

#[test]
fn the_hooks_file_is_the_one_tauri_includes() {
    // DST-R10, DST-R14: bundle.windows.nsis.installerHooks
    let conf = conf();
    let hooks = nsis(&conf)["installerHooks"].as_str().unwrap();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(hooks);
    assert_eq!(std::fs::read_to_string(path).unwrap(), HOOKS);
}

#[test]
fn installing_and_uninstalling_close_the_companion_with_quit() {
    // DST-R10, DST-R14 step 1: --quit through the single-instance channel
    for hook in ["NSIS_HOOK_PREINSTALL", "NSIS_HOOK_PREUNINSTALL"] {
        assert!(
            macro_body(hook).contains("PAGLE_CLOSE_COMPANION"),
            "{hook} closes the Companion"
        );
    }
    assert!(macro_body("PAGLE_CLOSE_COMPANION").contains("--quit"));
}

#[test]
fn uninstalling_runs_remove_for_at_most_the_removal_wait() {
    // DST-R14 steps 2 and 3; DST Tunables: removal wait 30 seconds
    assert!(HOOKS.contains("!define PAGLE_REMOVAL_WAIT_MS 30000\n"));
    let body = macro_body("NSIS_HOOK_PREUNINSTALL");
    assert!(body.contains(
        r#"nsExec::Exec /TIMEOUT=${PAGLE_REMOVAL_WAIT_MS} '"$INSTDIR\${MAINBINARYNAME}.exe" --remove'"#
    ));
}

#[test]
fn an_update_or_an_install_over_keeps_local_state() {
    // DST-R10, DST-R13, DST-R14: /UPDATE or the installer's marker skips
    // --remove
    let body = macro_body("NSIS_HOOK_PREUNINSTALL");
    assert!(body.contains("${If} $UpdateMode = 1\n  ${OrIf} $0 == \"1\""));
    assert!(body.contains("${If} $PagleKeepState = 0"));
}

/// The lines between `!macro <name>` and its `!macroend`.
fn macro_body(name: &str) -> &'static str {
    let start = HOOKS
        .find(&format!("!macro {name}\n"))
        .unwrap_or_else(|| panic!("hooks.nsh defines {name}"));
    let body = &HOOKS[start..];
    &body[..body.find("!macroend").unwrap()]
}

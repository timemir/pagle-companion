//! Checks `tauri.conf.json` and the NSIS hooks against DST Contracts,
//! "Tauri configuration", for the rows the Windows installer and the updater
//! own.

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
fn the_window_runs_only_its_own_scripts() {
    // The window can call the Companion's commands: its content security
    // policy lets it load scripts from the app alone, and no plugin, base
    // URL, form target or frame from anywhere
    let csp = conf()["app"]["security"]["csp"]
        .as_str()
        .expect("a content security policy")
        .to_owned();
    let directives: Vec<&str> = csp.split(';').map(str::trim).collect();
    for wanted in [
        "default-src 'self'",
        "object-src 'none'",
        "base-uri 'none'",
        "form-action 'none'",
        "frame-src 'none'",
    ] {
        assert!(directives.contains(&wanted), "{wanted} in {csp}");
    }
    assert!(
        !directives.iter().any(|d| d.starts_with("script-src")),
        "{csp}"
    );
    assert!(!csp.contains("unsafe-eval"), "{csp}");
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
    // Decided first, before Tauri's uninstaller deletes anything.
    assert!(
        body.find("StrCpy $PagleKeepState 1").unwrap()
            < body.find("!insertmacro PAGLE_CLOSE_COMPANION").unwrap()
    );
}

#[test]
fn the_installer_marks_the_uninstaller_it_runs() {
    // DST-R10, DST-R14: Tauri's reinstall page runs the old uninstaller
    // without /UPDATE, so the installer marks its environment when its
    // window opens. An older uninstaller reads a newer installer's marker,
    // so the name never changes.
    assert!(HOOKS.contains("!define PAGLE_INSTALLER_ENV \"PAGLE_COMPANION_INSTALLER\"\n"));
    assert!(HOOKS.contains("!define MUI_CUSTOMFUNCTION_GUIINIT PagleMarkInstaller\n"));
    assert!(
        function_body("PagleMarkInstaller")
            .contains(r#"SetEnvironmentVariable(t "${PAGLE_INSTALLER_ENV}", t "1")"#)
    );
    assert!(macro_body("NSIS_HOOK_PREUNINSTALL").contains("ReadEnvStr $0 ${PAGLE_INSTALLER_ENV}"));
    // The Companion the installer starts at the end doesn't inherit it.
    assert!(
        macro_body("NSIS_HOOK_POSTINSTALL")
            .contains(r#"SetEnvironmentVariable(t "${PAGLE_INSTALLER_ENV}", p 0)"#)
    );
}

#[test]
fn an_update_or_an_install_over_keeps_the_autostart_entry() {
    // DST-R10, DST-R13: Tauri's uninstaller deletes the Run value outside
    // update mode, so it is read before and written back after
    const RUN: &str = r#"HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}""#;
    assert!(
        macro_body("NSIS_HOOK_PREUNINSTALL").contains(&format!("ReadRegStr $PagleRunValue {RUN}"))
    );
    let (keep, _) = post_uninstall_branches();
    assert!(keep.contains(&format!("WriteRegStr {RUN} $PagleRunValue")));
}

#[test]
fn uninstalling_leaves_no_registry_key_whatever_the_checkbox() {
    // DST-R14: Tauri keeps Software\Pagle\Pagle Companion unless "Delete the
    // application data" is ticked; an uninstall deletes it either way, and
    // the checkbox changes nothing the hooks do
    let (keep, uninstall) = post_uninstall_branches();
    assert!(uninstall.contains(r#"DeleteRegKey SHCTX "${MANUPRODUCTKEY}""#));
    assert!(uninstall.contains(r#"DeleteRegKey /ifempty SHCTX "${MANUKEY}""#));
    assert!(!keep.contains("DeleteRegKey"));
    assert!(!HOOKS.contains("DeleteAppDataCheckbox"));
}

#[test]
fn a_remove_ended_after_the_removal_wait_is_logged() {
    // DST-R14 step 3: nsExec returns "timeout" when it ended --remove
    let body = macro_body("NSIS_HOOK_PREUNINSTALL");
    assert!(body.contains(
        "${If} $0 == \"timeout\"\n      DetailPrint \"--remove still ran after the removal wait and was ended\""
    ));
}

#[test]
fn the_updater_checks_the_one_endpoint() {
    // DST-R50: plugins.updater.endpoints is the single latest.json URL
    assert_eq!(
        conf()["plugins"]["updater"]["endpoints"],
        serde_json::json!([
            "https://github.com/timemir/pagle-companion/releases/latest/download/latest.json"
        ])
    );
}

#[test]
fn updates_install_passive_on_windows() {
    // DST-R13: plugins.updater.windows.installMode is passive, read by the
    // plugin as its passive mode (a progress bar, no questions)
    let conf = conf();
    assert_eq!(
        conf["plugins"]["updater"]["windows"]["installMode"],
        "passive"
    );
    let config: tauri_plugin_updater::Config =
        serde_json::from_value(conf["plugins"]["updater"].clone()).unwrap();
    assert_eq!(
        config.windows.unwrap().install_mode.nsis_args(),
        ["/P"].as_slice()
    );
}

/// The lines between `!macro <name>` and its `!macroend`.
fn macro_body(name: &str) -> &'static str {
    let start = HOOKS
        .find(&format!("!macro {name}\n"))
        .unwrap_or_else(|| panic!("hooks.nsh defines {name}"));
    let body = &HOOKS[start..];
    &body[..body.find("!macroend").unwrap()]
}

/// The lines between `Function <name>` and its `FunctionEnd`.
fn function_body(name: &str) -> &'static str {
    let start = HOOKS
        .find(&format!("Function {name}\n"))
        .unwrap_or_else(|| panic!("hooks.nsh defines {name}"));
    let body = &HOOKS[start..];
    &body[..body.find("FunctionEnd").unwrap()]
}

/// `NSIS_HOOK_POSTUNINSTALL`'s two branches: during an update or an install
/// over an existing one, and during an uninstall.
fn post_uninstall_branches() -> (&'static str, &'static str) {
    let body = macro_body("NSIS_HOOK_POSTUNINSTALL");
    let keep = body
        .split_once("  ${If} $PagleKeepState = 1\n")
        .expect("the hook branches on $PagleKeepState")
        .1;
    keep.split_once("\n  ${Else}\n")
        .expect("the hook has an uninstall branch")
}

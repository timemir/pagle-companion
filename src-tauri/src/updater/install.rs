//! How a waiting update is started (DST-R54, DST-R13): the command that runs
//! the new version, and the [`Launcher`] that starts it, the seam tests
//! record launches through instead of running them.
//!
//! - **Windows:** the kept installer, run as the updater plugin runs an NSIS
//!   update: `plugins.updater.windows.installMode`'s arguments (`/P`,
//!   passive: a progress bar, no questions; [`Ui::Passive`]) or, for an
//!   install the Player didn't choose, quiet mode's (`/S`: no progress bar,
//!   no window; [`Ui::Quiet`], DST-R76), `/UPDATE`, then `/R
//!   /ARGS` and this process's arguments, so that the installer starts the
//!   new version with them once it is installed (DST-R13), with
//!   `--autostart` added after a quiet one (DST-R76); then the configured
//!   `installerArgs` (none). `/UPDATE` is how the installer and
//!   `windows/hooks.nsh` know it is an update: Tauri's installer skips its
//!   reinstall page, so the old uninstaller and `--remove` never run, and
//!   an uninstaller run with `/UPDATE` keeps local state and the `Run` value
//!   (DST-R14). The installer closes the Companion with `--quit` before it
//!   replaces the program files (`PAGLE_CLOSE_COMPANION`).
//! - **Linux:** the AppImage at `$APPIMAGE`, already the new version (`keep`),
//!   started with this process's arguments from its own folder, again with
//!   `--autostart` added after a quiet install.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use tauri_plugin_updater::Config;

use crate::autostart;

/// How the Windows installer shows itself. Linux has nothing to show: the
/// new AppImage just starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ui {
    /// `plugins.updater.windows.installMode`, passive: a progress bar and no
    /// questions. **Restart to update** and the install at start (DST-R13,
    /// DST-R54).
    Passive,
    /// NSIS `/S`: no progress bar, no window and so no notification. The
    /// installs the Player didn't choose: the idle install and the forced
    /// one under update required (DST-R76, DST-R56).
    Quiet,
}

/// What starts the new version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    pub command_line: CommandLine,
}

/// The arguments a [`Launch`] passes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandLine {
    /// Windows: the installer's command line after the program, passed as it
    /// is. NSIS reads options out of the whole line (`GetOptions`), and the
    /// arguments after `/ARGS` are already quoted for it.
    Raw(String),
    /// Linux: the arguments one by one.
    Args(Vec<OsString>),
}

/// Starts a [`Launch`] and returns once it has started, without waiting for
/// it.
pub trait Launcher {
    fn launch(&self, launch: &Launch) -> io::Result<()>;
}

/// Starts the program for real.
pub struct SystemLauncher;

impl Launcher for SystemLauncher {
    fn launch(&self, launch: &Launch) -> io::Result<()> {
        let mut command = Command::new(&launch.program);
        match &launch.command_line {
            CommandLine::Raw(line) => raw_arg(&mut command, line)?,
            CommandLine::Args(args) => {
                command.args(args);
            }
        }
        if let Some(dir) = launch.program.parent().filter(|d| d.is_dir()) {
            command.current_dir(dir);
        }
        command.spawn().map(drop)
    }
}

#[cfg(windows)]
fn raw_arg(command: &mut Command, line: &str) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    command.raw_arg(line);
    Ok(())
}

#[cfg(not(windows))]
fn raw_arg(_command: &mut Command, _line: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a raw command line is Windows only",
    ))
}

/// NSIS's silent option, which the updater plugin's `quiet` install mode
/// passes in place of passive's `/P`.
///
/// A plain `/S` install starts nothing afterwards (DST-R10). With `/R`, Tauri's
/// installer template (tauri-cli 2.12, `.onInstSuccess`) starts the installed
/// Companion with the `/ARGS` value for `${Silent}` as for passive mode, so
/// the quiet update keeps `/R /ARGS` and restarts with the old arguments as
/// DST-R13's passive one does, plus `--autostart` ([`relaunch_args`],
/// DST-R76). `/UPDATE` still sets `$UpdateMode`,
/// which skips the reinstall page and keeps local state (DST-R14).
pub const QUIET: &str = "/S";

/// The kept installer, run as an update shown as `ui` with `args`, this
/// process's arguments without the program (DST-R13).
pub fn installer_launch(installer: &Path, config: &Config, ui: Ui, args: &[String]) -> Launch {
    let windows = config.windows.clone().unwrap_or_default();
    let (mut line, restarts): (Vec<String>, bool) = match ui {
        Ui::Passive => (
            windows
                .install_mode
                .nsis_args()
                .iter()
                .map(|arg| (*arg).to_owned())
                .collect(),
            // The plugin restarts the app after every mode but basicUi,
            // whose finish page has its own "Run" box.
            windows.install_mode.to_string() != "basicUi",
        ),
        // `/S` in place of `/P`; the rest as passive's, so the new version
        // starts again with the same arguments.
        Ui::Quiet => (vec![QUIET.to_owned()], true),
    };
    line.push("/UPDATE".into());
    if restarts {
        line.push("/R".into());
    }
    line.push("/ARGS".into());
    // Each quoted on its own and joined by spaces, so an added
    // `--autostart` is one more argument of the new version's.
    line.extend(
        relaunch_args(ui, args)
            .iter()
            .map(|arg| escape_nsis_arg(arg)),
    );
    line.extend(
        windows
            .installer_args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned()),
    );
    Launch {
        program: installer.to_path_buf(),
        command_line: CommandLine::Raw(line.join(" ")),
    }
}

/// The replaced AppImage, started after an install shown as `ui` with
/// `args`, this process's arguments without the program (DST-R54).
pub fn appimage_launch(appimage: &Path, ui: Ui, args: &[String]) -> Launch {
    Launch {
        program: appimage.to_path_buf(),
        command_line: CommandLine::Args(
            relaunch_args(ui, args)
                .into_iter()
                .map(OsString::from)
                .collect(),
        ),
    }
}

/// The arguments the new version starts with: this process's `args` after a
/// passive install (DST-R13), and after a quiet one the same with
/// `--autostart` added when it isn't there, so the Companion comes back in
/// the tray with no window, even when it was started from the Start menu
/// (DST-R76).
pub fn relaunch_args(ui: Ui, args: &[String]) -> Vec<String> {
    let mut relaunch = args.to_vec();
    if ui == Ui::Quiet && !args.iter().any(|arg| arg == autostart::ARG) {
        relaunch.push(autostart::ARG.to_owned());
    }
    relaunch
}

/// Quotes one argument after `/ARGS` as the updater plugin does
/// (`escape_nsis_current_exe_arg`): Windows' rules, and quoted also when it
/// holds a `/`, so that NSIS doesn't read it as an option of its own.
fn escape_nsis_arg(arg: &str) -> String {
    let quote = arg.is_empty() || arg.contains([' ', '\t', '/']);
    let mut escaped = String::with_capacity(arg.len() + 2);
    if quote {
        escaped.push('"');
    }
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
        } else {
            if c == '"' {
                // n backslashes before a quote become 2n + 1.
                escaped.extend(std::iter::repeat_n('\\', backslashes + 1));
            }
            backslashes = 0;
        }
        escaped.push(c);
    }
    if quote {
        // n backslashes before the closing quote become 2n.
        escaped.extend(std::iter::repeat_n('\\', backslashes));
        escaped.push('"');
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::release;

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_owned()).collect()
    }

    fn raw(launch: &Launch) -> &str {
        match &launch.command_line {
            CommandLine::Raw(line) => line,
            CommandLine::Args(_) => panic!("not a raw command line"),
        }
    }

    #[test]
    fn the_installer_runs_passive_as_an_update_and_restarts_with_the_same_arguments() {
        // DST-R13, DST Contracts "Tauri configuration": installMode passive
        // (/P), /UPDATE so that hooks.nsh keeps local state, and /R /ARGS
        // with the old process's arguments, so a login start comes back
        // with --autostart and no window
        let installer = Path::new(r"C:\state\pagle-companion_1.2.0_x64-setup.exe");
        let config = release::plugin_config();

        let launch = installer_launch(installer, &config, Ui::Passive, &args(&["--autostart"]));
        assert_eq!(launch.program, installer);
        assert_eq!(raw(&launch), "/P /UPDATE /R /ARGS --autostart");

        let launch = installer_launch(installer, &config, Ui::Passive, &[]);
        assert_eq!(raw(&launch), "/P /UPDATE /R /ARGS");

        // Whatever the arguments, nothing is added (DST-R13).
        let launch = installer_launch(installer, &config, Ui::Passive, &args(&["a b", "--x"]));
        assert_eq!(raw(&launch), "/P /UPDATE /R /ARGS \"a b\" --x");
    }

    #[test]
    fn a_quiet_install_runs_silent_as_an_update_and_restarts_in_the_tray() {
        // DST-R76: the idle and forced installs run the installer quietly
        // (NSIS /S) with the start arguments of the running process plus
        // --autostart, so the Companion comes back with no window: /S in
        // place of /P, /UPDATE /R /ARGS as passive's
        let installer = Path::new(r"C:\state\pagle-companion_1.2.0_x64-setup.exe");
        let config = release::plugin_config();

        // No arguments: a Start menu start comes back in the tray.
        let launch = installer_launch(installer, &config, Ui::Quiet, &[]);
        assert_eq!(launch.program, installer);
        assert_eq!(raw(&launch), "/S /UPDATE /R /ARGS --autostart");

        // --autostart already there: not twice.
        let launch = installer_launch(installer, &config, Ui::Quiet, &args(&["--autostart"]));
        assert_eq!(raw(&launch), "/S /UPDATE /R /ARGS --autostart");

        // Other arguments: kept, quoted as before, --autostart after them.
        let launch = installer_launch(installer, &config, Ui::Quiet, &args(&["a b", "--x"]));
        assert_eq!(raw(&launch), "/S /UPDATE /R /ARGS \"a b\" --x --autostart");
        let launch = installer_launch(
            installer,
            &config,
            Ui::Quiet,
            &args(&["--x", "--autostart", "/y"]),
        );
        assert_eq!(raw(&launch), "/S /UPDATE /R /ARGS --x --autostart \"/y\"");
    }

    #[test]
    fn only_a_quiet_install_adds_autostart() {
        // DST-R13, DST-R76: the passive installs keep the old arguments as
        // they are; the quiet ones add --autostart once
        assert_eq!(relaunch_args(Ui::Passive, &[]), Vec::<String>::new());
        assert_eq!(relaunch_args(Ui::Passive, &args(&["a"])), args(&["a"]));
        assert_eq!(relaunch_args(Ui::Quiet, &[]), args(&["--autostart"]));
        assert_eq!(
            relaunch_args(Ui::Quiet, &args(&["a", "--autostart"])),
            args(&["a", "--autostart"])
        );
        assert_eq!(
            relaunch_args(Ui::Quiet, &args(&["a"])),
            args(&["a", "--autostart"])
        );
        assert_eq!(crate::autostart::ARG, "--autostart");
    }

    #[test]
    fn the_command_line_is_the_one_hooks_nsh_reads_as_an_update() {
        // DST-R13, DST-R14: hooks.nsh keeps local state when the installer's
        // $UpdateMode is 1, which Tauri's installer sets from /UPDATE, and
        // closes the Companion with --quit before installing; quiet or not
        let hooks = include_str!("../../windows/hooks.nsh");
        assert!(hooks.contains("${If} $UpdateMode = 1"));
        assert!(hooks.contains("--quit"));
        let config = release::plugin_config();
        let launch = installer_launch(Path::new("setup.exe"), &config, Ui::Passive, &[]);
        let options: Vec<&str> = raw(&launch).split(' ').collect();
        assert!(options.contains(&"/UPDATE"));
        assert!(options.contains(&"/P"));
        assert!(!options.contains(&"/S"), "passive, not silent");

        let launch = installer_launch(Path::new("setup.exe"), &config, Ui::Quiet, &[]);
        let options: Vec<&str> = raw(&launch).split(' ').collect();
        assert!(options.contains(&"/UPDATE"));
        assert!(options.contains(&"/S"));
        assert!(!options.contains(&"/P"), "silent, not passive");
    }

    #[test]
    fn arguments_after_args_are_quoted_as_the_updater_plugin_quotes_them() {
        // DST-R13: the installer starts the new version with the arguments
        // the old process was started with, so they are quoted exactly as
        // the plugin quotes them (its own test cases)
        let cases = [
            ("something", "something"),
            ("--flag", "--flag"),
            ("--empty=", "--empty="),
            ("--arg=value", "--arg=value"),
            ("some space", "\"some space\""),
            ("--arg value", "\"--arg value\""),
            ("--arg=unwrapped space", "\"--arg=unwrapped space\""),
            ("--arg=\"wrapped\"", "--arg=\\\"wrapped\\\""),
            ("--arg=\"wrapped space\"", "\"--arg=\\\"wrapped space\\\"\""),
            (
                "--arg=midword\"wrapped space\"",
                "\"--arg=midword\\\"wrapped space\\\"\"",
            ),
            ("", "\"\""),
            // Also quoted: a slash, which NSIS would read as an option.
            ("/S", "\"/S\""),
            // Backslashes double only before a quote.
            (r"C:\dir\", r"C:\dir\"),
            (r"C:\my dir\", r#""C:\my dir\\""#),
        ];
        for (arg, escaped) in cases {
            assert_eq!(escape_nsis_arg(arg), escaped, "{arg}");
        }
    }

    #[test]
    fn the_appimage_starts_with_the_same_arguments() {
        // DST-R54: Restart to update starts the new file at $APPIMAGE with
        // the same arguments
        let appimage = Path::new("/home/deck/Applications/pagle-companion.AppImage");
        let launch = appimage_launch(appimage, Ui::Passive, &args(&["a b"]));
        assert_eq!(launch.program, appimage);
        assert_eq!(launch.command_line, CommandLine::Args(vec!["a b".into()]));

        // DST-R76: after the idle or forced install, back in the tray with no
        // window: --autostart added when missing, never twice.
        let launch = appimage_launch(appimage, Ui::Quiet, &[]);
        assert_eq!(
            launch.command_line,
            CommandLine::Args(vec!["--autostart".into()])
        );
        let launch = appimage_launch(appimage, Ui::Quiet, &args(&["a b"]));
        assert_eq!(
            launch.command_line,
            CommandLine::Args(vec!["a b".into(), "--autostart".into()])
        );
        let launch = appimage_launch(appimage, Ui::Quiet, &args(&["--autostart"]));
        assert_eq!(
            launch.command_line,
            CommandLine::Args(vec!["--autostart".into()])
        );
    }

    /// Waits for `path` to hold a whole line, for at most ten seconds.
    fn read_when_written(path: &Path) -> String {
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(path)
                && text.ends_with('\n')
            {
                return text;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("{} wasn't written", path.display());
    }

    #[cfg(windows)]
    #[test]
    fn the_installer_command_line_reaches_the_installer_as_it_is() {
        // DST-R13: a fake installer that records its command line gets
        // exactly the update's options
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-setup.bat");
        std::fs::write(&fake, "@echo off\r\necho %*> \"%~dp0args.txt\"\r\n").unwrap();
        let config = release::plugin_config();
        let launch = installer_launch(&fake, &config, Ui::Passive, &args(&["--autostart"]));

        SystemLauncher.launch(&launch).unwrap();
        let recorded = read_when_written(&tmp.path().join("args.txt"));
        assert_eq!(recorded.trim_end(), "/P /UPDATE /R /ARGS --autostart");

        // DST-R76: the quiet install's too, --autostart added.
        std::fs::remove_file(tmp.path().join("args.txt")).unwrap();
        let launch = installer_launch(&fake, &config, Ui::Quiet, &args(&["--x"]));
        SystemLauncher.launch(&launch).unwrap();
        let recorded = read_when_written(&tmp.path().join("args.txt"));
        assert_eq!(recorded.trim_end(), "/S /UPDATE /R /ARGS --x --autostart");
    }

    #[cfg(unix)]
    #[test]
    fn the_appimage_is_started_with_the_arguments_from_its_folder() {
        // DST-R54: Restart to update starts $APPIMAGE with the same
        // arguments; a fake AppImage records them
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("pagle-companion.AppImage");
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s|' \"$(pwd -P)\" \"$@\" > args.tmp\necho >> args.tmp\nmv args.tmp args.txt\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        SystemLauncher
            .launch(&appimage_launch(
                &fake,
                Ui::Passive,
                &args(&["--autostart", "a b"]),
            ))
            .unwrap();
        let recorded = read_when_written(&tmp.path().join("args.txt"));
        let dir = tmp.path().canonicalize().unwrap();
        assert_eq!(recorded, format!("{}|--autostart|a b|\n", dir.display()));
    }
}

//! How a waiting update is started (DST-R54, DST-R13): the command that runs
//! the new version, and the [`Launcher`] that starts it, the seam tests
//! record launches through instead of running them.
//!
//! - **Windows:** the kept installer, run as the updater plugin runs an NSIS
//!   update: `plugins.updater.windows.installMode`'s arguments (`/P`,
//!   passive: a progress bar, no questions), `/UPDATE`, then `/R /ARGS` and
//!   this process's arguments, so that the installer starts the new version
//!   with them once it is installed (DST-R13); then the configured
//!   `installerArgs` (none). `/UPDATE` is how the installer and
//!   `windows/hooks.nsh` know it is an update: Tauri's installer skips its
//!   reinstall page, so the old uninstaller and `--remove` never run, and
//!   an uninstaller run with `/UPDATE` keeps local state and the `Run` value
//!   (DST-R14). The installer closes the Companion with `--quit` before it
//!   replaces the program files (`PAGLE_CLOSE_COMPANION`).
//! - **Linux:** the AppImage at `$APPIMAGE`, already the new version (`keep`),
//!   started with this process's arguments from its own folder.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use tauri_plugin_updater::Config;

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

/// The kept installer, run as an update with `args`, this process's
/// arguments without the program (DST-R13).
pub fn installer_launch(installer: &Path, config: &Config, args: &[String]) -> Launch {
    let windows = config.windows.clone().unwrap_or_default();
    let mut line: Vec<String> = windows
        .install_mode
        .nsis_args()
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
    line.push("/UPDATE".into());
    // The plugin restarts the app after every mode but basicUi, whose finish
    // page has its own "Run" box.
    if windows.install_mode.to_string() != "basicUi" {
        line.push("/R".into());
    }
    line.push("/ARGS".into());
    line.extend(args.iter().map(|arg| escape_nsis_arg(arg)));
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

/// The replaced AppImage, started with `args`, this process's arguments
/// without the program (DST-R54).
pub fn appimage_launch(appimage: &Path, args: &[String]) -> Launch {
    Launch {
        program: appimage.to_path_buf(),
        command_line: CommandLine::Args(args.iter().map(OsString::from).collect()),
    }
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

        let launch = installer_launch(installer, &config, &args(&["--autostart"]));
        assert_eq!(launch.program, installer);
        assert_eq!(raw(&launch), "/P /UPDATE /R /ARGS --autostart");

        let launch = installer_launch(installer, &config, &[]);
        assert_eq!(raw(&launch), "/P /UPDATE /R /ARGS");
    }

    #[test]
    fn the_command_line_is_the_one_hooks_nsh_reads_as_an_update() {
        // DST-R13, DST-R14: hooks.nsh keeps local state when the installer's
        // $UpdateMode is 1, which Tauri's installer sets from /UPDATE, and
        // closes the Companion with --quit before installing
        let hooks = include_str!("../../windows/hooks.nsh");
        assert!(hooks.contains("${If} $UpdateMode = 1"));
        assert!(hooks.contains("--quit"));
        let launch = installer_launch(Path::new("setup.exe"), &release::plugin_config(), &[]);
        let options: Vec<&str> = raw(&launch).split(' ').collect();
        assert!(options.contains(&"/UPDATE"));
        assert!(options.contains(&"/P"));
        assert!(!options.contains(&"/S"), "passive, not silent");
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
        let launch = appimage_launch(appimage, &args(&["--autostart", "a b"]));
        assert_eq!(launch.program, appimage);
        assert_eq!(
            launch.command_line,
            CommandLine::Args(vec!["--autostart".into(), "a b".into()])
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
        let launch = installer_launch(&fake, &release::plugin_config(), &args(&["--autostart"]));

        SystemLauncher.launch(&launch).unwrap();
        let recorded = read_when_written(&tmp.path().join("args.txt"));
        assert_eq!(recorded.trim_end(), "/P /UPDATE /R /ARGS --autostart");
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
            .launch(&appimage_launch(&fake, &args(&["--autostart", "a b"])))
            .unwrap();
        let recorded = read_when_written(&tmp.path().join("args.txt"));
        let dir = tmp.path().canonicalize().unwrap();
        assert_eq!(recorded, format!("{}|--autostart|a b|\n", dir.display()));
    }
}

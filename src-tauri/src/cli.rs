//! The command line (CMP Contracts, Command line).

/// What a start of the binary does, picked from its arguments (CMP-R1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Tray mode. `autostart` is true for `--autostart`, which keeps the
    /// window closed unless it is a first run (CMP-R3, CMP-R100).
    Tray { autostart: bool },
    /// `--headless`: Linux only (CMP-R1).
    Headless,
    /// `--remove`: removal in a process of its own (CMP-R70).
    Remove,
    /// `--quit`: makes a running tray-mode process quit (CMP-R2).
    Quit,
}

/// The argument removal starts a child with, to make a running tray-mode
/// process quit (CMP-R70).
pub const QUIT: &str = "--quit";

/// Picks the mode. `--remove` wins over every other argument, then `--quit`,
/// then `--headless`, then `--autostart`; unknown arguments are ignored.
pub fn parse<S: AsRef<str>>(args: &[S]) -> Mode {
    let has = |flag: &str| args.iter().any(|a| a.as_ref() == flag);
    if has("--remove") {
        Mode::Remove
    } else if has(QUIT) {
        Mode::Quit
    } else if has("--headless") {
        Mode::Headless
    } else {
        Mode::Tray {
            autostart: has("--autostart"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_argument_is_tray_mode_with_the_window_open() {
        // CMP Contracts, Command line: none
        assert_eq!(parse::<&str>(&[]), Mode::Tray { autostart: false });
    }

    #[test]
    fn autostart_is_tray_mode_with_the_window_closed() {
        // CMP Contracts, Command line: --autostart (CMP-R3)
        assert_eq!(parse(&["--autostart"]), Mode::Tray { autostart: true });
    }

    #[test]
    fn each_flag_picks_its_mode() {
        // CMP-R1: one binary with three modes
        assert_eq!(parse(&["--headless"]), Mode::Headless);
        assert_eq!(parse(&["--remove"]), Mode::Remove);
        assert_eq!(parse(&["--quit"]), Mode::Quit);
    }

    #[test]
    fn remove_wins_then_quit_then_headless_then_autostart() {
        // CMP Contracts, Command line: precedence
        assert_eq!(
            parse(&["--autostart", "--headless", "--quit", "--remove"]),
            Mode::Remove
        );
        assert_eq!(parse(&["--autostart", "--headless", "--quit"]), Mode::Quit);
        assert_eq!(parse(&["--autostart", "--headless"]), Mode::Headless);
    }

    #[test]
    fn unknown_arguments_are_ignored() {
        // CMP Contracts, Command line: "Unknown arguments are ignored."
        assert_eq!(
            parse(&["--verbose", "file.txt"]),
            Mode::Tray { autostart: false }
        );
        assert_eq!(
            parse(&["--nope", "--autostart"]),
            Mode::Tray { autostart: true }
        );
    }
}

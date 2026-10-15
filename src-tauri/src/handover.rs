//! The handover between a tray-mode process and the Game Mode unit
//! (CMP-R92), so that only one of them watches (CMP-R2). Tray mode calls
//! both steps; "Keep uploading in Game Mode with a systemd user unit" (#16)
//! fills them in, while the toggle is on (CMP-R90). Until then there is no
//! toggle and no unit, nothing runs in Game Mode (CMP-R93), and both steps do
//! nothing.
//!
//! What #16 still has to build around them:
//! - `systemctl --user stop pagle-companion.service` in
//!   [`before_taking_lock`]. A headless process stopped this way releases the
//!   lock and exits 0 (`headless`), and tray mode's lock wait (CMP-R2)
//!   follows.
//! - `systemctl --user start pagle-companion.service` in
//!   [`after_releasing_lock`], except when quitting for `--quit` or removal
//!   (CMP-R70). Tray mode quits for `--quit` and for **Quit** alike today
//!   (`app.exit(0)`), so #16 tells them apart. (`--remove` sends that
//!   `--quit`; removal from the Linux tray item exits with
//!   `std::process::exit` and never reaches this step: `removal::window`.)
//! - Tray mode exiting on `SIGTERM` or `SIGHUP` (logout, the switch to Game
//!   Mode) through the same exit path. Headless mode's own signal handling is
//!   in `headless`.

use crate::companion::Companion;

/// CMP-R92: a tray-mode start stops the Game Mode unit before it takes the
/// watcher lock.
pub fn before_taking_lock(_companion: &Companion) {}

/// CMP-R92: a tray-mode process that exits, once it has released the
/// watcher lock, starts the Game Mode unit.
pub fn after_releasing_lock(_companion: &Companion) {}

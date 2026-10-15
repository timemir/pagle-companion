//! Pagle Companion: the desktop app that runs beside the game and carries
//! Pagle data between the game's files and the backend. Its spec is
//! `docs/spec/companion/` in `timemir/pagle-backend` (CMP and DST rules).

pub mod api;
pub mod cli;
pub mod credential_store;
pub mod device_flow;
pub mod installs;
pub mod link;
pub mod linking;
pub mod local_log;
pub mod lua;
pub mod notify;
pub mod os_notify;
pub mod pagle_log;
pub mod readings;
pub mod state;
pub mod state_dir;
#[cfg(test)]
mod stub_server;
pub mod tray;
pub mod upload_doc;

use cli::Mode;

/// Exit status of `--headless` on Windows, where headless mode doesn't exist
/// (CMP Contracts, Command line).
const HEADLESS_ON_WINDOWS: i32 = 2;

pub fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args) {
        // Removal (CMP-R70) comes with Track C.
        Mode::Remove => std::process::exit(0),
        Mode::Headless if cfg!(windows) => std::process::exit(HEADLESS_ON_WINDOWS),
        // Headless mode (CMP-R1) comes with Track C.
        Mode::Headless => std::process::exit(0),
        mode => tray::run(mode),
    }
}

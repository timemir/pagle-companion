//! Pagle Companion: the desktop app that runs beside the game and carries
//! Pagle data between the game's files and the backend. Its spec is
//! `docs/spec/companion/` in `timemir/pagle-backend` (CMP and DST rules).

pub mod api;
pub mod appimage;
pub mod autostart;
pub mod catch_logs;
pub mod cli;
pub mod companion;
pub mod credential_store;
pub mod device_flow;
pub mod first_run;
pub mod game_mode;
pub mod handover;
pub mod headless;
pub mod host;
#[cfg(test)]
mod hostile_input;
pub mod in_game;
pub mod installs;
pub mod integrity;
pub mod link;
pub mod linking;
pub mod local_log;
pub mod lua;
pub mod notify;
pub mod os_notify;
pub mod pagle_log;
pub mod readings;
pub mod removal;
pub mod state;
pub mod state_dir;
#[cfg(test)]
mod stub_server;
pub mod systemctl;
#[cfg(test)]
mod tauri_conf;
pub mod tray;
pub mod update_policy;
pub mod update_required;
pub mod updater;
pub mod upload;
pub mod upload_doc;
pub mod xdg;

use cli::Mode;

pub fn run() {
    // DST-R33: first, before any thread starts and before the webview.
    appimage::webkit_workaround();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args) {
        // In a process of its own, without Tauri; always status 0 (CMP-R70).
        Mode::Remove => std::process::exit(removal::run()),
        // Exits with status 2 on Windows (CMP Contracts, Command line).
        Mode::Headless => std::process::exit(headless::run()),
        mode => tray::run(mode),
    }
}

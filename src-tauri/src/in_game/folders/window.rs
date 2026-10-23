//! The data addon folders in the app: the [`Writer`] run over the scanner's
//! installs, the start sweep and TOC refresh, the delete on Unlink and on
//! losing the link (CMP-R85), and the write failures in the window's WoW
//! folders section (CMP-R83, CMP UI), which `ui/folders.js` shows.
//!
//! The in-game download timer (#33) calls [`write`], [`refresh_tocs`],
//! [`qualifying`] and [`data_files_hash_to`].

use serde::Serialize;

use super::{DataFile, Qualifying, WriteFailure, WriteReport, Writer};
use crate::companion::Companion;
use crate::host::Host;
use crate::installs::Scanner;

/// The event that carries a new [`View`] to the window.
pub const EVENT: &str = "in-game-folders";

/// What the WoW folders section shows of the data addon folders.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub failures: Vec<WriteFailure>,
}

/// The scanner's installs, as of its last scan.
fn installs<H: Host>(app: &H) -> Vec<String> {
    app.state::<Scanner>()
        .installs()
        .into_iter()
        .map(|install| install.flavor)
        .collect()
}

/// Every install the Companion knows: those the scan found, and those local
/// state names (CMP-R85, as CMP-R70 has it).
fn known_installs<H: Host>(app: &H) -> Vec<String> {
    let manual = app
        .try_state::<Companion>()
        .map(|companion| companion.state.read(|s| s.manual_installs.clone()))
        .unwrap_or_default();
    super::known_installs(installs(app), manual)
}

fn show<H: Host>(app: &H) {
    let failures = app.state::<Writer>().failures();
    app.emit(EVENT, View { failures });
}

/// At start, for this process, which holds the watcher lock (CMP-R2): once
/// the first scan has ended, deletes leftover `.tmp` files and refreshes the
/// TOCs (CMP-R83).
pub fn start<H: Host>(app: &H) {
    let first_scan = app.state::<Scanner>().subscribe();
    let app = app.clone();
    std::thread::spawn(move || {
        let Ok(snapshot) = first_scan.recv() else {
            return;
        };
        let flavors: Vec<String> = snapshot.installs.into_iter().map(|i| i.flavor).collect();
        app.state::<Writer>().start(&flavors);
        show(&app);
    });
}

/// Writes a body that passed CMP-R82 as `file`'s data file into every
/// install that has the Pagle addon (CMP-R80, CMP-R83). The download is
/// successful when [`WriteReport::succeeded`].
pub fn write<H: Host>(app: &H, file: DataFile, body: &[u8]) -> WriteReport {
    let report = app.state::<Writer>().write(&installs(app), file, body);
    show(app);
    report
}

/// The TOC refresh the timer makes at start and every in-game check
/// (CMP-R83).
pub fn refresh_tocs<H: Host>(app: &H) {
    app.state::<Writer>().refresh_tocs(&installs(app));
    show(app);
}

/// Which installs qualify, and which lack each data file (CMP-R80).
pub fn qualifying<H: Host>(app: &H) -> Qualifying {
    app.state::<Writer>().qualifying(&installs(app))
}

/// Whether the Anglers download may send `If-None-Match` with the kept
/// `etag`: `file` in every qualifying install hashes to it (CMP-R81).
pub fn data_files_hash_to<H: Host>(app: &H, file: DataFile, etag: &str) -> bool {
    app.state::<Writer>()
        .data_files_hash_to(&installs(app), file, etag)
}

/// Deletes the three folders in every install the Companion knows (CMP-R85): the
/// link's hook on Unlink and on losing the link.
pub fn delete_folders<H: Host>(app: &H) {
    let Some(writer) = app.try_state::<Writer>() else {
        return;
    };
    writer.delete_folders(&known_installs(app));
    show(app);
}

#[tauri::command]
pub fn in_game_folders(writer: tauri::State<'_, Writer>) -> View {
    View {
        failures: writer.failures(),
    }
}

//! What the window's WoW folders section shows of the reads: each account
//! folder's read problem (CMP-R30, CMP-R31) and "Checked every {poll
//! interval}" where it is polled (CMP-R22). The section is `ui/folders.js`.
//! Also keeps `characters` and `lastFolder` of the `logId`s local state
//! already holds as last read (CMP-R41).

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use super::{Readings, Snapshot, Update};
use crate::installs::{POLL_INTERVAL, Scanner};
use crate::state::State;
use crate::tray::Companion;

/// The event that carries a new [`View`] to the window.
pub const EVENT: &str = "readings";

/// What the WoW folders section shows of the reads.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    /// Every watched account folder; empty until the first scan's reads end.
    pub folders: Vec<FolderView>,
    /// The poll interval, for "Checked every {poll interval}" (CMP-R22).
    pub poll_seconds: u64,
}

/// One account folder, matched to the scanner's by `path`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderView {
    pub path: String,
    /// Checked every poll interval instead of watched (CMP-R22).
    pub polled: bool,
    /// CMP-R30's or CMP-R31's text, if any.
    pub problem: Option<&'static str>,
}

impl View {
    pub fn of(snapshot: Option<Snapshot>) -> Self {
        Self {
            folders: snapshot
                .map(|s| {
                    s.folders
                        .iter()
                        .map(|f| FolderView {
                            path: f.path.clone(),
                            polled: f.polled,
                            problem: f.problem(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            poll_seconds: POLL_INTERVAL.as_secs(),
        }
    }
}

/// The `characters` and `lastFolder` that the folders read anew in `update`
/// give the `logId`s local state already holds, where they differ (CMP-R41,
/// CMP Data): `(logId, characters, lastFolder)`. A `logId` local state
/// doesn't hold yet is confirm-once's to add (CMP-R40); an empty log isn't
/// counted (CMP-R32).
pub fn remembered(state: &State, update: &Update) -> Vec<(String, String, String)> {
    let mut changes = Vec::new();
    for path in &update.read {
        let Some(folder) = update.snapshot.folder(path) else {
            continue;
        };
        let Some(read) = folder.catch_log() else {
            continue;
        };
        let Some(kept) = state.logs.get(&read.log.log_id) else {
            continue;
        };
        let characters = read.log.characters_display();
        if kept.characters != characters || kept.last_folder != folder.path {
            changes.push((read.log.log_id.clone(), characters, folder.path.clone()));
        }
    }
    changes
}

/// Starts reading for this process, which holds the watcher lock (CMP-R2),
/// at every scan of the scanner, and keeps the window's section and local
/// state up to date.
pub fn start(app: &AppHandle) {
    let readings = app.state::<Readings>();
    let updates = readings.subscribe();
    readings.start(app.state::<Scanner>().subscribe_scans());
    let app = app.clone();
    std::thread::spawn(move || {
        for update in updates {
            let companion = app.state::<Companion>();
            let changes = companion.state.read(|s| remembered(s, &update));
            if !changes.is_empty() {
                let written = companion.state.update(|s| {
                    for (log_id, characters, folder) in changes {
                        if let Some(log) = s.logs.get_mut(&log_id) {
                            log.characters = characters;
                            log.last_folder = folder;
                        }
                    }
                });
                if let Err(e) = written {
                    log::error!("couldn't write state.json: {e}");
                }
            }
            let _ = app.emit(EVENT, View::of(Some(update.snapshot)));
        }
    });
}

#[tauri::command]
pub fn readings(readings: tauri::State<'_, Readings>) -> View {
    View::of(readings.snapshot())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::fixture::*;
    use super::super::{FileStamp, FolderReading, LocalRead, Reading};
    use super::*;
    use crate::pagle_log::{self, Found};
    use crate::state::LogState;

    fn folder(path: &str, log_id: &str, entries: u64) -> FolderReading {
        let Ok(Found::Valid(log)) = pagle_log::parse(pagle_lua_text(log_id, entries).as_bytes())
        else {
            panic!("fixture not valid");
        };
        let file = FileStamp {
            mtime_ms: 1_791_735_090_999,
            size: 1,
        };
        FolderReading {
            flavor: "G:/WoW/_classic_beta_".into(),
            name: path.rsplit('/').next().unwrap().into(),
            path: path.into(),
            polled: false,
            reading: Reading::Valid(LocalRead {
                log: Arc::new(log),
                as_of: file.as_of(),
                file,
            }),
            last_log_id: Some(log_id.into()),
        }
    }

    fn kept(characters: &str, last_folder: &str) -> LogState {
        LogState {
            answer: None,
            folder: None,
            characters: characters.into(),
            last_folder: last_folder.into(),
            whole_send_pending: false,
            last_run: None,
            last_uploaded_at: None,
        }
    }

    #[test]
    fn a_read_keeps_characters_and_last_folder_of_a_known_log_id() {
        // CMP-R41: its characters as last read, kept in local state; CMP
        // Data: lastFolder, the account folder the logId was last read from
        let one = "G:/WoW/_classic_beta_/WTF/Account/ADA#1";
        let two = "G:/WoW/_classic_beta_/WTF/Account/ADA#2";
        let mut state = State::default();
        state.logs.insert(LOG_A.into(), kept("Ada", one));
        let update = Update {
            snapshot: Snapshot {
                folders: vec![folder(one, LOG_A, 1), folder(two, LOG_A, 1)],
            },
            read: vec![two.into()],
            scan: false,
        };
        assert_eq!(
            remembered(&state, &update),
            [(LOG_A.to_owned(), "Ada Brook".to_owned(), two.to_owned())]
        );

        state.logs.insert(LOG_A.into(), kept("Ada Brook", two));
        assert!(remembered(&state, &update).is_empty());
    }

    #[test]
    fn an_unknown_or_empty_log_changes_nothing_in_local_state() {
        // CMP-R40: a new logId is confirm-once's to add; CMP-R32: an empty
        // log isn't counted
        let one = "G:/WoW/_classic_beta_/WTF/Account/ADA#1";
        let mut state = State::default();
        let update = Update {
            snapshot: Snapshot {
                folders: vec![folder(one, LOG_A, 0)],
            },
            read: vec![one.into()],
            scan: false,
        };
        assert!(remembered(&state, &update).is_empty());
        state.logs.insert(LOG_A.into(), kept("Bo", "elsewhere"));
        assert!(remembered(&state, &update).is_empty());
    }

    #[test]
    fn the_view_shows_problems_and_the_poll_interval() {
        // CMP-R22: "Checked every {poll interval}"; CMP-R30, CMP-R31: the
        // folder's read problem
        let mut unreadable = folder("G:/A/ADA#1", LOG_A, 1);
        unreadable.reading = Reading::Unreadable;
        unreadable.polled = true;
        let view = View::of(Some(Snapshot {
            folders: vec![unreadable, folder("G:/A/ADA#2", LOG_A, 1)],
        }));
        assert_eq!(view.poll_seconds, 30);
        assert!(view.folders[0].polled);
        assert_eq!(view.folders[0].problem, Some(super::super::COULDNT_READ));
        assert!(!view.folders[1].polled);
        assert_eq!(view.folders[1].problem, None);
        assert!(View::of(None).folders.is_empty());
    }
}

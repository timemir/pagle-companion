//! What the window's WoW folders section shows of the reads: each account
//! folder's read problem (CMP-R30, CMP-R31) and "Checked every {poll
//! interval}" where it is polled (CMP-R22). The section is `ui/folders.js`.
//! What the reads mean for each `logId` (CMP-R40 to CMP-R42) is
//! `catch_logs`'.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use super::{Readings, Snapshot};
use crate::installs::{POLL_INTERVAL, Scanner};

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

/// Starts reading for this process, which holds the watcher lock (CMP-R2),
/// at every scan of the scanner, and keeps the window's section up to date.
pub fn start(app: &AppHandle) {
    let readings = app.state::<Readings>();
    let updates = readings.subscribe();
    readings.start(app.state::<Scanner>().subscribe_scans());
    let app = app.clone();
    std::thread::spawn(move || {
        for update in updates {
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

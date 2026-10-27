//! Choosing what to upload (CMP-R40 to CMP-R42): asking once per `logId`,
//! remembering the answer, and settling a `logId` found in several account
//! folders on the one the Player picks.
//!
//! The decisions are pure functions of local state and the readings'
//! [`Snapshot`]: [`remember`] adds the `logId`s seen for the first time and
//! keeps each one's pick, characters and folder; [`decide`] tells, per
//! `logId`, the folder it uploads from, whether it needs a pick, and what its
//! row shows; [`confirmed`] is what upload runs work on (CMP-R45); [`causes`]
//! are CMP-R111's first two notifications; [`view`] is the window's Catch
//! logs list. [`CatchLogs`] runs them after every reading and every answer,
//! raises and clears the notifications, and tells upload runs (#28) when the
//! confirmed set changes. The window part is in [`window`].

pub mod window;

use std::collections::{BTreeMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, MutexGuard};

use serde::Serialize;

use crate::installs::paths;
use crate::notify::{Cause, CauseKey, Notifications, Notifier};
use crate::readings::{FolderReading, Reading, Snapshot};
use crate::state::{Answer, CatchLogState, LastRun, LogState, RunResult, State, Store};
use crate::upload::plan::relative_time;

/// What a row says of a `logId` no watched file holds (CMP-R41).
pub const NOT_FOUND: &str = "Not found on this computer";

/// What the window says after the **Upload** switch is turned off, with a
/// link to My Anglers (CMP-R41).
pub const SWITCHED_OFF: &str =
    "Pagle keeps what it already has. To remove it, delete it on the Pagle website.";

/// My Anglers on the Pagle website, where a Catch log is deleted (CMP-R41).
pub fn my_anglers_url(base_url: &str) -> String {
    format!("{}/me/anglers", base_url.trim_end_matches('/'))
}

/// Sign-in on the Pagle website, for held characters (CMP-R53).
pub fn sign_in_url(base_url: &str) -> String {
    format!("{}/signin", base_url.trim_end_matches('/'))
}

/// The confirmed `logId`s, each with the account folder it uploads from:
/// answered **Upload**, with its folder settled by CMP-R42 (CMP-R45).
pub type Confirmed = BTreeMap<String, String>;

/// CMP-R40's question as the awaiting row's state line says it (CMP UI,
/// Catch logs): the row already shows the characters and account folder.
pub const UPLOAD_IT: &str = "Upload it?";

/// CMP-R42's question, `folders` being the folder labels.
pub fn pick_question(folders: &[String]) -> String {
    format!(
        "The same Catch log is in {} account folders: {}. It was probably copied. Pagle \
         uploads it from one folder only. Pick the one you play on.",
        folders.len(),
        folders.join(", ")
    )
}

fn same_folder(a: &str, b: &str) -> bool {
    paths::same_folder_key(a) == paths::same_folder_key(b)
}

/// The last component of a canonical path.
fn last_component(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A canonical path without its last component.
fn parent(path: &str) -> &str {
    path.rfind('/').map_or(path, |i| &path[..i])
}

/// `{folder}` of CMP-R40: the account folder's name, followed by
/// " ({flavor folder name})" when more than one install is watched, or by
/// " ({install root path}/{flavor folder name})" when its install shares its
/// flavor folder name with another watched one. `installs` are the watched
/// installs' flavor folders.
pub fn folder_label(folder: &str, installs: &[String]) -> String {
    let name = last_component(folder);
    if installs.len() <= 1 {
        return name.to_owned();
    }
    // An account folder is `<flavor folder>/WTF/Account/<name>` (CMP-R15).
    let flavor = parent(parent(parent(folder)));
    let flavor_name = last_component(flavor);
    let sharing = installs
        .iter()
        .filter(|install| last_component(install) == flavor_name)
        .count();
    if sharing > 1 {
        format!("{name} ({flavor})")
    } else {
        format!("{name} ({flavor_name})")
    }
}

/// What the account folders' last reads say about one `logId`.
#[derive(Debug, Clone, Default)]
pub struct Copies<'a> {
    /// Folders whose valid local log with entries holds it (CMP-R31,
    /// CMP-R32): the copies CMP-R42 counts, in snapshot order.
    pub holders: Vec<&'a FolderReading>,
    /// Folders whose last read found it invalid (CMP-R31), or failed after
    /// they last held it (CMP-R30). They may still hold it: a pick on one of
    /// them stays, and their problem shows on the row.
    pub unsure: Vec<&'a FolderReading>,
}

impl<'a> Copies<'a> {
    pub fn of(snapshot: &'a Snapshot, log_id: &str) -> Self {
        let mut copies = Self::default();
        for folder in &snapshot.folders {
            if folder
                .catch_log()
                .is_some_and(|read| read.log.log_id == log_id)
            {
                copies.holders.push(folder);
            } else if matches!(
                folder.reading,
                Reading::Invalid { .. } | Reading::Unreadable
            ) && folder.last_log_id.as_deref() == Some(log_id)
            {
                copies.unsure.push(folder);
            }
        }
        copies
    }

    fn all(&self) -> impl Iterator<Item = &&'a FolderReading> {
        self.holders.iter().chain(&self.unsure)
    }

    /// The folder at `path`, if it holds the `logId` or may.
    fn get(&self, path: &str) -> Option<&'a FolderReading> {
        self.all().find(|f| same_folder(&f.path, path)).copied()
    }

    fn count(&self) -> usize {
        self.holders.len() + self.unsure.len()
    }
}

fn new_log(folder: &FolderReading) -> Option<LogState> {
    Some(LogState {
        answer: None,
        folder: None,
        characters: folder.catch_log()?.log.characters_display(),
        last_folder: folder.path.clone(),
        uploaded_snapshot: None,
        observed_snapshot: None,
        recovery: None,
        recovery_notice: None,
        last_run: None,
        last_uploaded_at: None,
    })
}

/// [`remember`]s the folders' reads in local state: adds every `logId` seen
/// for the first time in a valid local log with entries, awaiting (CMP-R40);
/// drops a pick its folder no longer backs or that leaves one folder only
/// (CMP-R42, CMP Data `folder`); and keeps `characters` and `lastFolder` as
/// last read from the folder the `logId` uploads from (CMP-R41). Returns
/// whether anything changed.
pub fn remember(state: &mut State, snapshot: &Snapshot) -> bool {
    let mut changed = false;
    for (log_id, holders) in snapshot.catch_logs() {
        if !state.logs.contains_key(&log_id)
            && let Some(log) = holders.first().and_then(|f| new_log(f))
        {
            state.logs.insert(log_id, log);
            changed = true;
        }
    }
    for (log_id, log) in &mut state.logs {
        let copies = Copies::of(snapshot, log_id);
        if let Some(pick) = &log.folder {
            let drop = if copies.get(pick).is_some() {
                // The pick leaves only one folder (CMP Data).
                copies.count() == 1
            } else {
                // The picked folder no longer holds it: one other becomes
                // the pick, several ask again (CMP-R42). With none, the pick
                // stays for when it comes back.
                !copies.holders.is_empty()
            };
            if drop {
                log.folder = None;
                changed = true;
            }
        }
        // As last read from the folder it uploads from, or while unpicked
        // from the first that holds it (CMP-R41).
        let source = match &log.folder {
            Some(pick) => copies.holders.iter().find(|f| same_folder(&f.path, pick)),
            None => copies.holders.first(),
        };
        if let Some(folder) = source
            && let Some(read) = folder.catch_log()
        {
            let characters = read.log.characters_display();
            if log.characters != characters || log.last_folder != folder.path {
                log.characters = characters;
                log.last_folder.clone_from(&folder.path);
                changed = true;
            }
        }
    }
    changed
}

/// What one `logId` is, given its [`Copies`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The account folder it uploads from once answered **Upload**: the pick,
    /// or the one folder that holds it (CMP-R42).
    pub folder: Option<String>,
    /// Several folders hold it and none is picked (CMP-R42).
    pub needs_pick: bool,
    /// The folders a pick chooses from (CMP-R42), in snapshot order.
    pub choices: Vec<String>,
    /// No watched file holds it, or may (CMP-R41).
    pub not_found: bool,
    /// CMP-R30's or CMP-R31's text, from the folder it uploads from, or
    /// while unsettled from the first folder that may hold it.
    pub problem: Option<&'static str>,
}

/// Decides one `logId` known to local state, after [`remember`].
pub fn decide(log: &LogState, copies: &Copies) -> Decision {
    let picked = log.folder.as_deref().and_then(|pick| copies.get(pick));
    let (folder, needs_pick) = match (picked, copies.holders.as_slice(), copies.unsure.as_slice()) {
        (Some(pick), _, _) => (Some(pick), false),
        (None, [one], _) | (None, [], [one]) => (Some(*one), false),
        (None, [], _) => (None, false),
        (None, _, _) => (None, true),
    };
    let problem = match folder {
        Some(folder) => folder.problem(),
        None => copies.unsure.first().and_then(|f| f.problem()),
    };
    Decision {
        folder: folder.map(|f| f.path.clone()),
        needs_pick,
        choices: copies.holders.iter().map(|f| f.path.clone()).collect(),
        not_found: copies.count() == 0,
        problem,
    }
}

/// The confirmed `logId`s (CMP-R45), after [`remember`]. Awaiting and
/// **Don't upload** logs are never in it (CMP-R40), nor a `logId` whose
/// folder isn't settled (CMP-R42).
pub fn confirmed(state: &State, snapshot: &Snapshot) -> Confirmed {
    state
        .logs
        .iter()
        .filter(|(_, log)| log.answer == Some(Answer::Upload))
        .filter_map(|(log_id, log)| {
            let folder = decide(log, &Copies::of(snapshot, log_id)).folder?;
            Some((log_id.clone(), folder))
        })
        .collect()
}

/// Whether a log awaits an answer it can be asked now: unanswered, with its
/// folder settled (CMP-R40).
fn awaiting(log: &LogState, decision: &Decision) -> bool {
    log.answer.is_none() && decision.folder.is_some()
}

/// Whether a log shows CMP-R42's question: several folders, none picked, and
/// not answered **Don't upload**, which needs no pick (CMP-R40).
fn asks_pick(log: &LogState, decision: &Decision) -> bool {
    decision.needs_pick && log.answer != Some(Answer::Skip)
}

/// Every `logId`'s two causes of CMP-R111, after [`remember`]: `Some` when
/// present, `None` when it has cleared.
pub fn causes(state: &State, snapshot: &Snapshot) -> Vec<(CauseKey, Option<Cause>)> {
    let mut causes = Vec::new();
    for (log_id, log) in &state.logs {
        let decision = decide(log, &Copies::of(snapshot, log_id));
        causes.push((
            CauseKey::AwaitingAnswer(log_id.clone()),
            awaiting(log, &decision).then(|| Cause::AwaitingAnswer {
                log_id: log_id.clone(),
                characters: log.characters.clone(),
            }),
        ));
        causes.push((
            CauseKey::SeveralFolders(log_id.clone()),
            asks_pick(log, &decision).then(|| Cause::SeveralFolders {
                log_id: log_id.clone(),
                folders: decision.choices.len(),
            }),
        ));
    }
    causes
}

/// One of the folders a pick chooses from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Choice {
    /// What [`CatchLogs::pick`] takes.
    pub path: String,
    pub label: String,
}

/// A row's state line, the first that applies (CMP UI, Catch logs): the
/// questions, not found, a read problem, then the last run's result from
/// `lastRun` ([`run_line`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Line {
    /// CMP-R40's question, "Upload it?", with **Upload** and **Don't upload**.
    Awaiting {
        text: &'static str,
    },
    /// CMP-R42's question, with one button per choice; and, while awaiting,
    /// **Don't upload**, which needs no pick (CMP-R40).
    SeveralFolders {
        text: String,
        dont_upload: bool,
    },
    /// CMP-R41's "Not found on this computer".
    NotFound {
        text: &'static str,
    },
    /// CMP-R30's or CMP-R31's text.
    Problem {
        text: &'static str,
    },
    /// The last run ended **uploaded**: "Uploaded {relative time}", then
    /// CMP-R53's lines.
    Uploaded {
        text: String,
        notes: Vec<Note>,
    },
    /// The last run ended **up to date**: "Up to date", then CMP-R53's lines
    /// from the last `catchLog` kept.
    UpToDate {
        text: &'static str,
        notes: Vec<Note>,
    },
    /// Recovery, its preparation result, and instructions (CMP-R52).
    Divergence {
        text: String,
        question: &'static str,
    },
    Stale {
        text: &'static str,
    },
    /// CMP-R54's text, with a link to My Anglers.
    Deleted {
        text: &'static str,
    },
    DevLog {
        text: &'static str,
    },
    WaitingForPagle {
        text: &'static str,
    },
    /// The `log_cap` message, with a link to My Anglers.
    LogCap {
        text: String,
    },
    TooLarge {
        text: &'static str,
    },
    /// Failed, with why and when it tries again.
    Failed {
        text: &'static str,
    },
}

impl Line {
    /// Whether the line asks something of the Player (CMP UI, Catch logs):
    /// CMP-R40's and CMP-R42's questions, a divergence, deleted, log cap, and
    /// held characters under **uploaded** or **up to date**. The window's
    /// `act` tone (`ui/catch-logs.js`) is the same set; keep them alike.
    pub fn needs_player(&self) -> bool {
        match self {
            Line::Awaiting { .. }
            | Line::SeveralFolders { .. }
            | Line::Divergence { .. }
            | Line::Deleted { .. }
            | Line::LogCap { .. } => true,
            Line::Uploaded { notes, .. } | Line::UpToDate { notes, .. } => notes
                .iter()
                .any(|note| matches!(note, Note::HeldCharacters { .. })),
            Line::NotFound { .. }
            | Line::Problem { .. }
            | Line::Stale { .. }
            | Line::DevLog { .. }
            | Line::WaitingForPagle { .. }
            | Line::TooLarge { .. }
            | Line::Failed { .. } => false,
        }
    }
}

/// One of CMP-R53's lines under **uploaded** and **up to date**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Note {
    /// The Pending reason's text (accounts.md UI), with no notification.
    Pending { text: &'static str },
    /// Held characters (ACC-R33), with a **Sign in** link to `/signin`.
    HeldCharacters { text: &'static str },
    /// Left off Leaderboards and First reported, with a link to My Anglers.
    LeftOff { text: &'static str },
}

/// What a row says after a run that ended up to date (CMP UI, Catch logs).
pub const UP_TO_DATE: &str = "Up to date";
/// CMP-R52's row text, also its notification (CMP-R111).
pub const DIVERGENCE: &str = "This Catch log differs from the copy on Pagle.";
/// The Recovery action explains preservation and game folder discovery.
pub const RECOVERY_EXPLANATION: &str = "Recovery restores Accepted history, preserves your complete original branch, and keeps records needing review available. Return to character selection for a new Pagle_Recovery folder; use /reload for an existing folder. Upload again after saving to check consistency.";
pub const STALE: &str = "Pagle already has a newer copy of this Catch log. Nothing changed.";
/// CMP-R54.
pub const DELETED: &str =
    "You deleted this Catch log on the Pagle website. Allow it again there to upload it.";
pub const DEV_LOG: &str =
    "This Catch log is from a Pagle development version and can't be uploaded.";
pub const WAITING_FOR_PAGLE: &str = "The Pagle website can't read this version of the addon \
                                     yet. Pagle Companion tries again later.";
/// The `log_cap` message when the refusal carries none (UPL Refusals).
pub const LOG_CAP: &str = "You already have 20 Catch logs on Pagle. Delete one to add another.";
pub const TOO_LARGE: &str = "This Catch log has an entry too large to upload.";
pub const FAILED: &str = "Upload failed. Pagle Companion tries again when the file changes.";
/// Failed after a second `gap`, not held back (CMP-R50).
pub const FAILED_GAP: &str = "Upload failed. Pagle Companion tries again later.";
/// Failed on `unsupported_document` (CMP-R50).
pub const UPDATE_COMPANION: &str = "Update Pagle Companion.";
/// CMP-R53, ACC-R33; also its notification (CMP-R111).
pub const HELD_CHARACTERS: &str = "A new character needs verifying. Sign in again to verify it.";
/// CMP-R53, for `flagged` and `excluded` alike.
pub const CHECKING: &str = "Competition is waiting for checks of your latest Upload. Your last checked Angler page stays available.";
pub const LEFT_OFF: &str =
    "This Angler is left off Leaderboards and First reported. Your Angler page says why.";

/// The text of a Pending reason (accounts.md UI, Pending reasons); `None`
/// for one this Companion doesn't know.
pub fn pending_reason_text(reason: &str) -> Option<&'static str> {
    Some(match reason {
        "forever-api" => {
            "Battle.net doesn't list WoW Forever characters yet. Your Catch log is safe here and \
             will be verified once it does."
        }
        "sign-in-needed" => "Sign in again so Pagle can verify this Catch log.",
        "no-wow-profile" => {
            "Pagle couldn't see your WoW characters. Sign in again, leave 'World of Warcraft \
             profile' ticked, and check that your Battle.net privacy settings allow third-party \
             sites."
        }
        "mixed-accounts" => {
            "This Catch log holds characters from more than one WoW account; it was probably \
             copied from another PC. If you just created a character, log out of it in game and \
             sign in again in a few minutes."
        }
        "not-listed" => "None of this Catch log's characters are in your Battle.net account.",
        _ => return None,
    })
}

/// Whether a `catchLog` has held characters (CMP-R53: `verified` with
/// `heldCharacters > 0`), which also raises its notification (CMP-R111).
pub fn has_held_characters(catch_log: &CatchLogState) -> bool {
    catch_log.verification == "verified" && catch_log.held_characters > 0
}

/// CMP-R53's lines for a `catchLog`: the Pending reason, held characters,
/// left off, all that apply, in that order (CMP UI, Catch logs).
pub fn catch_log_notes(catch_log: &CatchLogState) -> Vec<Note> {
    let mut notes = Vec::new();
    if catch_log.verification == "pending"
        && let Some(text) = catch_log
            .pending_reason
            .as_deref()
            .and_then(pending_reason_text)
    {
        notes.push(Note::Pending { text });
    }
    if has_held_characters(catch_log) {
        notes.push(Note::HeldCharacters {
            text: HELD_CHARACTERS,
        });
    }
    if catch_log
        .left_off
        .as_ref()
        .is_some_and(|left_off| !left_off.is_null())
    {
        let text = if catch_log
            .left_off
            .as_ref()
            .and_then(serde_json::Value::as_str)
            == Some("checking")
        {
            CHECKING
        } else {
            LEFT_OFF
        };
        notes.push(Note::LeftOff { text });
    }
    notes
}

/// The `log_cap` message of a run that ended **log cap** (CMP UI, Catch
/// logs; CMP-R111).
pub fn log_cap_text(run: &LastRun) -> String {
    run.message.clone().unwrap_or_else(|| LOG_CAP.into())
}

/// The last run's line (CMP UI, Catch logs), at `now` in Unix seconds.
pub fn run_line(log: &LogState, now: i64) -> Option<Line> {
    if let Some(notice) = &log.recovery_notice {
        return Some(Line::Divergence {
            text: notice.clone(),
            question: RECOVERY_EXPLANATION,
        });
    }
    if let Some(pending) = &log.recovery {
        return Some(Line::Divergence {
            text: pending.instructions.clone(),
            question: RECOVERY_EXPLANATION,
        });
    }
    let run = log.last_run.as_ref()?;
    let notes = || {
        run.catch_log
            .as_ref()
            .map(catch_log_notes)
            .unwrap_or_default()
    };
    Some(match run.result {
        RunResult::Uploaded => Line::Uploaded {
            text: format!("Uploaded {}", relative_time(now, run.at)),
            notes: notes(),
        },
        RunResult::UpToDate => Line::UpToDate {
            text: UP_TO_DATE,
            notes: notes(),
        },
        RunResult::Divergence => Line::Divergence {
            text: run.message.clone().unwrap_or_else(|| DIVERGENCE.into()),
            question: RECOVERY_EXPLANATION,
        },
        RunResult::Stale => Line::Stale { text: STALE },
        RunResult::Deleted => Line::Deleted { text: DELETED },
        RunResult::DevLog => Line::DevLog { text: DEV_LOG },
        RunResult::WaitingForPagle => Line::WaitingForPagle {
            text: WAITING_FOR_PAGLE,
        },
        RunResult::LogCap => Line::LogCap {
            text: log_cap_text(run),
        },
        RunResult::TooLarge => Line::TooLarge { text: TOO_LARGE },
        RunResult::Failed => Line::Failed {
            text: match run.code.as_deref() {
                Some(crate::upload::GAP) => FAILED_GAP,
                Some("unsupported_document") => UPDATE_COMPANION,
                _ => FAILED,
            },
        },
    })
}

/// One `logId`'s row in the Catch logs list (CMP-R41, CMP UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub log_id: String,
    /// CMP-R40's list, as last read.
    pub characters: String,
    /// The account folder's label (CMP-R40's `{folder}`); `None` while
    /// several hold it and none is picked.
    pub folder: Option<String>,
    /// The **Upload** switch: on when answered **Upload**.
    pub upload: bool,
    /// The folders a pick or **Change folder** chooses from, with CMP-R42's
    /// question for them.
    pub choices: Vec<Choice>,
    pub pick_text: Option<String>,
    /// Offers **Change folder** (CMP-R42).
    pub change_folder: bool,
    /// This ticket's state line; `None` leaves the row to the last run's
    /// result.
    pub line: Option<Line>,
}

/// The window's Catch logs list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub logs: Vec<Row>,
    /// Whether the folders have been read: until then rows show no state
    /// line.
    pub read: bool,
    /// CMP-R41's text after switching **Upload** off.
    pub switched_off: &'static str,
}

impl View {
    /// How many rows ask something of the Player ([`Line::needs_player`]).
    pub fn needing_player(&self) -> usize {
        self.logs
            .iter()
            .filter(|row| row.line.as_ref().is_some_and(Line::needs_player))
            .count()
    }
}

/// The Catch logs list (CMP-R41): every `logId` local state holds, after
/// [`remember`]. `snapshot` is `None` until the first reads have ended;
/// `now` is Unix seconds, for relative times.
pub fn view(state: &State, snapshot: Option<&Snapshot>, installs: &[String], now: i64) -> View {
    let logs = state
        .logs
        .iter()
        .map(|(log_id, log)| match snapshot {
            Some(snapshot) => row(log_id, log, snapshot, installs, now),
            None => Row {
                log_id: log_id.clone(),
                characters: log.characters.clone(),
                folder: Some(folder_label(&log.last_folder, installs)),
                upload: log.answer == Some(Answer::Upload),
                choices: Vec::new(),
                pick_text: None,
                change_folder: false,
                line: None,
            },
        })
        .collect();
    View {
        logs,
        read: snapshot.is_some(),
        switched_off: SWITCHED_OFF,
    }
}

fn row(log_id: &str, log: &LogState, snapshot: &Snapshot, installs: &[String], now: i64) -> Row {
    let decision = decide(log, &Copies::of(snapshot, log_id));
    let choices: Vec<Choice> = decision
        .choices
        .iter()
        .map(|path| Choice {
            path: path.clone(),
            label: folder_label(path, installs),
        })
        .collect();
    let pick_text = (choices.len() > 1).then(|| {
        let labels: Vec<String> = choices.iter().map(|c| c.label.clone()).collect();
        pick_question(&labels)
    });
    let asks_pick = asks_pick(log, &decision);
    let folder = (!asks_pick).then(|| {
        folder_label(
            decision.folder.as_deref().unwrap_or(&log.last_folder),
            installs,
        )
    });
    // The first that applies (CMP UI, Catch logs). CMP-R40 settles a pick
    // before it asks, so the pick question comes before "Upload it?".
    let line = if asks_pick {
        Some(Line::SeveralFolders {
            text: pick_text.clone().unwrap_or_default(),
            dont_upload: log.answer.is_none(),
        })
    } else if awaiting(log, &decision) {
        Some(Line::Awaiting { text: UPLOAD_IT })
    } else if decision.not_found {
        Some(Line::NotFound { text: NOT_FOUND })
    } else if log.recovery.is_some() || log.recovery_notice.is_some() {
        run_line(log, now)
    } else {
        decision
            .problem
            .map(|text| Line::Problem { text })
            .or_else(|| run_line(log, now))
    };
    Row {
        log_id: log_id.to_owned(),
        characters: log.characters.clone(),
        folder,
        upload: log.answer == Some(Answer::Upload),
        change_folder: !asks_pick && log.folder.is_some() && choices.len() > 1,
        choices,
        pick_text,
        line,
    }
}

/// Called with a `logId`.
pub type LogHook = Box<dyn Fn(&str) + Send + Sync>;

/// What upload runs (#28) do when the Player changes an answer. Each defaults
/// to doing nothing until #28 fills it in.
pub struct Hooks {
    /// The Player turned **Upload** on, or picked the folder of a log
    /// answered **Upload**, and the `logId` is now confirmed: start a run
    /// (CMP-R41, CMP-R45).
    pub start_run: LogHook,
    /// The Player turned **Upload** off: drop a queued run of the `logId`,
    /// and stop a running one after its current request (CMP-R41).
    pub stop_runs: LogHook,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            start_run: Box::new(|_| {}),
            stop_runs: Box::new(|_| {}),
        }
    }
}

/// Runs the decisions after every reading and every answer. Reach it with
/// `app.state::<CatchLogs>()`.
pub struct CatchLogs {
    hooks: Hooks,
    /// The `logId`s the first-run window listed, which raise no "Upload it?"
    /// notification in this process (CMP-R100).
    listed: Mutex<HashSet<String>>,
    /// `None` until the first reads have ended.
    confirmed: Mutex<Option<Confirmed>>,
    subscribers: Mutex<Vec<Sender<Confirmed>>>,
    /// One refresh at a time, so raises and the confirmed set follow state
    /// in order.
    serial: Mutex<()>,
}

impl CatchLogs {
    pub fn new(hooks: Hooks) -> Self {
        Self {
            hooks,
            listed: Mutex::default(),
            confirmed: Mutex::default(),
            subscribers: Mutex::default(),
            serial: Mutex::default(),
        }
    }

    /// The confirmed `logId`s with the folder each uploads from (CMP-R45);
    /// empty until the first reads have ended.
    pub fn confirmed(&self) -> Confirmed {
        lock(&self.confirmed).clone().unwrap_or_default()
    }

    /// A channel that receives the whole confirmed set every time it
    /// changes, starting with the current one once the first reads have
    /// ended. It stays open as long as `self`.
    pub fn subscribe(&self) -> Receiver<Confirmed> {
        let (tx, rx) = mpsc::channel();
        // Held across both, so no change can slip in between.
        let confirmed = lock(&self.confirmed);
        if let Some(current) = &*confirmed {
            let _ = tx.send(current.clone());
        }
        lock(&self.subscribers).push(tx);
        rx
    }

    /// The `logId`s the first-run window listed when its step 1 ended
    /// (CMP-R100): none of them raises an "Upload it?" notification in this
    /// process, and one held meanwhile is dropped. (The hold itself, no
    /// notification at all while the window is open, is
    /// [`Notifications::hold`]'s.)
    pub fn list<N: Notifier>(
        &self,
        log_ids: impl IntoIterator<Item = String>,
        notifications: &Notifications<N>,
    ) {
        let mut listed = lock(&self.listed);
        for log_id in log_ids {
            notifications.clear(&CauseKey::AwaitingAnswer(log_id.clone()));
            listed.insert(log_id);
        }
    }

    /// Takes a new reading into local state, raises and clears CMP-R111's
    /// first two notifications, and tells subscribers when the confirmed set
    /// changed.
    pub fn refresh<N: Notifier>(
        &self,
        store: &Store,
        notifications: &Notifications<N>,
        snapshot: &Snapshot,
    ) {
        let _serial = lock(&self.serial);
        self.refresh_serial(store, notifications, snapshot);
    }

    fn refresh_serial<N: Notifier>(
        &self,
        store: &Store,
        notifications: &Notifications<N>,
        snapshot: &Snapshot,
    ) {
        // Written only when something changed (CMP Data: after every change).
        let changes = store.read(|s| remember(&mut s.clone(), snapshot));
        if changes {
            let added: Vec<String> = store.read(|s| {
                snapshot
                    .catch_logs()
                    .into_keys()
                    .filter(|id| !s.logs.contains_key(id))
                    .collect()
            });
            if let Err(e) = store.update(|s| remember(s, snapshot)) {
                log::error!("couldn't write state.json: {e}");
            }
            for log_id in added {
                log::info!("found Catch log {log_id}: awaiting an answer");
            }
        }

        let (causes, confirmed) = store.read(|s| (causes(s, snapshot), confirmed(s, snapshot)));
        let listed = lock(&self.listed);
        for (key, cause) in causes {
            let Some(cause) = cause else {
                notifications.clear(&key);
                continue;
            };
            if let Cause::AwaitingAnswer { log_id, .. } = &cause
                && listed.contains(log_id)
            {
                continue;
            }
            if notifications.raise(&cause) {
                // Never the characters (CMP-R112).
                log::info!("notified: {key:?}");
            }
        }
        drop(listed);
        self.publish(confirmed);
    }

    /// Keeps the confirmed set and tells subscribers when it changed.
    fn publish(&self, confirmed: Confirmed) {
        let mut current = lock(&self.confirmed);
        if current.as_ref() == Some(&confirmed) {
            return;
        }
        lock(&self.subscribers).retain(|tx| tx.send(confirmed.clone()).is_ok());
        *current = Some(confirmed);
    }

    /// The Player's answer: **Upload** or **Don't upload**, from the
    /// question or the **Upload** switch (CMP-R40, CMP-R41). The error is
    /// the text to show.
    pub fn answer<N: Notifier>(
        &self,
        store: &Store,
        notifications: &Notifications<N>,
        snapshot: Option<&Snapshot>,
        log_id: &str,
        upload: bool,
    ) -> Result<(), String> {
        let serial = lock(&self.serial);
        let was = store
            .read(|s| s.logs.get(log_id).map(|log| log.answer))
            .ok_or(UNKNOWN_LOG)?;
        let answer = if upload { Answer::Upload } else { Answer::Skip };
        if was == Some(answer) {
            return Ok(());
        }
        let written = store.update(|s| {
            if let Some(log) = s.logs.get_mut(log_id) {
                log.answer = Some(answer);
            }
        });
        if let Err(e) = written {
            log::error!("couldn't write state.json: {e}");
        }
        log::info!(
            "Catch log {log_id}: {}",
            if upload { "Upload" } else { "Don't upload" }
        );
        if let Some(snapshot) = snapshot {
            self.refresh_serial(store, notifications, snapshot);
        }
        // Hooks run unlocked, so they may call back here.
        drop(serial);
        if upload {
            if self.confirmed().contains_key(log_id) {
                (self.hooks.start_run)(log_id);
            }
        } else if was == Some(Answer::Upload) {
            (self.hooks.stop_runs)(log_id);
        }
        Ok(())
    }

    /// The Player's pick of the folder a `logId` uploads from, from CMP-R42's
    /// question or **Change folder**. The error is the text to show.
    pub fn pick<N: Notifier>(
        &self,
        store: &Store,
        notifications: &Notifications<N>,
        snapshot: &Snapshot,
        log_id: &str,
        folder: &str,
    ) -> Result<(), String> {
        let serial = lock(&self.serial);
        if store.read(|s| !s.logs.contains_key(log_id)) {
            return Err(UNKNOWN_LOG.into());
        }
        let copies = Copies::of(snapshot, log_id);
        let Some(picked) = copies.holders.iter().find(|f| same_folder(&f.path, folder)) else {
            return Err(NOT_HELD.into());
        };
        let before = self.confirmed().get(log_id).cloned();
        let written = store.update(|s| {
            if let Some(log) = s.logs.get_mut(log_id) {
                log.folder = Some(picked.path.clone());
            }
        });
        if let Err(e) = written {
            log::error!("couldn't write state.json: {e}");
        }
        log::info!("Catch log {log_id}: picked {}", picked.path);
        self.refresh_serial(store, notifications, snapshot);
        let after = self.confirmed().get(log_id).cloned();
        drop(serial);
        if after.is_some() && after != before {
            (self.hooks.start_run)(log_id);
        }
        Ok(())
    }
}

/// What an answer or a pick for a `logId` local state doesn't hold says.
const UNKNOWN_LOG: &str = "Pagle Companion doesn't know this Catch log.";

/// What a pick of a folder that doesn't hold the `logId` says.
const NOT_HELD: &str = "That account folder no longer holds this Catch log.";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::pagle_log::{self, Found};
    use crate::readings::fixture::{LOG_A, LOG_B, pagle_lua_text};
    use crate::readings::{CANT_UPLOAD, COULDNT_READ, FileStamp, LocalRead};
    use crate::state::{self, CatchLogState, LastRun, Loaded};

    const INSTALL: &str = "G:/WoW/_classic_beta_";
    const ONE: &str = "G:/WoW/_classic_beta_/WTF/Account/ADA#1";
    const TWO: &str = "G:/WoW/_classic_beta_/WTF/Account/ADA#2";
    const THREE: &str = "G:/WoW/_classic_beta_/WTF/Account/ADA#3";

    fn reading(path: &str, reading: Reading) -> FolderReading {
        FolderReading {
            flavor: INSTALL.into(),
            name: path.rsplit('/').next().unwrap().into(),
            path: path.into(),
            polled: false,
            last_log_id: reading.log_id().map(str::to_owned),
            reading,
        }
    }

    /// An account folder whose `Pagle.lua` holds a valid local log.
    fn holding(path: &str, log_id: &str, entries: u64) -> FolderReading {
        let Ok(Found::Valid(log)) = pagle_log::parse(pagle_lua_text(log_id, entries).as_bytes())
        else {
            panic!("fixture not valid");
        };
        let file = FileStamp {
            mtime_ms: 1_791_735_090_999,
            size: 1,
        };
        reading(
            path,
            Reading::Valid(LocalRead {
                log: Arc::new(log),
                as_of: file.as_of(),
                file,
            }),
        )
    }

    fn no_log(path: &str) -> FolderReading {
        reading(path, Reading::NoCatchLog { file: None })
    }

    fn unreadable(path: &str, last: &str) -> FolderReading {
        let mut folder = reading(path, Reading::Unreadable);
        folder.last_log_id = Some(last.into());
        folder
    }

    fn snapshot(folders: Vec<FolderReading>) -> Snapshot {
        Snapshot { folders }
    }

    fn snapshot_of(folders: Vec<FolderReading>) -> Snapshot {
        snapshot(folders)
    }

    fn installs() -> Vec<String> {
        vec![INSTALL.into()]
    }

    fn kept(answer: Option<Answer>, folder: Option<&str>, last_folder: &str) -> LogState {
        LogState {
            answer,
            folder: folder.map(str::to_owned),
            characters: "Ada Brook".into(),
            last_folder: last_folder.into(),
            uploaded_snapshot: None,
            observed_snapshot: None,
            recovery: None,
            recovery_notice: None,
            last_run: None,
            last_uploaded_at: None,
        }
    }

    fn state_with(logs: &[(&str, LogState)]) -> State {
        State {
            first_run_done: true,
            logs: logs
                .iter()
                .map(|(id, log)| ((*id).to_owned(), log.clone()))
                .collect(),
            ..State::default()
        }
    }

    /// `remember`, then the row of `log_id`.
    fn row_of(state: &mut State, snapshot: &Snapshot, log_id: &str) -> Row {
        remember(state, snapshot);
        view(state, Some(snapshot), &installs(), 0)
            .logs
            .into_iter()
            .find(|r| r.log_id == log_id)
            .expect("no row")
    }

    fn present(state: &State, snapshot: &Snapshot) -> Vec<Cause> {
        causes(state, snapshot)
            .into_iter()
            .filter_map(|(_, cause)| cause)
            .collect()
    }

    // The pure decisions.

    #[test]
    fn the_texts_are_cmp_r40s_r41s_and_r42s() {
        // CMP-R40: the question, as the row's state line says it (CMP UI);
        // CMP-R41: "Not found on this computer" and the text after switching
        // Upload off; CMP-R42: the pick question
        assert_eq!(UPLOAD_IT, "Upload it?");
        assert_eq!(
            pick_question(&["ADA#1".into(), "ADA#2".into()]),
            "The same Catch log is in 2 account folders: ADA#1, ADA#2. It was probably \
             copied. Pagle uploads it from one folder only. Pick the one you play on."
        );
        assert_eq!(NOT_FOUND, "Not found on this computer");
        assert_eq!(
            SWITCHED_OFF,
            "Pagle keeps what it already has. To remove it, delete it on the Pagle website."
        );
    }

    #[test]
    fn my_anglers_is_under_the_base_url() {
        // CMP-R41: a link to My Anglers, `<base URL>/me/anglers`
        assert_eq!(
            my_anglers_url("https://pagle.app"),
            "https://pagle.app/me/anglers"
        );
        assert_eq!(
            my_anglers_url("http://localhost:8080/"),
            "http://localhost:8080/me/anglers"
        );
    }

    #[test]
    fn the_folder_names_its_install_only_when_several_are_watched() {
        // CMP-R40: `{folder}` is the account folder's name, followed by
        // " ({flavor folder name})" when more than one install is watched,
        // or by " ({install root path}/{flavor folder name})" when two share
        // a flavor folder name
        let beta = "G:/World of Warcraft/_classic_beta_";
        let era = "G:/World of Warcraft/_classic_era_";
        let other_beta = "D:/Games/WoW/_classic_beta_";
        let folder = format!("{beta}/WTF/Account/ADA#1");

        assert_eq!(folder_label(&folder, &[beta.into()]), "ADA#1");
        assert_eq!(
            folder_label(&folder, &[beta.into(), era.into()]),
            "ADA#1 (_classic_beta_)"
        );
        assert_eq!(
            folder_label(&folder, &[beta.into(), other_beta.into(), era.into()]),
            "ADA#1 (G:/World of Warcraft/_classic_beta_)"
        );
        assert_eq!(
            folder_label(
                &format!("{era}/WTF/Account/ADA#1"),
                &[beta.into(), other_beta.into(), era.into()]
            ),
            "ADA#1 (_classic_era_)"
        );
    }

    #[test]
    fn a_new_log_id_with_entries_is_remembered_awaiting_and_asked_about() {
        // CMP-R40: the first time a logId is seen in a valid local log with
        // entries, the Player is asked, in the list and with a notification
        let mut state = state_with(&[]);
        let snapshot = snapshot(vec![holding(ONE, LOG_A, 2)]);

        assert!(remember(&mut state, &snapshot));
        let log = &state.logs[LOG_A];
        assert_eq!(log.answer, None);
        assert_eq!(log.folder, None);
        assert_eq!(log.characters, "Ada Brook");
        assert_eq!(log.last_folder, ONE);
        assert!(!remember(&mut state, &snapshot));

        let row = row_of(&mut state, &snapshot, LOG_A);
        assert_eq!(row.folder.as_deref(), Some("ADA#1"));
        assert!(!row.upload);
        assert_eq!(row.line, Some(Line::Awaiting { text: "Upload it?" }));
        assert_eq!(
            present(&state, &snapshot),
            [Cause::AwaitingAnswer {
                log_id: LOG_A.into(),
                characters: "Ada Brook".into()
            }]
        );
    }

    #[test]
    fn an_empty_or_invalid_log_is_not_offered() {
        // CMP-R32: a local log with no entries is not offered; CMP-R31: an
        // invalid local log is never uploaded
        let mut state = state_with(&[]);
        let invalid = reading(
            TWO,
            Reading::Invalid {
                log_id: Some(LOG_B.into()),
                file: FileStamp {
                    mtime_ms: 0,
                    size: 1,
                },
            },
        );
        let snapshot = snapshot(vec![holding(ONE, LOG_A, 0), invalid]);
        assert!(!remember(&mut state, &snapshot));
        assert!(state.logs.is_empty());
        assert!(causes(&state, &snapshot).is_empty());
    }

    #[test]
    fn awaiting_and_dont_upload_logs_are_never_confirmed() {
        // CMP-R40: until the Player answers, nothing of the log leaves the
        // machine; only logs answered Upload are ever uploaded
        let snapshot = snapshot(vec![
            holding(ONE, LOG_A, 1),
            holding(TWO, LOG_B, 1),
            holding(THREE, "00000000000000ff", 1),
        ]);
        let mut state = state_with(&[
            (LOG_A, kept(None, None, ONE)),
            (LOG_B, kept(Some(Answer::Skip), None, TWO)),
            ("00000000000000ff", kept(Some(Answer::Upload), None, THREE)),
        ]);
        remember(&mut state, &snapshot);
        assert_eq!(
            confirmed(&state, &snapshot),
            Confirmed::from([("00000000000000ff".into(), THREE.into())])
        );
        // An answered log raises nothing.
        assert_eq!(present(&state, &snapshot).len(), 1);
    }

    #[test]
    fn one_log_id_in_two_folders_asks_for_a_pick_and_isnt_uploaded() {
        // CMP-R42: until a folder is picked none is uploaded, and the window
        // and a notification say "The same Catch log is in 2 account
        // folders: …"; CMP-R40: its question follows once a folder is picked
        let snapshot = snapshot(vec![holding(ONE, LOG_A, 2), holding(TWO, LOG_A, 1)]);
        for answer in [None, Some(Answer::Upload)] {
            let mut state = state_with(&[(LOG_A, kept(answer, None, ONE))]);
            let row = row_of(&mut state, &snapshot, LOG_A);
            assert_eq!(row.folder, None);
            assert_eq!(
                row.line,
                Some(Line::SeveralFolders {
                    text: pick_question(&["ADA#1".into(), "ADA#2".into()]),
                    dont_upload: answer.is_none(),
                })
            );
            assert_eq!(
                row.choices,
                [
                    Choice {
                        path: ONE.into(),
                        label: "ADA#1".into()
                    },
                    Choice {
                        path: TWO.into(),
                        label: "ADA#2".into()
                    }
                ]
            );
            assert!(!row.change_folder);
            assert!(confirmed(&state, &snapshot).is_empty());
            assert_eq!(
                present(&state, &snapshot),
                [Cause::SeveralFolders {
                    log_id: LOG_A.into(),
                    folders: 2
                }]
            );
        }
    }

    #[test]
    fn a_picked_folder_is_the_one_uploaded_from_and_can_be_changed() {
        // CMP-R42: uploaded from the picked folder only; the pick is
        // remembered and the row offers Change folder
        let snapshot = snapshot(vec![holding(ONE, LOG_A, 2), holding(TWO, LOG_A, 1)]);
        let mut state = state_with(&[(LOG_A, kept(Some(Answer::Upload), Some(TWO), ONE))]);
        let row = row_of(&mut state, &snapshot, LOG_A);
        assert_eq!(state.logs[LOG_A].folder.as_deref(), Some(TWO));
        assert_eq!(row.folder.as_deref(), Some("ADA#2"));
        assert!(row.change_folder);
        assert_eq!(row.line, None);
        assert_eq!(
            row.pick_text,
            Some(pick_question(&["ADA#1".into(), "ADA#2".into()]))
        );
        assert_eq!(
            confirmed(&state, &snapshot),
            Confirmed::from([(LOG_A.into(), TWO.into())])
        );
        assert!(present(&state, &snapshot).is_empty());
        // CMP-R41: the characters and folder shown are the picked folder's.
        assert_eq!(state.logs[LOG_A].last_folder, TWO);
    }

    #[test]
    fn a_pick_whose_folder_lost_the_log_moves_to_the_one_other_without_asking() {
        // CMP-R42: if the picked folder no longer holds that logId and
        // exactly one other folder does, that one becomes the pick without
        // asking; CMP Data: folder is null when the logId sits in one folder
        let mut state = state_with(&[(LOG_A, kept(Some(Answer::Upload), Some(ONE), ONE))]);
        let snapshot = snapshot(vec![no_log(ONE), holding(TWO, LOG_A, 1)]);
        let row = row_of(&mut state, &snapshot, LOG_A);
        assert_eq!(state.logs[LOG_A].folder, None);
        assert_eq!(state.logs[LOG_A].last_folder, TWO);
        assert_eq!(row.folder.as_deref(), Some("ADA#2"));
        assert_eq!(row.line, None);
        assert_eq!(
            confirmed(&state, &snapshot),
            Confirmed::from([(LOG_A.into(), TWO.into())])
        );
        assert!(present(&state, &snapshot).is_empty());
    }

    #[test]
    fn a_pick_whose_folder_lost_the_log_to_several_others_asks_again() {
        // CMP-R42: if several other folders hold it, the Player is asked
        // again
        let mut state = state_with(&[(LOG_A, kept(Some(Answer::Upload), Some(ONE), ONE))]);
        let snapshot = snapshot(vec![
            no_log(ONE),
            holding(TWO, LOG_A, 1),
            holding(THREE, LOG_A, 1),
        ]);
        let row = row_of(&mut state, &snapshot, LOG_A);
        assert_eq!(state.logs[LOG_A].folder, None);
        assert!(matches!(row.line, Some(Line::SeveralFolders { .. })));
        assert!(confirmed(&state, &snapshot).is_empty());
    }

    #[test]
    fn a_pick_that_leaves_one_folder_is_set_back_to_null() {
        // CMP Data: folder is set back to null when a pick leaves only one
        let mut state = state_with(&[(LOG_A, kept(Some(Answer::Upload), Some(ONE), ONE))]);
        let snapshot = snapshot(vec![holding(ONE, LOG_A, 1), no_log(TWO)]);
        assert!(remember(&mut state, &snapshot));
        assert_eq!(state.logs[LOG_A].folder, None);
        assert_eq!(
            confirmed(&state, &snapshot),
            Confirmed::from([(LOG_A.into(), ONE.into())])
        );
    }

    #[test]
    fn a_pick_on_a_folder_that_failed_to_read_stays() {
        // CMP-R30: a failed read gives up until the next event, which tells
        // nothing of what the file holds: the pick stays, nothing is
        // re-picked, and the row shows CMP-R30's text
        let mut state = state_with(&[(LOG_A, kept(Some(Answer::Upload), Some(ONE), ONE))]);
        let snapshot = snapshot(vec![unreadable(ONE, LOG_A), holding(TWO, LOG_A, 1)]);
        let row = row_of(&mut state, &snapshot, LOG_A);
        assert_eq!(state.logs[LOG_A].folder.as_deref(), Some(ONE));
        assert_eq!(row.line, Some(Line::Problem { text: COULDNT_READ }));
        assert_eq!(
            confirmed(&state, &snapshot),
            Confirmed::from([(LOG_A.into(), ONE.into())])
        );
    }

    #[test]
    fn a_dont_upload_log_in_two_folders_needs_no_pick() {
        // CMP-R40: a logId answered Don't upload needs no folder pick
        let mut state = state_with(&[(LOG_A, kept(Some(Answer::Skip), None, ONE))]);
        let snapshot = snapshot(vec![holding(ONE, LOG_A, 1), holding(TWO, LOG_A, 1)]);
        let row = row_of(&mut state, &snapshot, LOG_A);
        assert_eq!(row.line, None);
        assert!(present(&state, &snapshot).is_empty());
        assert!(confirmed(&state, &snapshot).is_empty());
    }

    #[test]
    fn a_log_no_file_holds_is_not_found() {
        // CMP-R41: a log whose watched file no longer holds it shows "Not
        // found on this computer"; it awaits no answer meanwhile
        let snapshot = snapshot(vec![no_log(ONE)]);
        for answer in [None, Some(Answer::Upload), Some(Answer::Skip)] {
            let mut state = state_with(&[(LOG_A, kept(answer, None, ONE))]);
            let row = row_of(&mut state, &snapshot, LOG_A);
            assert_eq!(row.line, Some(Line::NotFound { text: NOT_FOUND }));
            assert_eq!(row.folder.as_deref(), Some("ADA#1"));
            assert_eq!(row.characters, "Ada Brook");
            assert!(present(&state, &snapshot).is_empty());
            assert!(confirmed(&state, &snapshot).is_empty());
        }
    }

    #[test]
    fn an_invalid_log_shows_cmp_r31s_text_on_its_row() {
        // CMP-R31: its logId's row, when the logId could be read, shows
        // "Pagle.lua holds a Catch log this Companion can't upload. …"
        let mut state = state_with(&[(LOG_A, kept(Some(Answer::Upload), None, ONE))]);
        let snapshot = snapshot(vec![reading(
            ONE,
            Reading::Invalid {
                log_id: Some(LOG_A.into()),
                file: FileStamp {
                    mtime_ms: 0,
                    size: 1,
                },
            },
        )]);
        let row = row_of(&mut state, &snapshot, LOG_A);
        assert_eq!(row.line, Some(Line::Problem { text: CANT_UPLOAD }));
    }

    #[test]
    fn the_first_line_that_applies_is_shown() {
        // CMP UI, Catch logs: a row shows one state line, the first that
        // applies: awaiting, several folders none picked, not found, invalid
        // or unreadable, then the last run's result
        let mut state = state_with(&[(LOG_A, kept(None, None, ONE))]);
        let snapshot = snapshot(vec![unreadable(ONE, LOG_A)]);
        let row = row_of(&mut state, &snapshot, LOG_A);
        assert!(matches!(row.line, Some(Line::Awaiting { .. })));

        let mut state = state_with(&[(LOG_A, kept(Some(Answer::Upload), None, ONE))]);
        let row = row_of(&mut state, &snapshot, LOG_A);
        assert_eq!(row.line, Some(Line::Problem { text: COULDNT_READ }));
    }

    #[test]
    fn a_read_keeps_characters_and_last_folder_of_a_known_log_id() {
        // CMP-R41: its characters as last read, kept in local state; CMP
        // Data: lastFolder, the account folder the logId was last read from
        let mut state = state_with(&[(LOG_A, kept(Some(Answer::Upload), None, ONE))]);
        state.logs.get_mut(LOG_A).unwrap().characters = "Ada".into();
        let snapshot = snapshot(vec![no_log(ONE), holding(TWO, LOG_A, 1)]);
        assert!(remember(&mut state, &snapshot));
        assert_eq!(state.logs[LOG_A].characters, "Ada Brook");
        assert_eq!(state.logs[LOG_A].last_folder, TWO);
        assert!(!remember(&mut state, &snapshot));
    }

    #[test]
    fn the_window_gets_each_line_with_its_kind_and_camel_case_fields() {
        // CMP UI, Catch logs: the several-folders line carries its folder
        // buttons and, while awaiting, Don't upload (CMP-R40)
        let line = Line::SeveralFolders {
            text: "t".into(),
            dont_upload: true,
        };
        assert_eq!(
            serde_json::to_value(&line).unwrap(),
            serde_json::json!({ "kind": "several-folders", "text": "t", "dontUpload": true })
        );
        assert_eq!(
            serde_json::to_value(Line::NotFound { text: NOT_FOUND }).unwrap(),
            serde_json::json!({ "kind": "not-found", "text": NOT_FOUND })
        );
    }

    #[test]
    fn the_lines_that_need_the_player_are_the_windows_act_ones() {
        // CMP UI, Catch logs: awaiting, several folders none picked,
        // divergence, deleted, log cap and held characters ask something of
        // the Player (ui/catch-logs.js, the "act" tone); nothing else does
        let held = vec![Note::HeldCharacters {
            text: HELD_CHARACTERS,
        }];
        let pending = vec![
            Note::Pending { text: "t" },
            Note::LeftOff { text: LEFT_OFF },
        ];
        let needs = [
            Line::Awaiting { text: UPLOAD_IT },
            Line::SeveralFolders {
                text: "t".into(),
                dont_upload: false,
            },
            Line::Divergence {
                text: DIVERGENCE.into(),
                question: RECOVERY_EXPLANATION,
            },
            Line::Deleted { text: DELETED },
            Line::LogCap {
                text: LOG_CAP.into(),
            },
            Line::Uploaded {
                text: "t".into(),
                notes: held.clone(),
            },
            Line::UpToDate {
                text: UP_TO_DATE,
                notes: held,
            },
        ];
        let doesnt = [
            Line::NotFound { text: NOT_FOUND },
            Line::Problem { text: CANT_UPLOAD },
            Line::Uploaded {
                text: "t".into(),
                notes: pending.clone(),
            },
            Line::UpToDate {
                text: UP_TO_DATE,
                notes: pending,
            },
            Line::Stale { text: STALE },
            Line::DevLog { text: DEV_LOG },
            Line::WaitingForPagle {
                text: WAITING_FOR_PAGLE,
            },
            Line::TooLarge { text: TOO_LARGE },
            Line::Failed { text: FAILED },
        ];
        for line in &needs {
            assert!(line.needs_player(), "{line:?}");
        }
        for line in &doesnt {
            assert!(!line.needs_player(), "{line:?}");
        }

        // Rows still unread show no line, and need nothing.
        let row = |line| Row {
            log_id: LOG_A.into(),
            characters: String::new(),
            folder: None,
            upload: true,
            choices: Vec::new(),
            pick_text: None,
            change_folder: false,
            line,
        };
        let view = View {
            logs: vec![
                row(None),
                row(Some(Line::Deleted { text: DELETED })),
                row(Some(Line::Stale { text: STALE })),
                row(Some(Line::Awaiting { text: UPLOAD_IT })),
            ],
            read: true,
            switched_off: SWITCHED_OFF,
        };
        assert_eq!(view.needing_player(), 2);
    }

    #[test]
    fn before_the_first_reads_rows_show_no_state_line() {
        // CMP-R41: the window lists every Catch log the Companion has seen,
        // with its characters and folder as last kept
        let state = state_with(&[(LOG_A, kept(Some(Answer::Upload), None, ONE))]);
        let view = view(&state, None, &installs(), 0);
        assert!(!view.read);
        assert_eq!(view.logs.len(), 1);
        assert_eq!(view.logs[0].line, None);
        assert!(view.logs[0].upload);
        assert_eq!(view.logs[0].folder.as_deref(), Some("ADA#1"));
    }

    // The last run's lines (CMP UI, Catch logs).

    fn ran(result: RunResult, code: Option<&str>, catch_log: Option<CatchLogState>) -> LogState {
        let mut log = kept(Some(Answer::Upload), None, ONE);
        log.last_run = Some(LastRun {
            result,
            at: 1_000,
            file_mtime_ms: 1,
            file_size: 1,
            format: 1,
            companion_version: "1.0.0".into(),
            code: code.map(str::to_owned),
            message: code.map(|_| "You already have 20 Catch logs on Pagle.".into()),
            catch_log,
        });
        log
    }

    fn text(line: Option<Line>) -> String {
        serde_json::to_value(line).unwrap()["text"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn every_result_has_its_line() {
        // CMP UI, Catch logs: the line of each last run's result
        let cases = [
            (RunResult::Uploaded, None, "Uploaded 5 minutes ago"),
            (RunResult::UpToDate, None, "Up to date"),
            (
                RunResult::Divergence,
                None,
                "This Catch log differs from the copy on Pagle.",
            ),
            (
                RunResult::Stale,
                Some("stale"),
                "Pagle already has a newer copy of this Catch log. Nothing changed.",
            ),
            (
                RunResult::Deleted,
                Some("deleted"),
                "You deleted this Catch log on the Pagle website. Allow it again there to \
                 upload it.",
            ),
            (
                RunResult::DevLog,
                Some("dev_log"),
                "This Catch log is from a Pagle development version and can't be uploaded.",
            ),
            (
                RunResult::WaitingForPagle,
                Some("unsupported_format"),
                "The Pagle website can't read this version of the addon yet. Pagle Companion \
                 tries again later.",
            ),
            (
                RunResult::LogCap,
                Some("log_cap"),
                "You already have 20 Catch logs on Pagle.",
            ),
            (
                RunResult::TooLarge,
                None,
                "This Catch log has an entry too large to upload.",
            ),
            (
                RunResult::Failed,
                Some("entry_cap"),
                "Upload failed. Pagle Companion tries again when the file changes.",
            ),
            (
                RunResult::Failed,
                Some("gap"),
                "Upload failed. Pagle Companion tries again later.",
            ),
            (
                RunResult::Failed,
                Some("unsupported_document"),
                "Update Pagle Companion.",
            ),
        ];
        for (result, code, expected) in cases {
            let line = run_line(&ran(result, code, None), 1_000 + 5 * 60);
            assert_eq!(text(line), expected, "{result:?} {code:?}");
        }
        assert_eq!(run_line(&kept(Some(Answer::Upload), None, ONE), 0), None);
    }

    #[test]
    fn log_cap_without_a_message_says_the_cap() {
        // CMP UI, Catch logs: the log_cap message (UPL Refusals)
        let mut log = ran(RunResult::LogCap, Some("log_cap"), None);
        log.last_run.as_mut().unwrap().message = None;
        assert_eq!(
            run_line(&log, 0),
            Some(Line::LogCap {
                text: "You already have 20 Catch logs on Pagle. Delete one to add another.".into()
            })
        );
    }

    fn catch_log(
        verification: &str,
        reason: Option<&str>,
        held: u32,
        left_off: bool,
    ) -> CatchLogState {
        CatchLogState {
            verification: verification.into(),
            pending_reason: reason.map(str::to_owned),
            held_characters: held,
            left_off: left_off.then(|| serde_json::json!("flagged")),
        }
    }

    #[test]
    fn every_pending_reason_has_its_text() {
        // CMP-R53: pending with a pendingReason shows the text for that
        // reason (accounts.md UI, Pending reasons)
        let cases = [
            (
                "forever-api",
                "Battle.net doesn't list WoW Forever characters yet. Your Catch log is safe \
                 here and will be verified once it does.",
            ),
            (
                "sign-in-needed",
                "Sign in again so Pagle can verify this Catch log.",
            ),
            (
                "no-wow-profile",
                "Pagle couldn't see your WoW characters. Sign in again, leave 'World of \
                 Warcraft profile' ticked, and check that your Battle.net privacy settings \
                 allow third-party sites.",
            ),
            (
                "mixed-accounts",
                "This Catch log holds characters from more than one WoW account; it was \
                 probably copied from another PC. If you just created a character, log out of \
                 it in game and sign in again in a few minutes.",
            ),
            (
                "not-listed",
                "None of this Catch log's characters are in your Battle.net account.",
            ),
        ];
        for (reason, text) in cases {
            assert_eq!(
                catch_log_notes(&catch_log("pending", Some(reason), 0, false)),
                [Note::Pending { text }]
            );
        }
        // A reason this Companion doesn't know shows nothing.
        assert!(catch_log_notes(&catch_log("pending", Some("new-reason"), 0, false)).is_empty());
    }

    #[test]
    fn catch_log_lines_show_all_that_apply_in_order() {
        // CMP-R53: pending with a null reason, or verified with no held
        // characters: nothing more; CMP UI: Pending reason, held characters,
        // left off, all that apply, in that order, under uploaded and up to
        // date only
        assert!(catch_log_notes(&catch_log("pending", None, 0, false)).is_empty());
        assert!(catch_log_notes(&catch_log("verified", None, 0, false)).is_empty());
        let notes = catch_log_notes(&catch_log("verified", None, 2, true));
        assert_eq!(
            notes,
            [
                Note::HeldCharacters {
                    text: HELD_CHARACTERS
                },
                Note::LeftOff { text: LEFT_OFF }
            ]
        );
        assert_eq!(
            HELD_CHARACTERS,
            "A new character needs verifying. Sign in again to verify it."
        );
        assert_eq!(
            LEFT_OFF,
            "This Angler is left off Leaderboards and First reported. Your Angler page says why."
        );
        let state = catch_log("verified", None, 2, true);
        let failed = ran(RunResult::Failed, Some("entry_cap"), Some(state.clone()));
        assert!(matches!(run_line(&failed, 0), Some(Line::Failed { .. })));
        let up_to_date = ran(RunResult::UpToDate, None, Some(state));
        assert_eq!(
            run_line(&up_to_date, 0),
            Some(Line::UpToDate {
                text: UP_TO_DATE,
                notes
            })
        );
    }

    #[test]
    fn the_divergence_line_explains_recovery_and_game_folder_discovery() {
        // CMP-R52: explain preservation and game folder discovery.
        let log = ran(RunResult::Divergence, None, None);
        assert_eq!(
            serde_json::to_value(run_line(&log, 0)).unwrap(),
            serde_json::json!({
                "kind": "divergence",
                "text": "This Catch log differs from the copy on Pagle.",
                "question": RECOVERY_EXPLANATION
            })
        );
    }

    #[test]
    fn the_read_lines_come_before_the_last_runs_result() {
        // CMP UI, Catch logs: not found, invalid or unreadable come before the
        // last run's result
        let snapshot = snapshot(vec![unreadable(ONE, LOG_A)]);
        let mut state = state_with(&[(LOG_A, ran(RunResult::Deleted, Some("deleted"), None))]);
        let row = row_of(&mut state, &snapshot, LOG_A);
        assert_eq!(row.line, Some(Line::Problem { text: COULDNT_READ }));
        let snapshot = snapshot_of(vec![holding(ONE, LOG_A, 2)]);
        let row = row_of(&mut state, &snapshot, LOG_A);
        assert!(matches!(row.line, Some(Line::Deleted { .. })));
    }

    // The service: notifications, answers, picks and the confirmed set.

    #[derive(Default, Clone)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Notifier for Recorder {
        fn show(&self, text: &str) {
            self.0.lock().unwrap().push(text.to_owned());
        }
    }

    impl Recorder {
        fn shown(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    struct Running {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        store: Store,
        recorder: Recorder,
        notifications: Notifications<Recorder>,
        catch_logs: CatchLogs,
    }

    fn running(state: State, hooks: Hooks) -> Running {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let recorder = Recorder::default();
        Running {
            store: Store::new(&path, state),
            path,
            _dir: dir,
            notifications: Notifications::new(recorder.clone()),
            recorder,
            catch_logs: CatchLogs::new(hooks),
        }
    }

    impl Running {
        fn refresh(&self, snapshot: &Snapshot) {
            self.catch_logs
                .refresh(&self.store, &self.notifications, snapshot);
        }

        fn answer(&self, snapshot: &Snapshot, upload: bool) -> Result<(), String> {
            self.catch_logs.answer(
                &self.store,
                &self.notifications,
                Some(snapshot),
                LOG_A,
                upload,
            )
        }

        fn pick(&self, snapshot: &Snapshot, folder: &str) -> Result<(), String> {
            self.catch_logs
                .pick(&self.store, &self.notifications, snapshot, LOG_A, folder)
        }
    }

    /// Hooks that record each call as `start <logId>` or `stop <logId>`.
    fn recording_hooks() -> (Hooks, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (start, stop) = (Arc::clone(&calls), Arc::clone(&calls));
        let hooks = Hooks {
            start_run: Box::new(move |id| start.lock().unwrap().push(format!("start {id}"))),
            stop_runs: Box::new(move |id| stop.lock().unwrap().push(format!("stop {id}"))),
        };
        (hooks, calls)
    }

    #[test]
    fn an_awaiting_log_notifies_once_per_process_and_again_only_after_it_cleared() {
        // CMP-R110: each is raised once per cause per process, and again
        // only after that cause has cleared; CMP-R111: "Found a Catch log:
        // {characters}. Upload it?"
        let running = running(state_with(&[]), Hooks::default());
        let snapshot = snapshot(vec![holding(ONE, LOG_A, 1)]);
        running.refresh(&snapshot);
        running.refresh(&snapshot);
        assert_eq!(
            running.recorder.shown(),
            ["Found a Catch log: Ada Brook. Upload it?"]
        );

        // Answered: the cause clears, and an answered log never asks.
        running.answer(&snapshot, false).unwrap();
        running.refresh(&snapshot);
        assert_eq!(running.recorder.shown().len(), 1);

        // Awaiting again (local state lost its answer): it notifies again.
        running
            .store
            .update(|s| s.logs.get_mut(LOG_A).unwrap().answer = None)
            .unwrap();
        running.refresh(&snapshot);
        assert_eq!(running.recorder.shown().len(), 2);
    }

    #[test]
    fn several_folders_notify_once_and_clear_when_picked() {
        // CMP-R111: "The same Catch log is in {n} account folders. Pick the
        // one you play on."; CMP-R40: the question follows once a folder is
        // picked
        let running = running(state_with(&[]), Hooks::default());
        let snapshot = snapshot(vec![holding(ONE, LOG_A, 1), holding(TWO, LOG_A, 1)]);
        running.refresh(&snapshot);
        running.refresh(&snapshot);
        assert_eq!(
            running.recorder.shown(),
            ["The same Catch log is in 2 account folders. Pick the one you play on."]
        );

        running.pick(&snapshot, TWO).unwrap();
        assert_eq!(
            running
                .store
                .read(|s| s.logs[LOG_A].folder.clone())
                .as_deref(),
            Some(TWO)
        );
        assert_eq!(
            running.recorder.shown()[1],
            "Found a Catch log: Ada Brook. Upload it?"
        );
        assert!(running.catch_logs.confirmed().is_empty());
    }

    #[test]
    fn headless_mode_keeps_a_new_log_awaiting_and_a_later_tray_start_asks() {
        // CMP-R40: in headless mode nothing is asked; the log stays awaiting
        // until a tray-mode process asks. CMP-R1: headless mode shows no
        // notification.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let snapshot = snapshot(vec![holding(ONE, LOG_A, 2)]);
        let headless = CatchLogs::new(Hooks::default());
        let store = Store::new(&path, state_with(&[]));
        let silent: Notifications<Box<dyn Notifier>> =
            Notifications::new(Box::new(crate::notify::Silent));
        headless.refresh(&store, &silent, &snapshot);
        assert!(headless.confirmed().is_empty());

        let Loaded::State(saved) = state::load(&path).unwrap() else {
            panic!("state.json wasn't written");
        };
        assert_eq!(saved.logs[LOG_A].answer, None);

        // The tray-mode process that starts next asks about it.
        let tray = running(*saved, Hooks::default());
        tray.refresh(&snapshot);
        assert_eq!(
            tray.recorder.shown(),
            ["Found a Catch log: Ada Brook. Upload it?"]
        );
    }

    #[test]
    fn a_pick_must_be_a_folder_that_holds_the_log() {
        // CMP-R42: one button per folder that holds it
        let running = running(state_with(&[]), Hooks::default());
        let snapshot = snapshot(vec![holding(ONE, LOG_A, 1), holding(TWO, LOG_A, 1)]);
        running.refresh(&snapshot);
        assert!(running.pick(&snapshot, THREE).is_err());
        assert_eq!(running.store.read(|s| s.logs[LOG_A].folder.clone()), None);
    }

    #[test]
    fn dont_upload_is_remembered_across_a_restart_and_nothing_is_confirmed() {
        // CMP-R40: the answer is remembered per logId in local state; only
        // logs answered Upload are ever uploaded
        let first = running(state_with(&[]), Hooks::default());
        let snapshot = snapshot(vec![holding(ONE, LOG_A, 1)]);
        first.refresh(&snapshot);
        first.answer(&snapshot, false).unwrap();

        let Loaded::State(saved) = state::load(&first.path).unwrap() else {
            panic!("state.json wasn't written");
        };
        assert_eq!(saved.logs[LOG_A].answer, Some(Answer::Skip));
        let restarted = running(*saved, Hooks::default());
        restarted.refresh(&snapshot);
        assert!(restarted.recorder.shown().is_empty());
        assert!(restarted.catch_logs.confirmed().is_empty());
    }

    #[test]
    fn switching_upload_on_answers_upload_and_starts_a_run() {
        // CMP-R41: switching it on sets the answer to Upload and starts a run
        // (CMP-R45); switching it off stops future Uploads only
        let (hooks, calls) = recording_hooks();
        let running = running(
            state_with(&[(LOG_A, kept(Some(Answer::Skip), None, ONE))]),
            hooks,
        );
        let snapshot = snapshot(vec![holding(ONE, LOG_A, 1)]);
        running.refresh(&snapshot);
        assert!(running.catch_logs.confirmed().is_empty());

        running.answer(&snapshot, true).unwrap();
        assert_eq!(
            running.store.read(|s| s.logs[LOG_A].answer),
            Some(Answer::Upload)
        );
        assert_eq!(
            running.catch_logs.confirmed(),
            Confirmed::from([(LOG_A.into(), ONE.into())])
        );
        // Switching on what is on already starts nothing more.
        running.answer(&snapshot, true).unwrap();

        running.answer(&snapshot, false).unwrap();
        assert_eq!(
            running.store.read(|s| s.logs[LOG_A].answer),
            Some(Answer::Skip)
        );
        assert!(running.catch_logs.confirmed().is_empty());
        assert_eq!(
            *calls.lock().unwrap(),
            [format!("start {LOG_A}"), format!("stop {LOG_A}")]
        );
    }

    #[test]
    fn upload_switched_on_before_a_pick_starts_its_run_at_the_pick() {
        // CMP-R42: none of the folders is uploaded until a folder is picked;
        // CMP-R45: the run the switch starts waits for the pick
        let (hooks, calls) = recording_hooks();
        let running = running(state_with(&[]), hooks);
        let snapshot = snapshot(vec![holding(ONE, LOG_A, 1), holding(TWO, LOG_A, 1)]);
        running.refresh(&snapshot);
        running.answer(&snapshot, true).unwrap();
        assert!(calls.lock().unwrap().is_empty());

        running.pick(&snapshot, ONE).unwrap();
        assert_eq!(*calls.lock().unwrap(), [format!("start {LOG_A}")]);
        assert_eq!(
            running.catch_logs.confirmed(),
            Confirmed::from([(LOG_A.into(), ONE.into())])
        );
    }

    #[test]
    fn an_unknown_log_id_cant_be_answered() {
        // CMP-R41: the switch belongs to a Catch log the Companion has seen
        let running = running(state_with(&[]), Hooks::default());
        assert!(running.answer(&snapshot(vec![]), true).is_err());
    }

    #[test]
    fn the_confirmed_set_reaches_subscribers_when_it_changes() {
        // CMP-R45: upload runs are for confirmed logIds (answered Upload,
        // with the folder settled by CMP-R42)
        let running = running(
            state_with(&[(LOG_A, kept(Some(Answer::Upload), Some(ONE), ONE))]),
            Hooks::default(),
        );
        let updates = running.catch_logs.subscribe();
        assert!(updates.try_recv().is_err());

        let both = snapshot(vec![holding(ONE, LOG_A, 1), holding(TWO, LOG_A, 1)]);
        running.refresh(&both);
        assert_eq!(
            updates.try_recv().unwrap(),
            Confirmed::from([(LOG_A.into(), ONE.into())])
        );
        // Unchanged: nothing more.
        running.refresh(&both);
        assert!(updates.try_recv().is_err());
        // A late subscriber starts with the current set.
        assert_eq!(running.catch_logs.subscribe().try_recv().unwrap().len(), 1);

        // The picked folder loses it: re-picked without asking.
        let moved = snapshot(vec![no_log(ONE), holding(TWO, LOG_A, 1)]);
        running.refresh(&moved);
        assert_eq!(
            updates.try_recv().unwrap(),
            Confirmed::from([(LOG_A.into(), TWO.into())])
        );
        assert!(running.recorder.shown().is_empty());
    }

    #[test]
    fn no_notification_while_the_first_run_window_is_open() {
        // CMP-R100: no notification is raised while the first-run window is
        // open, and the logIds it listed raise no "Upload it?" notification
        // in that process
        let running = running(state_with(&[]), Hooks::default());
        running.notifications.hold();
        let one = snapshot(vec![holding(ONE, LOG_A, 1)]);
        running.refresh(&one);
        assert!(running.recorder.shown().is_empty());

        // Step 1 ends with LOG_A listed and still awaiting (the window was
        // closed early); LOG_B turns up after.
        running
            .catch_logs
            .list([LOG_A.to_owned()], &running.notifications);
        let more = snapshot(vec![
            holding(ONE, LOG_A, 1),
            holding(TWO, LOG_B, 1),
            holding(THREE, LOG_B, 1),
        ]);
        running.refresh(&more);
        assert!(running.recorder.shown().is_empty());

        running.notifications.release();
        running.refresh(&more);
        assert_eq!(
            running.recorder.shown(),
            ["The same Catch log is in 2 account folders. Pick the one you play on."]
        );
    }

    #[test]
    fn a_listed_log_picked_later_raises_no_upload_it() {
        // CMP-R100: a logId in two folders shows CMP-R42's question in the
        // list; once listed, its "Upload it?" never notifies in that process
        let running = running(state_with(&[]), Hooks::default());
        running.notifications.hold();
        let both = snapshot(vec![holding(ONE, LOG_A, 1), holding(TWO, LOG_A, 1)]);
        running.refresh(&both);
        running
            .catch_logs
            .list([LOG_A.to_owned()], &running.notifications);
        running.notifications.release();
        // Still without a pick when step 1 ended: CMP-R42's notification.
        assert_eq!(running.recorder.shown().len(), 1);

        running.pick(&both, ONE).unwrap();
        running.refresh(&both);
        assert_eq!(running.recorder.shown().len(), 1);
        assert_eq!(running.store.read(|s| s.logs[LOG_A].answer), None);
    }
}

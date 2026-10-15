//! First run (CMP-R100): the window's four steps, Scan, Link, Upload and
//! Done, from the first tray-mode start until the window closes.
//!
//! The steps are a state machine, [`next`]: each [`Event`] (a button, the
//! link stored, the post-link runs ended, the window closed) moves the
//! [`Step`] and names the [`Effect`]s to run, in order. [`FirstRun`] keeps
//! the step and the Player's ticks; the effects are run by [`window`], which
//! also holds the window's commands.
//!
//! Step 1 ends at **Link to Pagle** (or **Continue**, CMP-R100's case of a
//! credential already in its store) and at **Not now**: then the ticks
//! become the answers of CMP-R40 ([`answers`], [`end_step_1`]), local state
//! is written for the first time, the autostart entry is created (CMP-R3)
//! and `firstRunDone` is set. Closing the window before that ends step 1
//! with every found log awaiting. While the window is open no notification
//! is raised (`Notifications::hold`), and the `logId`s step 1 listed never
//! raise "Upload it?" in this process (`CatchLogs::list`).

pub mod window;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};

use serde::Serialize;

use crate::catch_logs::{self, Copies, Line, Row};
use crate::readings::Snapshot;
use crate::state::{Answer, State};

/// The first-run window's step (CMP-R100).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    /// 1. The found list with ticks.
    Scan,
    /// 2. The linking part (CMP-R60).
    Link,
    /// 3. "Uploading…" until each post-link run has ended (CMP-R101).
    Uploading,
    /// 4. The status and **Close**.
    Done,
    /// The window has closed: the first run is over, or this start wasn't
    /// one.
    Over,
}

/// What happens to the first-run window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// **Link to Pagle**, or **Continue** with a credential already in its
    /// store.
    Primary,
    /// **Not now**: skips steps 2 and 3.
    NotNow,
    /// **Continue** found no credential in its store any more: link instead.
    NoCredential,
    /// The link was stored (CMP-R61).
    Linked,
    /// Each post-link run has ended.
    RunsEnded,
    /// **Close**, or the window was closed.
    Close,
}

/// What an [`Event`] makes the Companion do, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Step 1 ends: with `answer`, the ticks become the answers of CMP-R40;
    /// without (the window closed early), every found log stays awaiting.
    /// Then local state is written with `firstRunDone` set, the autostart
    /// entry is created (CMP-R3), and the listed `logId`s raise no "Upload
    /// it?" notification in this process.
    EndStep1 { answer: bool },
    /// Runs the device flow (CMP-R60).
    StartLink,
    /// Keeps the link already in its store and starts the post-link runs
    /// (CMP-R101).
    ContinueLink,
    /// Waits for each post-link run to end, then [`Event::RunsEnded`].
    WaitForRuns,
    /// The first-run window is over: notifications held while it was open
    /// are shown, unless their cause has cleared.
    Release,
}

/// The step after `event` in `step`, and what to do. `credential_stored`:
/// a credential was already in its store when the first run started, so the
/// button reads **Continue**.
pub fn next(step: Step, event: Event, credential_stored: bool) -> (Step, Vec<Effect>) {
    use Effect::*;
    let end_step_1 = EndStep1 { answer: true };
    match (step, event) {
        (Step::Scan, Event::Primary) if credential_stored => {
            (Step::Uploading, vec![end_step_1, ContinueLink, WaitForRuns])
        }
        (Step::Scan, Event::Primary) => (Step::Link, vec![end_step_1, StartLink]),
        (Step::Scan, Event::NotNow) => (Step::Done, vec![end_step_1]),
        (Step::Scan, Event::Close) => (Step::Over, vec![EndStep1 { answer: false }, Release]),
        (Step::Uploading, Event::NoCredential) => (Step::Link, vec![StartLink]),
        // Linking again after it failed or was cancelled.
        (Step::Link, Event::Primary) => (Step::Link, vec![StartLink]),
        (Step::Link, Event::NotNow) => (Step::Done, vec![]),
        (Step::Link, Event::Linked) => (Step::Uploading, vec![WaitForRuns]),
        (Step::Uploading, Event::RunsEnded) => (Step::Done, vec![]),
        (Step::Link | Step::Uploading | Step::Done, Event::Close) => (Step::Over, vec![Release]),
        (step, _) => (step, vec![]),
    }
}

/// Whether a row can be ticked: it asks CMP-R40's question, with its folder
/// settled. A `logId` in two folders can't be until a folder is picked
/// (CMP-R42).
pub fn can_tick(row: &Row) -> bool {
    matches!(row.line, Some(Line::Awaiting { .. }))
}

/// The answers the ticks become when step 1 ends (CMP-R100): every log
/// awaiting an answer with its folder settled, ticked (by default) **Upload**
/// and unticked **Don't upload**. A `logId` still without a pick stays
/// awaiting (CMP-R42), as does one no folder holds.
pub fn answers(
    state: &State,
    snapshot: &Snapshot,
    unticked: &BTreeSet<String>,
) -> BTreeMap<String, Answer> {
    state
        .logs
        .iter()
        .filter(|(_, log)| log.answer.is_none())
        .filter(|(log_id, log)| {
            catch_logs::decide(log, &Copies::of(snapshot, log_id))
                .folder
                .is_some()
        })
        .map(|(log_id, _)| {
            let answer = if unticked.contains(log_id) {
                Answer::Skip
            } else {
                Answer::Upload
            };
            (log_id.clone(), answer)
        })
        .collect()
}

/// Step 1's end in local state (CMP-R100): the answers, and `firstRunDone`,
/// from which on local state is written.
pub fn end_step_1(state: &mut State, answers: &BTreeMap<String, Answer>) {
    for (log_id, answer) in answers {
        if let Some(log) = state.logs.get_mut(log_id) {
            log.answer = Some(*answer);
        }
    }
    state.first_run_done = true;
}

/// One row of step 1's found list: the Catch logs list's row with its tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TickRow {
    #[serde(flatten)]
    pub row: Row,
    pub ticked: bool,
    pub can_tick: bool,
}

/// Step 1's found list: every Catch log found, ticked by default.
pub fn tick_rows(rows: Vec<Row>, unticked: &BTreeSet<String>) -> Vec<TickRow> {
    rows.into_iter()
        .map(|row| {
            let can_tick = can_tick(&row);
            TickRow {
                ticked: can_tick && !unticked.contains(&row.log_id),
                can_tick,
                row,
            }
        })
        .collect()
}

/// The button that ends step 1 and links (CMP-R100).
pub fn primary_text(credential_stored: bool) -> &'static str {
    if credential_stored {
        "Continue"
    } else {
        "Link to Pagle"
    }
}

/// Step 4's status (CMP-R100): the link ("Linked as …", "Linked" while no
/// BattleTag is known, or "Not linked"), "Last uploaded …" and "Start at
/// login: on" or "off" as set.
pub fn done_status(link: &str, last_uploaded: &str, start_at_login: bool) -> Vec<String> {
    vec![
        link.to_owned(),
        last_uploaded.to_owned(),
        format!(
            "Start at login: {}",
            if start_at_login { "on" } else { "off" }
        ),
    ]
}

/// What the first-run window shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub step: Step,
    /// **Link to Pagle** or **Continue**.
    pub primary: &'static str,
    /// The scan has ended and found no install: the no-install state with
    /// **Add a WoW folder…** (CMP UI, WoW folders).
    pub no_install: bool,
    /// The folders have been read: until then the list is still filling.
    pub read: bool,
    pub logs: Vec<TickRow>,
    /// Step 4's lines.
    pub status: Vec<String>,
}

/// The first run's step and ticks. Reach it with `app.state::<FirstRun>()`.
pub struct FirstRun {
    inner: Mutex<Inner>,
}

struct Inner {
    step: Step,
    /// The `logId`s the Player unticked; every other is ticked.
    unticked: BTreeSet<String>,
    credential_stored: bool,
}

impl FirstRun {
    /// At step 1 on a first run, else over.
    pub fn new(first_run: bool) -> Self {
        Self {
            inner: Mutex::new(Inner {
                step: if first_run { Step::Scan } else { Step::Over },
                unticked: BTreeSet::new(),
                credential_stored: false,
            }),
        }
    }

    pub fn step(&self) -> Step {
        self.lock().step
    }

    /// Whether the first-run window is still open (CMP-R100).
    pub fn is_open(&self) -> bool {
        self.step() != Step::Over
    }

    pub fn credential_stored(&self) -> bool {
        self.lock().credential_stored
    }

    /// Whether a credential was in its store when the first run started.
    pub fn set_credential_stored(&self, stored: bool) {
        self.lock().credential_stored = stored;
    }

    /// Moves to the next step and returns what to do (see [`next`]).
    pub fn handle(&self, event: Event) -> Vec<Effect> {
        let mut inner = self.lock();
        let (step, effects) = next(inner.step, event, inner.credential_stored);
        if step != inner.step {
            log::info!("first run: {:?} -> {step:?}", inner.step);
        }
        inner.step = step;
        effects
    }

    /// Ticks or unticks a row of step 1's list.
    pub fn tick(&self, log_id: &str, ticked: bool) {
        let mut inner = self.lock();
        if ticked {
            inner.unticked.remove(log_id);
        } else {
            inner.unticked.insert(log_id.to_owned());
        }
    }

    pub fn unticked(&self) -> BTreeSet<String> {
        self.lock().unticked.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::pagle_log::{self, Found};
    use crate::readings::fixture::{LOG_A, LOG_B, pagle_lua_text};
    use crate::readings::{FileStamp, FolderReading, LocalRead, Reading};
    use crate::state::{self, LogState, Store};

    const INSTALL: &str = "G:/WoW/_classic_beta_";
    const ONE: &str = "G:/WoW/_classic_beta_/WTF/Account/ADA#1";
    const TWO: &str = "G:/WoW/_classic_beta_/WTF/Account/ADA#2";
    const THREE: &str = "G:/WoW/_classic_beta_/WTF/Account/ADA#3";
    const FOUR: &str = "G:/WoW/_classic_beta_/WTF/Account/ADA#4";
    const LOG_C: &str = "00000000000000cc";

    /// An account folder whose `Pagle.lua` holds a valid local log.
    fn holding(path: &str, log_id: &str) -> FolderReading {
        let Ok(Found::Valid(log)) = pagle_log::parse(pagle_lua_text(log_id, 2).as_bytes()) else {
            panic!("fixture not valid");
        };
        let file = FileStamp {
            mtime_ms: 1_791_735_090_999,
            size: 1,
        };
        FolderReading {
            flavor: INSTALL.into(),
            name: path.rsplit('/').next().unwrap().into(),
            path: path.into(),
            polled: false,
            last_log_id: Some(log_id.into()),
            reading: Reading::Valid(LocalRead {
                log: Arc::new(log),
                as_of: file.as_of(),
                file,
            }),
        }
    }

    /// LOG_A in ONE, LOG_B in TWO, and LOG_C in both THREE and FOUR:
    /// CMP-R42's case.
    fn found() -> Snapshot {
        Snapshot {
            folders: vec![
                holding(ONE, LOG_A),
                holding(TWO, LOG_B),
                holding(THREE, LOG_C),
                holding(FOUR, LOG_C),
            ],
        }
    }

    /// Local state of a first run once the folders are read.
    fn first_run_state(snapshot: &Snapshot) -> State {
        let mut state = State::default();
        catch_logs::remember(&mut state, snapshot);
        state
    }

    fn rows(state: &State, snapshot: &Snapshot, unticked: &BTreeSet<String>) -> Vec<TickRow> {
        let view = catch_logs::view(state, Some(snapshot), &[INSTALL.into()], 0);
        tick_rows(view.logs, unticked)
    }

    fn row<'a>(rows: &'a [TickRow], log_id: &str) -> &'a TickRow {
        rows.iter().find(|r| r.row.log_id == log_id).unwrap()
    }

    #[test]
    fn step_1_lists_every_log_ticked_by_default() {
        // CMP-R100: it lists every Catch log found, with its characters and
        // account folder, each ticked by default
        let snapshot = found();
        let state = first_run_state(&snapshot);
        let rows = rows(&state, &snapshot, &BTreeSet::new());
        assert_eq!(rows.len(), 3);
        let a = row(&rows, LOG_A);
        assert!(a.ticked && a.can_tick);
        assert_eq!(a.row.characters, "Ada Brook");
        assert_eq!(a.row.folder.as_deref(), Some("ADA#1"));
    }

    #[test]
    fn a_log_in_two_folders_shows_the_pick_and_cant_be_ticked() {
        // CMP-R100: a logId in two folders shows CMP-R42's question in the
        // list and can't be ticked until a folder is picked
        let snapshot = found();
        let mut state = first_run_state(&snapshot);
        let c = row(&rows(&state, &snapshot, &BTreeSet::new()), LOG_C).clone();
        assert!(!c.can_tick && !c.ticked);
        assert!(matches!(c.row.line, Some(Line::SeveralFolders { .. })));
        assert_eq!(c.row.choices.len(), 2);

        state.logs.get_mut(LOG_C).unwrap().folder = Some(c.row.choices[1].path.clone());
        let c = row(&rows(&state, &snapshot, &BTreeSet::new()), LOG_C).clone();
        assert!(c.can_tick && c.ticked);
    }

    #[test]
    fn the_ticks_become_the_answers() {
        // CMP-R100: ticked: Upload, unticked: Don't upload; CMP-R42: a logId
        // still without a pick when step 1 ends stays awaiting
        let snapshot = found();
        let state = first_run_state(&snapshot);
        let unticked = BTreeSet::from([LOG_B.to_owned()]);
        assert_eq!(
            answers(&state, &snapshot, &unticked),
            BTreeMap::from([
                (LOG_A.to_owned(), Answer::Upload),
                (LOG_B.to_owned(), Answer::Skip),
            ])
        );
    }

    #[test]
    fn step_1_writes_local_state_only_when_it_ends() {
        // CMP-R100: until step 1 ends, local state stays in memory and
        // nothing of it is written; then the ticks become the answers, local
        // state is written and firstRunDone is set
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        let snapshot = found();
        let store = Store::new(&path, State::default());
        store
            .update(|s| catch_logs::remember(s, &snapshot))
            .unwrap();
        assert!(!path.exists());

        let answers = store.read(|s| answers(s, &snapshot, &BTreeSet::new()));
        store.update(|s| end_step_1(s, &answers)).unwrap();
        let state::Loaded::State(written) = state::load(&path).unwrap() else {
            panic!("step 1's end didn't write state.json");
        };
        assert!(written.first_run_done);
        assert_eq!(written.logs[LOG_A].answer, Some(Answer::Upload));
        assert_eq!(written.logs[LOG_C].answer, None);
    }

    #[test]
    fn closing_early_leaves_every_found_log_awaiting() {
        // CMP-R100: closing the window before step 1 ends writes local state
        // with every found log awaiting (CMP-R40) and firstRunDone set
        let snapshot = found();
        let mut state = first_run_state(&snapshot);
        let (step, effects) = next(Step::Scan, Event::Close, false);
        assert_eq!(step, Step::Over);
        assert_eq!(
            effects,
            [Effect::EndStep1 { answer: false }, Effect::Release]
        );
        end_step_1(&mut state, &BTreeMap::new());
        assert!(state.first_run_done);
        assert!(state.logs.values().all(|log| log.answer.is_none()));
        assert_eq!(state.logs.len(), 3);
    }

    #[test]
    fn link_to_pagle_ends_step_1_then_links_then_uploads_then_is_done() {
        // CMP-R100: Scan, Link, Upload ("Uploading…" until each post-link
        // run has ended), Done with Close, which closes it to the tray
        let (step, effects) = next(Step::Scan, Event::Primary, false);
        assert_eq!(step, Step::Link);
        assert_eq!(
            effects,
            [Effect::EndStep1 { answer: true }, Effect::StartLink]
        );
        let (step, effects) = next(step, Event::Linked, false);
        assert_eq!(
            (step, effects),
            (Step::Uploading, vec![Effect::WaitForRuns])
        );
        let (step, effects) = next(step, Event::RunsEnded, false);
        assert_eq!((step, effects), (Step::Done, vec![]));
        let (step, effects) = next(step, Event::Close, false);
        assert_eq!((step, effects), (Step::Over, vec![Effect::Release]));
    }

    #[test]
    fn continue_skips_linking_and_runs_the_post_link_runs() {
        // CMP-R100: with a credential already in its store, the button reads
        // Continue, step 2 is skipped, and step 3 runs the post-link runs
        assert_eq!(primary_text(true), "Continue");
        assert_eq!(primary_text(false), "Link to Pagle");
        let (step, effects) = next(Step::Scan, Event::Primary, true);
        assert_eq!(step, Step::Uploading);
        assert_eq!(
            effects,
            [
                Effect::EndStep1 { answer: true },
                Effect::ContinueLink,
                Effect::WaitForRuns
            ]
        );
        // The credential was gone by then: link instead.
        let (step, effects) = next(step, Event::NoCredential, true);
        assert_eq!((step, effects), (Step::Link, vec![Effect::StartLink]));
    }

    #[test]
    fn not_now_skips_steps_2_and_3() {
        // CMP-R100: Not now ends step 1 and skips step 2 and 3
        let (step, effects) = next(Step::Scan, Event::NotNow, false);
        assert_eq!(
            (step, effects),
            (Step::Done, vec![Effect::EndStep1 { answer: true }])
        );
        // From a link that failed or was cancelled, too.
        assert_eq!(next(Step::Link, Event::NotNow, false), (Step::Done, vec![]));
    }

    #[test]
    fn step_1_ends_once() {
        // CMP-R100: firstRunDone is set when step 1 ends; a later start is
        // not a first run
        for step in [Step::Link, Step::Uploading, Step::Done, Step::Over] {
            for event in [Event::Primary, Event::NotNow, Event::Close] {
                let (_, effects) = next(step, event, false);
                assert!(
                    !effects.iter().any(|e| matches!(e, Effect::EndStep1 { .. })),
                    "{step:?} {event:?}"
                );
            }
        }
        // Closing the window after step 1 only ends the hold.
        assert_eq!(next(Step::Link, Event::Close, false).1, [Effect::Release]);
        assert_eq!(next(Step::Over, Event::Close, false).1, []);
        // A link stored after the window closed changes nothing here.
        assert_eq!(next(Step::Over, Event::Linked, false), (Step::Over, vec![]));
    }

    #[test]
    fn the_service_keeps_the_step_and_the_ticks() {
        // CMP-R100: each ticked by default; the ticks become the answers
        let first_run = FirstRun::new(true);
        assert!(first_run.is_open());
        first_run.tick(LOG_A, false);
        first_run.tick(LOG_B, false);
        first_run.tick(LOG_B, true);
        assert_eq!(first_run.unticked(), BTreeSet::from([LOG_A.to_owned()]));

        first_run.set_credential_stored(true);
        assert_eq!(first_run.handle(Event::Primary).len(), 3);
        assert_eq!(first_run.step(), Step::Uploading);
        assert!(!FirstRun::new(false).is_open());
    }

    #[test]
    fn done_shows_the_status_as_set() {
        // CMP-R100: the status ("Last uploaded …", "Start at login: on" or
        // "off" as set); "Linked" until a BattleTag is known
        assert_eq!(
            done_status("Linked", "Nothing uploaded yet", true),
            ["Linked", "Nothing uploaded yet", "Start at login: on"]
        );
        assert_eq!(
            done_status("Not linked", "Nothing uploaded yet", false)[2],
            "Start at login: off"
        );
    }

    #[test]
    fn the_window_gets_each_row_with_its_tick() {
        // CMP UI, First run: the found list with ticks
        let snapshot = Snapshot {
            folders: vec![holding(ONE, LOG_A)],
        };
        let state = first_run_state(&snapshot);
        let json = serde_json::to_value(&rows(&state, &snapshot, &BTreeSet::new())[0]).unwrap();
        assert_eq!(json["logId"], LOG_A);
        assert_eq!(json["ticked"], true);
        assert_eq!(json["canTick"], true);
        assert_eq!(json["line"]["kind"], "awaiting");
        let log: &LogState = &state.logs[LOG_A];
        assert_eq!(json["characters"], log.characters.as_str());
    }
}

//! The scan loop (CMP-R14): a thread that scans at start, every rescan
//! interval, when asked to (a uid change, a folder added or removed), and
//! when a watched `Agent/` folder's `product.db` or an install's account
//! folders change, debounced; with polling where watching fails (CMP-R22).
//! Every scan that changes the set of installs is sent to every subscriber.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use serde::Serialize;

use super::watch::{Filter, Watches};
use super::{DEBOUNCE, DbSource, Install, POLL_INTERVAL, RESCAN_INTERVAL, Skipped};

/// What a scan reads besides the file system, asked for at every scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    /// The `product.db` folders to read (CMP-R10).
    pub sources: Vec<DbSource>,
    /// The manual installs kept in local state (CMP-R13).
    pub manual: Vec<String>,
    /// The Forever product uid (CMP-R12).
    pub uid: String,
}

/// The set of installs as of the last scan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// Every install to watch, each flavor folder once, with its account
    /// folders.
    pub installs: Vec<Install>,
    /// Manual installs that no longer qualify: not watched, shown greyed with
    /// "Not found" (CMP-R13).
    pub not_found: Vec<String>,
}

enum Msg {
    Rescan,
    Changed,
}

/// Finds installs and keeps finding them. Reach it with
/// `app.state::<Scanner>()`.
pub struct Scanner {
    tx: Sender<Msg>,
    rx: Mutex<Option<Receiver<Msg>>>,
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    snapshot: Mutex<Option<Snapshot>>,
    subscribers: Mutex<Vec<Sender<Snapshot>>>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    /// A scanner that scans nothing until [`Scanner::start`].
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            shared: Arc::default(),
        }
    }

    /// Starts scanning, on a thread of its own, with `inputs` asked for at
    /// every scan. Only the first call starts anything.
    pub fn start(&self, inputs: impl Fn() -> Inputs + Send + 'static) {
        let Some(rx) = lock(&self.rx).take() else {
            return;
        };
        let tx = self.tx.clone();
        let shared = Arc::clone(&self.shared);
        std::thread::spawn(move || run(rx, tx, &shared, inputs));
    }

    /// The set of installs as of the last scan; `None` before the first scan
    /// has ended.
    pub fn snapshot(&self) -> Option<Snapshot> {
        lock(&self.shared.snapshot).clone()
    }

    /// The installs to watch, as of the last scan (CMP-R15, CMP-R20).
    pub fn installs(&self) -> Vec<Install> {
        self.snapshot().map(|s| s.installs).unwrap_or_default()
    }

    /// A channel that receives the whole new [`Snapshot`] every time a scan
    /// changes it, starting with the current one if a scan has ended. It
    /// stays open as long as the scanner.
    pub fn subscribe(&self) -> Receiver<Snapshot> {
        let (tx, rx) = mpsc::channel();
        // Held across both, so no scan can slip in between.
        let snapshot = lock(&self.shared.snapshot);
        if let Some(snapshot) = snapshot.as_ref() {
            let _ = tx.send(snapshot.clone());
        }
        lock(&self.shared.subscribers).push(tx);
        rx
    }

    /// Asks for a scan now: when the Forever product uid changes, or a folder
    /// is added or removed (CMP-R14). Before [`Scanner::start`], the scan
    /// waits for it.
    pub fn rescan(&self) {
        let _ = self.tx.send(Msg::Rescan);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn run(rx: Receiver<Msg>, tx: Sender<Msg>, shared: &Shared, inputs: impl Fn() -> Inputs) {
    let mut watches = Watches::new(move |_dir| {
        let _ = tx.send(Msg::Changed);
    });
    let mut logged_skips = HashSet::new();
    let now = Instant::now();
    let mut scan_at = now;
    let mut poll_at = now + POLL_INTERVAL;
    loop {
        let now = Instant::now();
        if scan_at <= now {
            scan_once(&inputs(), &mut watches, shared, &mut logged_skips);
            scan_at = Instant::now() + RESCAN_INTERVAL;
        }
        if watches.has_polled() && poll_at <= now {
            if !watches.poll().is_empty() {
                scan_at = scan_at.min(now);
                continue;
            }
            poll_at = now + POLL_INTERVAL;
        }
        let deadline = if watches.has_polled() {
            scan_at.min(poll_at)
        } else {
            scan_at
        };
        match rx.recv_timeout(deadline.saturating_duration_since(now)) {
            Ok(Msg::Rescan) => scan_at = Instant::now(),
            // A scan once events have stopped for the debounce time.
            Ok(Msg::Changed) => scan_at = Instant::now() + DEBOUNCE,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Runs one scan, watches what it found, and publishes it if it changed.
fn scan_once(
    inputs: &Inputs,
    watches: &mut Watches,
    shared: &Shared,
    logged_skips: &mut HashSet<PathBuf>,
) {
    let scan = super::scan(&inputs.sources, &inputs.manual, &inputs.uid);
    note_skipped(&scan.skipped, logged_skips);

    let product_db = Filter::Name("product.db".to_owned());
    let mut wanted: Vec<(PathBuf, Filter)> = inputs
        .sources
        .iter()
        .map(|source| (source.agent_dir.clone(), product_db.clone()))
        .collect();
    let account_dirs: Vec<PathBuf> = scan
        .installs
        .iter()
        .map(|install| super::accounts_watch_dir(Path::new(&install.flavor)))
        .collect();
    wanted.extend(
        account_dirs
            .iter()
            .map(|dir| (dir.clone(), Filter::Entries)),
    );
    watches.set(&wanted);

    let mut installs = scan.installs;
    for (install, dir) in installs.iter_mut().zip(&account_dirs) {
        install.accounts_polled = watches.is_polled(dir);
    }
    publish(
        shared,
        Snapshot {
            installs,
            not_found: scan.not_found,
        },
    );
}

/// Notes each `product.db` that couldn't be read or decoded in the local log
/// file (CMP-R11), once until it reads again.
fn note_skipped(skipped: &[Skipped], logged: &mut HashSet<PathBuf>) {
    logged.retain(|path| skipped.iter().any(|s| &s.path == path));
    for skip in skipped {
        if logged.insert(skip.path.clone()) {
            log::warn!("skipped {}: {}", skip.path.display(), skip.reason);
        }
    }
}

fn publish(shared: &Shared, snapshot: Snapshot) {
    let mut current = lock(&shared.snapshot);
    if current.as_ref() == Some(&snapshot) {
        return;
    }
    log::info!(
        "scan found {} install(s){}",
        snapshot.installs.len(),
        snapshot
            .installs
            .iter()
            .map(|i| format!(", {} with {} account folder(s)", i.flavor, i.accounts.len()))
            .collect::<String>()
    );
    for missing in &snapshot.not_found {
        log::info!("added folder {missing} is no longer a WoW Forever install");
    }
    *current = Some(snapshot.clone());
    lock(&shared.subscribers).retain(|tx| tx.send(snapshot.clone()).is_ok());
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use super::super::fixture::*;
    use super::super::{BUILT_IN_FOREVER_PRODUCT_UID as UID, paths};
    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    fn wait_for(rx: &Receiver<Snapshot>, done: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let until = Instant::now() + WAIT;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let snapshot = rx.recv_timeout(left).expect("no matching snapshot in time");
            if done(&snapshot) {
                return snapshot;
            }
        }
    }

    fn account_names(snapshot: &Snapshot) -> Vec<String> {
        snapshot
            .installs
            .iter()
            .flat_map(|i| i.accounts.iter().map(|a| a.name.clone()))
            .collect()
    }

    #[test]
    fn a_scan_runs_at_start_and_subscribers_get_its_installs() {
        // CMP-R14: a scan runs at start
        let tmp = tempfile::tempdir().unwrap();
        let beta = flavor(tmp.path(), "WoW/_classic_beta_", UID);
        let manual = vec![paths::canonical(&beta).unwrap()];
        let scanner = Scanner::new();
        let rx = scanner.subscribe();
        assert_eq!(scanner.snapshot(), None);

        scanner.start(move || Inputs {
            sources: vec![],
            manual: manual.clone(),
            uid: UID.into(),
        });
        let snapshot = wait_for(&rx, |_| true);
        assert_eq!(snapshot.installs.len(), 1);
        assert_eq!(scanner.installs(), snapshot.installs);
        // A later subscriber starts with the current set.
        assert_eq!(scanner.subscribe().recv_timeout(WAIT).unwrap(), snapshot);
    }

    #[test]
    fn account_folders_that_appear_or_go_are_noticed() {
        // CMP-R15: the Companion watches WTF/Account/ for account folders
        // that appear or go, debounced (CMP-R14)
        let tmp = tempfile::tempdir().unwrap();
        let beta = flavor(tmp.path(), "WoW/_classic_beta_", UID);
        mkdirs(&beta, "WTF/Account/1#1");
        let manual = vec![paths::canonical(&beta).unwrap()];
        let scanner = Scanner::new();
        let rx = scanner.subscribe();
        scanner.start(move || Inputs {
            sources: vec![],
            manual: manual.clone(),
            uid: UID.into(),
        });
        wait_for(&rx, |s| account_names(s) == ["1#1"]);

        mkdirs(&beta, "WTF/Account/ADA#2");
        wait_for(&rx, |s| account_names(s) == ["1#1", "ADA#2"]);
        fs::remove_dir(beta.join("WTF/Account/1#1")).unwrap();
        wait_for(&rx, |s| account_names(s) == ["ADA#2"]);
    }

    #[test]
    fn a_changed_product_db_starts_a_scan() {
        // CMP-R14: a scan runs when any product.db the Companion knows of
        // changes, noticed by watching its Agent/ folder
        let tmp = tempfile::tempdir().unwrap();
        let root = mkdirs(tmp.path(), "WoW");
        let beta = flavor(&root, "_classic_beta_", UID);
        let agent = mkdirs(tmp.path(), "ProgramData/Battle.net/Agent");
        let source = DbSource {
            agent_dir: agent.clone(),
            prefix: None,
        };
        let scanner = Scanner::new();
        let rx = scanner.subscribe();
        scanner.start(move || Inputs {
            sources: vec![source.clone()],
            manual: vec![],
            uid: UID.into(),
        });
        wait_for(&rx, |s| s.installs.is_empty());

        product_db(&agent, &[(&location(&root), "_classic_beta_")]);
        let found = wait_for(&rx, |s| !s.installs.is_empty());
        assert_eq!(found.installs[0].flavor, paths::canonical(&beta).unwrap());
    }

    #[test]
    fn a_rescan_picks_up_a_changed_input() {
        // CMP-R14: a scan runs when a folder is added or removed, or the uid
        // changes
        let tmp = tempfile::tempdir().unwrap();
        let beta = flavor(tmp.path(), "WoW/_classic_beta_", UID);
        let manual = Arc::new(Mutex::new(Vec::<String>::new()));
        let scanner = Scanner::new();
        let rx = scanner.subscribe();
        let inputs = Arc::clone(&manual);
        scanner.start(move || Inputs {
            sources: vec![],
            manual: lock(&inputs).clone(),
            uid: UID.into(),
        });
        wait_for(&rx, |s| s.installs.is_empty());

        lock(&manual).push(paths::canonical(&beta).unwrap());
        scanner.rescan();
        wait_for(&rx, |s| s.installs.len() == 1);
    }

    #[test]
    fn a_skipped_product_db_is_noted_once_until_it_reads_again() {
        // CMP-R11: a product.db that can't be read or decoded is skipped and
        // noted in the local log file
        let skip = Skipped {
            path: PathBuf::from("C:/ProgramData/Battle.net/Agent/product.db"),
            reason: "a field runs past the end".into(),
        };
        let mut logged = HashSet::new();
        note_skipped(std::slice::from_ref(&skip), &mut logged);
        assert!(logged.contains(&skip.path));
        note_skipped(&[], &mut logged);
        assert!(logged.is_empty());
    }
}

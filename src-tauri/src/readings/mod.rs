//! Watching every account folder's `SavedVariables/` and reading its
//! `Pagle.lua` after each save (CMP-R20 to CMP-R23), into a local log with
//! the as-of time its Uploads carry (CMP-R30 to CMP-R32).
//!
//! [`Readings`] follows the scanner's installs: at every scan it reads each
//! account folder's file (CMP-R23), watches each `SavedVariables/` (or the
//! account folder until it appears) for `Pagle.lua`, debounced, polls where
//! watching fails (CMP-R22), and tells its subscribers what each folder
//! holds. It asks nothing and uploads nothing: confirm-once (CMP-R40) and
//! upload runs (CMP-R45) subscribe here, and a run reads its file when it
//! starts with [`Readings::read_now`] (CMP-R46).

pub mod read;
pub mod window;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::installs::watch::{Filter, Watches};
use crate::installs::{self, DEBOUNCE, POLL_INTERVAL, paths};

pub use read::{FileStamp, LocalRead, READ_ATTEMPTS, READ_RETRY_DELAY, Reading};

/// What an account folder shows after the last read attempt failed (CMP-R30).
pub const COULDNT_READ: &str =
    "Couldn't read Pagle.lua. Pagle tries again after your next /reload or logout.";

/// What an account folder shows whose `Pagle.lua` holds an invalid local log
/// (CMP-R31).
pub const CANT_UPLOAD: &str =
    "Pagle.lua holds a Catch log this Companion can't upload. Update Pagle Companion.";

/// One account folder and what its `Pagle.lua` held at the last read.
#[derive(Debug, Clone, PartialEq)]
pub struct FolderReading {
    /// The install's flavor folder, canonical.
    pub flavor: String,
    /// The account folder's name, shown and never used to decide anything
    /// (CMP-R16).
    pub name: String,
    /// The account folder, canonical (CMP-R17): what `folder` and
    /// `lastFolder` of local state hold.
    pub path: String,
    /// Its `SavedVariables/` (or, until that appears, the account folder)
    /// can't be watched and is checked every poll interval instead (CMP-R22).
    pub polled: bool,
    pub reading: Reading,
    /// The `logId` the folder held at its last read that could tell, kept
    /// through a failed read, for the row that shows CMP-R30's text (CMP-R30:
    /// "on the row of the `logId` it last held, if any").
    pub last_log_id: Option<String>,
}

impl FolderReading {
    /// CMP-R30's or CMP-R31's text, when the folder has a read problem.
    pub fn problem(&self) -> Option<&'static str> {
        match self.reading {
            Reading::Unreadable => Some(COULDNT_READ),
            Reading::Invalid { .. } => Some(CANT_UPLOAD),
            Reading::NoCatchLog { .. } | Reading::Valid(_) => None,
        }
    }

    /// The valid local log the folder holds, if it has entries: an empty one
    /// isn't offered, counted as a copy or uploaded (CMP-R32).
    pub fn catch_log(&self) -> Option<&LocalRead> {
        match &self.reading {
            Reading::Valid(read) if !read.log.is_empty() => Some(read),
            _ => None,
        }
    }
}

/// What every watched account folder held at its last read, in the
/// scanner's order: installs, then account folders by name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub folders: Vec<FolderReading>,
}

impl Snapshot {
    /// The account folder at `path`, compared as CMP-R14 compares folders.
    pub fn folder(&self, path: &str) -> Option<&FolderReading> {
        let key = paths::same_folder_key(path);
        self.folders
            .iter()
            .find(|f| paths::same_folder_key(&f.path) == key)
    }

    /// Every `logId` a valid local log with entries holds, with the account
    /// folders holding it, in folder order: more than one is CMP-R42's case.
    /// Empty logs are left out (CMP-R32).
    pub fn catch_logs(&self) -> BTreeMap<String, Vec<&FolderReading>> {
        let mut logs: BTreeMap<String, Vec<&FolderReading>> = BTreeMap::new();
        for folder in &self.folders {
            if let Some(read) = folder.catch_log() {
                logs.entry(read.log.log_id.clone())
                    .or_default()
                    .push(folder);
            }
        }
        logs
    }
}

/// What a subscriber receives: the whole new [`Snapshot`], and why.
#[derive(Debug, Clone)]
pub struct Update {
    pub snapshot: Snapshot,
    /// The account folders whose `Pagle.lua` was read anew for this update,
    /// after a debounced change, a poll or a scan, and not skipped by
    /// CMP-R21: each is CMP-R45's "debounced change of its watched file".
    /// [`Readings::read_now`]'s reads never appear here.
    pub read: Vec<String>,
    /// This update ends a scan's reads (CMP-R23), sent after every scan
    /// whether anything changed or not: CMP-R45's "after each later scan".
    pub scan: bool,
}

/// The timings [`Readings`] runs with: the CMP Tunables, except in tests.
#[derive(Debug, Clone)]
pub struct Config {
    pub debounce: Duration,
    pub poll_interval: Duration,
    pub read_attempts: u32,
    pub read_retry_delay: Duration,
    /// Whether to ask the OS to watch folders; `false` polls them all, as
    /// where every watch is refused (CMP-R22).
    pub watch: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            debounce: DEBOUNCE,
            poll_interval: POLL_INTERVAL,
            read_attempts: READ_ATTEMPTS,
            read_retry_delay: READ_RETRY_DELAY,
            watch: true,
        }
    }
}

enum Msg {
    Scan(installs::Snapshot),
    Changed(PathBuf),
}

/// Watches and reads every account folder's `Pagle.lua`. Reach it with
/// `app.state::<Readings>()`.
pub struct Readings {
    tx: Sender<Msg>,
    rx: Mutex<Option<Receiver<Msg>>>,
    shared: Arc<Shared>,
}

struct Shared {
    config: Config,
    inner: Mutex<Inner>,
    subscribers: Mutex<Vec<Sender<Update>>>,
    /// How many times a file was read (CMP-R30's reads, not attempts).
    file_reads: AtomicU64,
    /// How many folders' debounce times have ended (CMP-R21).
    debounced: AtomicU64,
}

#[derive(Default)]
struct Inner {
    /// `None` until the first scan's reads have ended.
    snapshot: Option<Snapshot>,
    /// Per account folder (by [`paths::same_folder_key`]), the stamp of its
    /// last successful read: `None` for no file (CMP-R21).
    last_ok: HashMap<String, Option<FileStamp>>,
}

impl Default for Readings {
    fn default() -> Self {
        Self::new()
    }
}

impl Readings {
    /// Reads nothing until [`Readings::start`].
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    pub fn with_config(config: Config) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            shared: Arc::new(Shared {
                config,
                inner: Mutex::default(),
                subscribers: Mutex::default(),
                file_reads: AtomicU64::new(0),
                debounced: AtomicU64::new(0),
            }),
        }
    }

    /// Starts watching and reading, on a thread of its own, the account
    /// folders of every snapshot `scans` brings: one per scan, as
    /// [`installs::Scanner::subscribe_scans`] gives them. Only the first call
    /// starts anything.
    pub fn start(&self, scans: Receiver<installs::Snapshot>) {
        let Some(rx) = lock(&self.rx).take() else {
            return;
        };
        let forward = self.tx.clone();
        std::thread::spawn(move || {
            for snapshot in scans {
                if forward.send(Msg::Scan(snapshot)).is_err() {
                    return;
                }
            }
        });
        let tx = self.tx.clone();
        let shared = Arc::clone(&self.shared);
        std::thread::spawn(move || run(&rx, tx, &shared));
    }

    /// What every account folder held at its last read; `None` before the
    /// first scan's reads have ended.
    pub fn snapshot(&self) -> Option<Snapshot> {
        lock(&self.shared.inner).snapshot.clone()
    }

    /// A channel that receives an [`Update`] every time a read or a scan
    /// changes the snapshot, and after every scan; starting with the current
    /// snapshot (no reads, not a scan) if the first scan's reads have ended.
    /// It stays open as long as `self`.
    pub fn subscribe(&self) -> Receiver<Update> {
        let (tx, rx) = mpsc::channel();
        // Held across both, so no update can slip in between.
        let inner = lock(&self.shared.inner);
        if let Some(snapshot) = &inner.snapshot {
            let _ = tx.send(Update {
                snapshot: snapshot.clone(),
                read: Vec::new(),
                scan: false,
            });
        }
        lock(&self.shared.subscribers).push(tx);
        rx
    }

    /// Reads the account folder's `Pagle.lua` now, as CMP-R30 says: what an
    /// upload run does when it starts (CMP-R46), and **Upload now** (CMP-R23).
    /// Every Upload reads actual bytes: modification time and size cannot
    /// establish that character metadata or the accepted prefix is unchanged.
    /// This blocks for the read, up to the read attempts
    /// 2 s apart. A watched folder's new reading reaches subscribers without
    /// counting as a change ([`Update::read`] stays empty), so it never
    /// triggers a run by itself.
    pub fn read_now(&self, folder: &str) -> Reading {
        let key = paths::same_folder_key(folder);
        let path = Path::new(folder);
        let reading = self.shared.read_file(path);
        let mut inner = lock(&self.shared.inner);
        if let Some(mut snapshot) = inner.snapshot.clone()
            && let Some(at) = snapshot
                .folders
                .iter()
                .position(|f| paths::same_folder_key(&f.path) == key)
        {
            let folder = &mut snapshot.folders[at];
            let changed = folder.reading != reading;
            set_reading(folder, reading.clone());
            if let Some(stamp) = reading.stamp() {
                inner.last_ok.insert(key, stamp);
            }
            if changed {
                inner.snapshot = Some(snapshot);
                self.shared.publish(&inner, Vec::new(), false);
            }
        }
        reading
    }

    #[cfg(test)]
    fn file_reads(&self) -> u64 {
        self.shared.file_reads.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn debounced(&self) -> u64 {
        self.shared.debounced.load(Ordering::SeqCst)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Takes a new reading into a folder, keeping the `logId` it last held
/// through a failed read.
fn set_reading(folder: &mut FolderReading, reading: Reading) {
    if !matches!(reading, Reading::Unreadable) {
        folder.last_log_id = reading.log_id().map(str::to_owned);
    }
    folder.reading = reading;
}

impl Shared {
    /// Whether a read of the folder would do nothing more, its file's stamp
    /// being that of its last successful read (CMP-R21).
    fn unchanged(&self, key: &str, path: &Path) -> bool {
        let Ok(stamp) = read::current_stamp(path) else {
            return false;
        };
        lock(&self.inner).last_ok.get(key) == Some(&stamp)
    }

    fn read_file(&self, account_folder: &Path) -> Reading {
        self.file_reads.fetch_add(1, Ordering::SeqCst);
        let reading = read::read_with(
            account_folder,
            self.config.read_attempts,
            self.config.read_retry_delay,
            &mut || {},
        );
        match &reading {
            Reading::Valid(read) => log::info!(
                "read {}: Catch log {} with {} entries, as of {}",
                account_folder.display(),
                read.log.log_id,
                read.log.len(),
                read.as_of
            ),
            Reading::NoCatchLog { .. } => {
                log::info!("read {}: no Catch log", account_folder.display());
            }
            // Logged by the read.
            Reading::Invalid { .. } | Reading::Unreadable => {}
        }
        reading
    }

    /// Sends the current snapshot to every subscriber. Called with `inner`
    /// held, so subscribers get updates in order.
    fn publish(&self, inner: &Inner, read: Vec<String>, scan: bool) {
        let Some(snapshot) = &inner.snapshot else {
            return;
        };
        let update = Update {
            snapshot: snapshot.clone(),
            read,
            scan,
        };
        lock(&self.subscribers).retain(|tx| tx.send(update.clone()).is_ok());
    }
}

/// One watched account folder.
struct Watched {
    key: String,
    flavor: String,
    name: String,
    path: String,
    /// `SavedVariables/`, or the account folder until that exists (CMP-R20).
    watch_dir: PathBuf,
    /// Whether `watch_dir` is `SavedVariables/`.
    watching_saved_variables: bool,
}

impl Watched {
    fn of(flavor: &str, account: &installs::AccountFolder) -> Self {
        let mut watched = Self {
            key: paths::same_folder_key(&account.path),
            flavor: flavor.to_owned(),
            name: account.name.clone(),
            path: account.path.clone(),
            watch_dir: PathBuf::new(),
            watching_saved_variables: false,
        };
        watched.find_watch_dir();
        watched
    }

    /// Watches `SavedVariables/` once it exists, and the account folder for
    /// it to appear until then (CMP-R20).
    fn find_watch_dir(&mut self) {
        let account = Path::new(&self.path);
        (self.watch_dir, self.watching_saved_variables) =
            match paths::lookup(account, "SavedVariables") {
                Some(dir) if dir.is_dir() => (dir, true),
                _ => (account.to_path_buf(), false),
            };
    }

    /// Events for any other name are ignored (CMP-R20).
    fn filter(&self) -> Filter {
        Filter::Name(if self.watching_saved_variables {
            "Pagle.lua".to_owned()
        } else {
            "SavedVariables".to_owned()
        })
    }
}

/// The loop: reads at every scan, after each debounced change, and when a
/// polled folder changes.
fn run(rx: &Receiver<Msg>, tx: Sender<Msg>, shared: &Shared) {
    let config = &shared.config;
    let mut watches = if config.watch {
        Watches::new(move |dir| {
            let _ = tx.send(Msg::Changed(dir));
        })
    } else {
        Watches::polling_only()
    };
    let mut folders: Vec<Watched> = Vec::new();
    // Per folder key, when its debounce ends.
    let mut due: HashMap<String, Instant> = HashMap::new();
    let mut poll_at = Instant::now() + config.poll_interval;
    loop {
        let now = Instant::now();
        let ready: Vec<String> = due
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        if !ready.is_empty() {
            due.retain(|_, at| *at > now);
            shared
                .debounced
                .fetch_add(ready.len() as u64, Ordering::SeqCst);
            rewatch(&mut folders, &mut watches);
            read_folders(shared, &folders, &watches, &ready, false);
        }
        if watches.has_polled() && poll_at <= now {
            // A change counts as an event (CMP-R22).
            for dir in watches.poll() {
                for folder in folders.iter().filter(|f| f.watch_dir == dir) {
                    due.insert(folder.key.clone(), now + config.debounce);
                }
            }
            poll_at = now + config.poll_interval;
        }

        let mut deadline = due.values().min().copied();
        if watches.has_polled() {
            deadline = Some(deadline.map_or(poll_at, |d| d.min(poll_at)));
        }
        let msg = match deadline {
            Some(deadline) => rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match msg {
            Ok(Msg::Scan(snapshot)) => {
                folders = snapshot
                    .installs
                    .iter()
                    .flat_map(|install| {
                        install
                            .accounts
                            .iter()
                            .map(|account| Watched::of(&install.flavor, account))
                    })
                    .collect();
                due.retain(|key, _| folders.iter().any(|f| &f.key == key));
                rewatch(&mut folders, &mut watches);
                let all: Vec<String> = folders.iter().map(|f| f.key.clone()).collect();
                read_folders(shared, &folders, &watches, &all, true);
            }
            Ok(Msg::Changed(dir)) => {
                let mut appeared = false;
                for folder in folders.iter().filter(|f| f.watch_dir == dir) {
                    // A read once events have stopped for the debounce time
                    // (CMP-R21).
                    due.insert(folder.key.clone(), Instant::now() + config.debounce);
                    appeared |= !folder.watching_saved_variables;
                }
                if appeared {
                    // Watch `SavedVariables/` as soon as it appears.
                    rewatch(&mut folders, &mut watches);
                    read_folders(shared, &folders, &watches, &[], false);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Makes the watched set each folder's current watch dir.
fn rewatch(folders: &mut [Watched], watches: &mut Watches) {
    for folder in folders.iter_mut() {
        folder.find_watch_dir();
    }
    let wanted: Vec<(PathBuf, Filter)> = folders
        .iter()
        .map(|f| (f.watch_dir.clone(), f.filter()))
        .collect();
    watches.set(&wanted);
}

/// Reads the folders of `keys` whose file changed since their last
/// successful read (CMP-R21), takes the results and the polled flags into
/// the snapshot, and tells subscribers when anything changed, or always at
/// the end of a scan.
fn read_folders(
    shared: &Shared,
    folders: &[Watched],
    watches: &Watches,
    keys: &[String],
    scan: bool,
) {
    let mut fresh: HashMap<&str, Reading> = HashMap::new();
    let mut read = Vec::new();
    for folder in folders.iter().filter(|f| keys.contains(&f.key)) {
        if shared.unchanged(&folder.key, Path::new(&folder.path)) {
            continue;
        }
        fresh.insert(&folder.key, shared.read_file(Path::new(&folder.path)));
        read.push(folder.path.clone());
    }

    let mut inner = lock(&shared.inner);
    let previous = inner.snapshot.take();
    let first = previous.is_none();
    let old = previous.unwrap_or_default();
    let mut snapshot = Snapshot::default();
    for folder in folders {
        let previous = old
            .folders
            .iter()
            .find(|f| paths::same_folder_key(&f.path) == folder.key);
        let mut reading = FolderReading {
            flavor: folder.flavor.clone(),
            name: folder.name.clone(),
            path: folder.path.clone(),
            polled: watches.is_polled(&folder.watch_dir),
            reading: Reading::NoCatchLog { file: None },
            last_log_id: None,
        };
        if let Some(previous) = previous {
            reading.reading = previous.reading.clone();
            reading.last_log_id = previous.last_log_id.clone();
        }
        if let Some(new) = fresh.remove(folder.key.as_str()) {
            if let Some(stamp) = new.stamp() {
                inner.last_ok.insert(folder.key.clone(), stamp);
            }
            set_reading(&mut reading, new);
        }
        snapshot.folders.push(reading);
    }
    inner
        .last_ok
        .retain(|key, _| folders.iter().any(|f| &f.key == key));
    let changed = snapshot != old;
    inner.snapshot = Some(snapshot);
    if changed || scan || first || !read.is_empty() {
        shared.publish(&inner, read, scan);
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    //! Account folders and `Pagle.lua` files, saved the way the game saves
    //! them.

    use std::fs;
    use std::path::{Path, PathBuf};

    /// A `logId` (LOG-R15).
    pub const LOG_A: &str = "3f9a2c1e7b0d4a65";
    pub const LOG_B: &str = "0123456789abcdef";

    /// An account folder `<base>/WoW/_classic_beta_/WTF/Account/<name>`.
    pub fn account_folder(base: &Path, name: &str) -> PathBuf {
        let dir = base.join("WoW/_classic_beta_/WTF/Account").join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A `Pagle.lua` with a `PagleDB`, and a `PagleLog` holding one
    /// character and `entries` entries, in the shapes of `pagle_log`'s tests.
    pub fn pagle_lua_text(log_id: &str, entries: u64) -> String {
        let entries: Vec<String> = (0..entries)
            .map(|i| format!("\"S1|1|{}|0|87|150|25\"", 1_791_735_050 + i))
            .collect();
        format!(
            "PagleDB = {{ [\"global\"] = {{ [\"decodeErrors\"] = 0 }} }}\n\
             PagleLog = {{\n  [\"format\"] = 1,\n  [\"logId\"] = \"{log_id}\",\n  \
             [\"characters\"] = {{ {{ [\"guid\"] = \"Player-4618-0A1B2C3D\", [\"name\"] = \"Ada\", \
             [\"surname\"] = \"Brook\", [\"classID\"] = 1, [\"raceID\"] = 1, \
             [\"faction\"] = \"Alliance\", [\"realmID\"] = 4618, [\"hardcore\"] = false, \
             [\"firstSeen\"] = 1791731680, [\"lastSeen\"] = 1791735090 }} }},\n  \
             [\"entries\"] = {{ {} }},\n}}\n",
            entries.join(", ")
        )
    }

    /// Saves `name` in the account folder's `SavedVariables/` as the game
    /// does: writes `<name>.tmp`, moves the old file to `<name>.bak`, and
    /// renames the new one into place.
    pub fn save_file(account: &Path, name: &str, text: &str) {
        let saved_variables = account.join("SavedVariables");
        fs::create_dir_all(&saved_variables).unwrap();
        let file = saved_variables.join(name);
        let tmp = saved_variables.join(format!("{name}.tmp"));
        fs::write(&tmp, text).unwrap();
        if file.exists() {
            fs::rename(&file, saved_variables.join(format!("{name}.bak"))).unwrap();
        }
        fs::rename(&tmp, &file).unwrap();
    }

    /// Saves `Pagle.lua` as the game does.
    pub fn save(account: &Path, text: &str) {
        save_file(account, "Pagle.lua", text);
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use crate::installs::{AccountFolder, Install, Source};
    use std::fs;

    const WAIT: Duration = Duration::from_secs(15);

    fn canon(path: &Path) -> String {
        paths::canonical(path).unwrap()
    }

    /// One install holding `accounts`, as a scan finds it.
    fn scanned(accounts: &[&Path]) -> installs::Snapshot {
        let flavor = accounts
            .first()
            .map(|a| canon(&a.join("../../..")))
            .unwrap_or_default();
        installs::Snapshot {
            installs: vec![Install {
                root: flavor.clone(),
                flavor,
                source: Source::Manual,
                accounts: accounts
                    .iter()
                    .map(|a| AccountFolder {
                        name: a.file_name().unwrap().to_string_lossy().into_owned(),
                        path: canon(a),
                    })
                    .collect(),
                accounts_polled: false,
            }],
            not_found: vec![],
        }
    }

    struct Running {
        readings: Readings,
        updates: Receiver<Update>,
        scans: Sender<installs::Snapshot>,
    }

    fn start(config: Config, accounts: &[&Path]) -> Running {
        let readings = Readings::with_config(config);
        let updates = readings.subscribe();
        let (scans, scans_rx) = mpsc::channel();
        readings.start(scans_rx);
        scans.send(scanned(accounts)).unwrap();
        let running = Running {
            readings,
            updates,
            scans,
        };
        wait_for(&running.updates, |u| u.scan);
        running
    }

    fn wait_for(rx: &Receiver<Update>, done: impl Fn(&Update) -> bool) -> Update {
        let until = Instant::now() + WAIT;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let update = rx.recv_timeout(left).expect("no matching update in time");
            if done(&update) {
                return update;
            }
        }
    }

    fn entries(snapshot: &Snapshot, account: &Path) -> Option<u64> {
        match &snapshot.folder(&canon(account))?.reading {
            Reading::Valid(read) => Some(read.log.len()),
            _ => None,
        }
    }

    fn quiet(rx: &Receiver<Update>, wait: Duration) -> bool {
        std::thread::sleep(wait);
        rx.try_iter().all(|u| u.read.is_empty())
    }

    #[test]
    fn the_window_texts_are_cmp_r30s_and_cmp_r31s() {
        // CMP-R30, CMP-R31: the texts an account folder shows
        assert_eq!(
            COULDNT_READ,
            "Couldn't read Pagle.lua. Pagle tries again after your next /reload or logout."
        );
        assert_eq!(
            CANT_UPLOAD,
            "Pagle.lua holds a Catch log this Companion can't upload. Update Pagle Companion."
        );
    }

    #[test]
    fn the_defaults_are_the_cmp_tunables() {
        // CMP Tunables: debounce 1 s, poll interval 30 s, 3 read attempts 2 s
        // apart
        let config = Config::default();
        assert_eq!(config.debounce, Duration::from_secs(1));
        assert_eq!(config.poll_interval, Duration::from_secs(30));
        assert_eq!(config.read_attempts, 3);
        assert_eq!(config.read_retry_delay, Duration::from_secs(2));
        assert!(config.watch);
    }

    #[test]
    fn a_scan_reads_every_account_folder() {
        // CMP-R23: watched files are read at every scan
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        let bo = account_folder(tmp.path(), "BO#1");
        save(&ada, &pagle_lua_text(LOG_A, 2));

        let running = start(Config::default(), &[&ada, &bo]);
        let snapshot = running.readings.snapshot().unwrap();
        assert_eq!(entries(&snapshot, &ada), Some(2));
        let bo_reading = snapshot.folder(&canon(&bo)).unwrap();
        assert_eq!(bo_reading.reading, Reading::NoCatchLog { file: None });
        assert_eq!(bo_reading.name, "BO#1");
        assert_eq!(running.readings.file_reads(), 2);
    }

    #[test]
    fn a_save_by_rename_triggers_one_read_a_second_after_the_last_event() {
        // CMP-R20, CMP-R21: the directory is watched, so replacing Pagle.lua
        // by rename (the old one moved to Pagle.lua.bak) triggers one read,
        // once the debounce time has passed with no further event
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        save(&ada, &pagle_lua_text(LOG_A, 1));
        let running = start(Config::default(), &[&ada]);
        let reads = running.readings.file_reads();

        save(&ada, &pagle_lua_text(LOG_A, 2));
        let saved = Instant::now();
        let update = wait_for(&running.updates, |u| !u.read.is_empty());
        assert!(
            saved.elapsed() >= DEBOUNCE,
            "read after {:?}",
            saved.elapsed()
        );
        assert_eq!(update.read, [canon(&ada)]);
        assert!(!update.scan);
        assert_eq!(entries(&update.snapshot, &ada), Some(2));
        assert!(ada.join("SavedVariables/Pagle.lua.bak").exists());

        assert!(quiet(&running.updates, Duration::from_millis(1500)));
        assert_eq!(running.readings.file_reads(), reads + 1);
    }

    #[test]
    fn a_burst_of_five_saves_triggers_one_read() {
        // CMP-R21: per folder, the read waits until the debounce time has
        // passed with no further event
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        save(&ada, &pagle_lua_text(LOG_A, 1));
        let running = start(Config::default(), &[&ada]);
        let reads = running.readings.file_reads();

        for n in 2..=6 {
            save(&ada, &pagle_lua_text(LOG_A, n));
            std::thread::sleep(Duration::from_millis(100));
        }
        let update = wait_for(&running.updates, |u| !u.read.is_empty());
        assert_eq!(entries(&update.snapshot, &ada), Some(6));
        assert!(quiet(&running.updates, Duration::from_millis(1500)));
        assert_eq!(running.readings.debounced(), 1);
        assert_eq!(running.readings.file_reads(), reads + 1);
    }

    #[test]
    fn a_change_to_another_file_triggers_nothing() {
        // CMP-R20: events for any name other than Pagle.lua are ignored
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        save(&ada, &pagle_lua_text(LOG_A, 1));
        let running = start(Config::default(), &[&ada]);
        let reads = running.readings.file_reads();

        save_file(&ada, "PagleDB.lua", "PagleDB = { a = 1 }\n");
        save_file(&ada, "PagleDB.lua", "PagleDB = { a = 2 }\n");
        assert!(quiet(&running.updates, Duration::from_millis(2500)));
        assert_eq!(running.readings.debounced(), 0);
        assert_eq!(running.readings.file_reads(), reads);
    }

    #[test]
    fn a_saved_variables_folder_created_after_start_is_watched() {
        // CMP-R20: while SavedVariables/ doesn't exist, its nearest existing
        // parent is watched until it appears
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        let running = start(Config::default(), &[&ada]);
        assert_eq!(
            running.readings.snapshot().unwrap().folders[0].reading,
            Reading::NoCatchLog { file: None }
        );

        save(&ada, &pagle_lua_text(LOG_A, 1));
        let update = wait_for(&running.updates, |u| entries(&u.snapshot, &ada).is_some());
        assert_eq!(update.read, [canon(&ada)]);

        // And it is SavedVariables/ that is watched from then on.
        save(&ada, &pagle_lua_text(LOG_A, 3));
        wait_for(&running.updates, |u| entries(&u.snapshot, &ada) == Some(3));
    }

    #[test]
    fn a_read_of_an_unchanged_file_does_nothing_more() {
        // CMP-R21: a read whose modification time (ms) and size equal those
        // of the folder's last successful read does nothing more
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        save(&ada, &pagle_lua_text(LOG_A, 1));
        let running = start(Config::default(), &[&ada]);
        let reads = running.readings.file_reads();

        running.scans.send(scanned(&[&ada])).unwrap();
        let update = wait_for(&running.updates, |u| u.scan);
        // CMP-R23: every scan still tells subscribers, with nothing read.
        assert!(update.read.is_empty());
        assert_eq!(running.readings.file_reads(), reads);
    }

    #[test]
    fn a_folder_that_cant_be_watched_is_polled_every_interval() {
        // CMP-R22: where watching fails, Pagle.lua's modification time and
        // size are checked every poll interval and a change counts as an
        // event; the window shows "Checked every 30 s"
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        save(&ada, &pagle_lua_text(LOG_A, 1));
        let config = Config {
            poll_interval: Duration::from_millis(300),
            watch: false,
            ..Config::default()
        };
        let running = start(config, &[&ada]);
        assert!(running.readings.snapshot().unwrap().folders[0].polled);
        let view = window::View::of(running.readings.snapshot());
        assert!(view.folders[0].polled);
        assert_eq!(view.poll_seconds, 30);

        save(&ada, &pagle_lua_text(LOG_A, 2));
        let update = wait_for(&running.updates, |u| !u.read.is_empty());
        assert_eq!(entries(&update.snapshot, &ada), Some(2));
        assert!(update.snapshot.folders[0].polled);
    }

    #[test]
    fn three_failed_reads_show_cmp_r30s_text_on_the_folder() {
        // CMP-R30: after the last failure the account folder shows "Couldn't
        // read Pagle.lua. …", and keeps the logId it last held for its row
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        save(&ada, &pagle_lua_text(LOG_A, 1));
        let config = Config {
            read_retry_delay: Duration::from_millis(10),
            ..Config::default()
        };
        let running = start(config, &[&ada]);

        save(&ada, "PagleLog = {");
        let update = wait_for(&running.updates, |u| !u.read.is_empty());
        let folder = &update.snapshot.folders[0];
        assert_eq!(folder.reading, Reading::Unreadable);
        assert_eq!(folder.problem(), Some(COULDNT_READ));
        assert_eq!(folder.last_log_id.as_deref(), Some(LOG_A));
        assert_eq!(
            window::View::of(Some(update.snapshot)).folders[0].problem,
            Some(COULDNT_READ)
        );

        // The next save reads again.
        save(&ada, &pagle_lua_text(LOG_A, 2));
        let update = wait_for(&running.updates, |u| !u.read.is_empty());
        assert_eq!(update.snapshot.folders[0].problem(), None);
    }

    #[test]
    fn an_invalid_log_shows_cmp_r31s_text() {
        // CMP-R31: its account folder, and its logId's row when the logId
        // could be read, show "Pagle.lua holds a Catch log this Companion
        // can't upload. …"
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        let text = pagle_lua_text(LOG_A, 1).replace("[\"format\"] = 1", "[\"format\"] = 0");
        save(&ada, &text);
        let running = start(Config::default(), &[&ada]);
        let snapshot = running.readings.snapshot().unwrap();
        let folder = &snapshot.folders[0];
        assert_eq!(folder.problem(), Some(CANT_UPLOAD));
        assert_eq!(folder.last_log_id.as_deref(), Some(LOG_A));
        assert!(snapshot.catch_logs().is_empty());
    }

    #[test]
    fn a_pagle_log_with_zero_entries_produces_no_offer() {
        // CMP-R32: a local log with no entries is not offered, counted as a
        // copy or uploaded until it has one
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        save(&ada, &pagle_lua_text(LOG_A, 0));
        let running = start(Config::default(), &[&ada]);
        let snapshot = running.readings.snapshot().unwrap();
        assert!(matches!(snapshot.folders[0].reading, Reading::Valid(_)));
        assert!(snapshot.folders[0].catch_log().is_none());
        assert!(snapshot.catch_logs().is_empty());

        save(&ada, &pagle_lua_text(LOG_A, 1));
        let update = wait_for(&running.updates, |u| !u.read.is_empty());
        assert_eq!(
            update.snapshot.catch_logs().keys().collect::<Vec<_>>(),
            [LOG_A]
        );
    }

    #[test]
    fn catch_logs_tell_which_folders_hold_each_log_id() {
        // CMP-R42: valid local logs with entries and the same logId in two
        // or more account folders; an empty one isn't counted as a copy
        // (CMP-R32)
        let tmp = tempfile::tempdir().unwrap();
        let ada1 = account_folder(tmp.path(), "ADA#1");
        let ada2 = account_folder(tmp.path(), "ADA#2");
        let ada3 = account_folder(tmp.path(), "ADA#3");
        let bo = account_folder(tmp.path(), "BO#1");
        save(&ada1, &pagle_lua_text(LOG_A, 2));
        save(&ada2, &pagle_lua_text(LOG_A, 1));
        save(&ada3, &pagle_lua_text(LOG_A, 0));
        save(&bo, &pagle_lua_text(LOG_B, 1));
        let running = start(Config::default(), &[&ada1, &ada2, &ada3, &bo]);

        let snapshot = running.readings.snapshot().unwrap();
        let logs = snapshot.catch_logs();
        let folders =
            |id: &str| -> Vec<String> { logs[id].iter().map(|f| f.name.clone()).collect() };
        assert_eq!(folders(LOG_A), ["ADA#1", "ADA#2"]);
        assert_eq!(folders(LOG_B), ["BO#1"]);
    }

    #[test]
    fn upload_read_sees_same_size_metadata_even_when_mtime_is_restored() {
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        save(&ada, &pagle_lua_text(LOG_A, 1));
        let running = start(Config::default(), &[&ada]);
        let file = ada.join("SavedVariables/Pagle.lua");
        let before = fs::metadata(&file).unwrap();
        let edited = pagle_lua_text(LOG_A, 1).replace("\"Ada\"", "\"Eve\"");
        fs::write(&file, edited).unwrap();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(before.modified().unwrap()))
            .unwrap();
        assert_eq!(fs::metadata(&file).unwrap().len(), before.len());
        let Reading::Valid(read) = running.readings.read_now(&canon(&ada)) else {
            panic!("not valid");
        };
        assert_eq!(read.log.characters[0].name, "Eve");
    }

    #[test]
    fn read_now_reads_a_watched_file_without_triggering_a_run() {
        // CMP-R46: every run reads its watched file when it starts; CMP-R21:
        // an unchanged file isn't read again
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        save(&ada, &pagle_lua_text(LOG_A, 1));
        let running = start(Config::default(), &[&ada]);
        let reads = running.readings.file_reads();

        let Reading::Valid(read) = running.readings.read_now(&canon(&ada)) else {
            panic!("not valid");
        };
        assert_eq!(read.log.len(), 1);
        assert_eq!(running.readings.file_reads(), reads + 1);

        // A save the debounce hasn't reached yet is read at once, and
        // subscribers learn of it without a trigger.
        save(&ada, &pagle_lua_text(LOG_A, 4));
        let Reading::Valid(read) = running.readings.read_now(&canon(&ada)) else {
            panic!("not valid");
        };
        assert_eq!(read.log.len(), 4);
        let update = running.updates.recv_timeout(WAIT).unwrap();
        assert!(update.read.is_empty());
        assert!(!update.scan);
        assert_eq!(entries(&update.snapshot, &ada), Some(4));
        // The debounced read that follows finds the file as read: nothing
        // more.
        assert!(quiet(&running.updates, Duration::from_millis(2000)));
        assert_eq!(running.readings.file_reads(), reads + 2);
    }

    #[test]
    fn a_folder_gone_from_a_scan_is_no_longer_read() {
        // CMP-R15, CMP-R20: account folders follow the scans
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        let bo = account_folder(tmp.path(), "BO#1");
        save(&bo, &pagle_lua_text(LOG_B, 1));
        let running = start(Config::default(), &[&ada, &bo]);

        running.scans.send(scanned(&[&ada])).unwrap();
        let update = wait_for(&running.updates, |u| u.scan);
        assert_eq!(update.snapshot.folders.len(), 1);
        assert!(update.snapshot.folder(&canon(&bo)).is_none());
        let reads = running.readings.file_reads();
        save(&bo, &pagle_lua_text(LOG_B, 2));
        assert!(quiet(&running.updates, Duration::from_millis(2000)));
        assert_eq!(running.readings.file_reads(), reads);
    }

    #[test]
    fn a_late_subscriber_starts_with_the_current_snapshot() {
        // CMP-R23: what each folder holds is there for every reader
        let tmp = tempfile::tempdir().unwrap();
        let ada = account_folder(tmp.path(), "ADA#1");
        save(&ada, &pagle_lua_text(LOG_A, 1));
        let running = start(Config::default(), &[&ada]);
        let first = running.readings.subscribe().recv_timeout(WAIT).unwrap();
        assert_eq!(Some(first.snapshot), running.readings.snapshot());
        assert!(first.read.is_empty() && !first.scan);
    }
}

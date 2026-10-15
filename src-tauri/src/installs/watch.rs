//! Watching folders, one level deep, with polling where watching fails
//! (CMP-R22): the OS refuses the watch, or the folder is on a network file
//! system. A polled folder is checked every poll interval, and a change in
//! what it is checked for counts as an event.
//!
//! The scanner watches each `Agent/` folder for `product.db` (CMP-R14) and
//! each install's `WTF/Account/` for account folders that appear or go
//! (CMP-R15).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use notify::event::{EventKind, ModifyKind};
use notify::{RecursiveMode, Watcher};

use super::paths;

/// What a change in a watched folder is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// Any event for the entry of this name, compared without regard to case
    /// (CMP-R14: `product.db`, which Battle.net may replace).
    Name(String),
    /// Entries that appear, go or are renamed (CMP-R15).
    Entries,
}

/// What polling compares between two checks.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fingerprint {
    /// The named entry's modification time and size, if it exists.
    File(Option<(SystemTime, u64)>),
    /// The folder's entry names, sorted.
    Listing(Vec<String>),
}

fn fingerprint(dir: &Path, filter: &Filter) -> Fingerprint {
    match filter {
        Filter::Name(name) => Fingerprint::File(
            paths::lookup(dir, name)
                .and_then(|path| fs::metadata(path).ok())
                .and_then(|meta| Some((meta.modified().ok()?, meta.len()))),
        ),
        Filter::Entries => {
            let mut names: Vec<String> = fs::read_dir(dir)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            Fingerprint::Listing(names)
        }
    }
}

/// Whether an event of `kind` for `path`, in a folder watched for `filter`,
/// is a change.
fn is_change(filter: &Filter, kind: &EventKind, path: &Path) -> bool {
    match filter {
        Filter::Name(name) => {
            !kind.is_access()
                && path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
        }
        Filter::Entries => matches!(
            kind,
            EventKind::Any
                | EventKind::Other
                | EventKind::Create(_)
                | EventKind::Remove(_)
                | EventKind::Modify(ModifyKind::Name(_) | ModifyKind::Any)
        ),
    }
}

type Filters = Arc<Mutex<HashMap<PathBuf, Filter>>>;

fn lock_filters(filters: &Filters) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Filter>> {
    filters.lock().unwrap_or_else(|e| e.into_inner())
}

/// A set of watched folders. Changes in watched folders are reported to the
/// callback given to [`Watches::new`], from the watcher's own thread; changes
/// in polled folders are returned by [`Watches::poll`].
pub struct Watches {
    watcher: Option<notify::RecommendedWatcher>,
    /// The folders watched by the OS, with their filters.
    watched: Filters,
    /// The folders polled instead, with their last fingerprints.
    polled: HashMap<PathBuf, (Filter, Fingerprint)>,
}

impl Watches {
    /// `on_change` is called with the watched folder's path, as given to
    /// [`Watches::set`], for every event that is a change.
    pub fn new(on_change: impl Fn(PathBuf) + Send + 'static) -> Self {
        let watched: Filters = Arc::default();
        let filters = Arc::clone(&watched);
        let handler = move |event: notify::Result<notify::Event>| {
            let event = match event {
                Ok(event) => event,
                Err(e) => {
                    log::warn!("a folder watch failed: {e}");
                    return;
                }
            };
            let filters = lock_filters(&filters);
            for path in &event.paths {
                // The folder itself, or one of its entries.
                for dir in [Some(path.as_path()), path.parent()].into_iter().flatten() {
                    let Some(filter) = filters.get(dir) else {
                        continue;
                    };
                    if dir == path || is_change(filter, &event.kind, path) {
                        on_change(dir.to_path_buf());
                    }
                }
            }
        };
        let watcher = notify::recommended_watcher(handler)
            .inspect_err(|e| log::warn!("can't watch folders, polling them instead: {e}"))
            .ok();
        Self {
            watcher,
            watched,
            polled: HashMap::new(),
        }
    }

    /// A set that watches nothing and polls every folder, as where the OS
    /// refuses every watch (CMP-R22).
    pub fn polling_only() -> Self {
        Self {
            watcher: None,
            watched: Arc::default(),
            polled: HashMap::new(),
        }
    }

    /// Makes `wanted` the set of folders watched: stops watching the others,
    /// and watches the new ones, or polls those that can't be watched.
    ///
    /// The filters' lock is never held across a call into the watcher: the
    /// watcher's own thread, which runs the event handler that takes that
    /// lock, answers `watch` and `unwatch` (notify's inotify backend), so
    /// holding it there could deadlock.
    pub fn set(&mut self, wanted: &[(PathBuf, Filter)]) {
        let stale: Vec<PathBuf> = {
            let mut watched = lock_filters(&self.watched);
            let stale = watched
                .keys()
                .filter(|dir| !wanted.iter().any(|(w, _)| w == *dir))
                .cloned()
                .collect();
            watched.retain(|dir, _| wanted.iter().any(|(w, _)| w == dir));
            stale
        };
        if let Some(watcher) = self.watcher.as_mut() {
            for dir in stale {
                let _ = watcher.unwatch(&dir);
            }
        }
        self.polled
            .retain(|dir, _| wanted.iter().any(|(w, _)| w == dir));

        for (dir, filter) in wanted {
            if lock_filters(&self.watched).contains_key(dir) || self.polled.contains_key(dir) {
                continue;
            }
            let refused = if is_network_path(dir) {
                Some("it is on a network file system".to_owned())
            } else if let Some(watcher) = self.watcher.as_mut() {
                // In place before the watch starts, so no event is missed.
                lock_filters(&self.watched).insert(dir.clone(), filter.clone());
                let refused = watcher.watch(dir, RecursiveMode::NonRecursive).err();
                if refused.is_some() {
                    lock_filters(&self.watched).remove(dir);
                }
                refused.map(|e| e.to_string())
            } else {
                Some("folder watching isn't available".to_owned())
            };
            if let Some(reason) = refused {
                log::info!(
                    "checking {} every poll interval instead of watching it: {reason}",
                    dir.display()
                );
                self.polled
                    .insert(dir.clone(), (filter.clone(), fingerprint(dir, filter)));
            }
        }
    }

    /// Whether `dir` is polled instead of watched (CMP-R22).
    pub fn is_polled(&self, dir: &Path) -> bool {
        self.polled.contains_key(dir)
    }

    /// Whether any folder is polled.
    pub fn has_polled(&self) -> bool {
        !self.polled.is_empty()
    }

    /// Checks every polled folder and returns those that changed since the
    /// last check.
    pub fn poll(&mut self) -> Vec<PathBuf> {
        let mut changed = Vec::new();
        for (dir, (filter, last)) in &mut self.polled {
            let now = fingerprint(dir, filter);
            if now != *last {
                *last = now;
                changed.push(dir.clone());
            }
        }
        changed
    }
}

/// The Linux file system types that count as network file systems
/// (CMP-R22). Other FUSE file systems count as watchable.
pub const NETWORK_FS_TYPES: [&str; 6] = ["nfs", "nfs4", "cifs", "smb2", "smbfs", "fuse.sshfs"];

/// The type of the file system `path` is on, from the text of
/// `/proc/self/mountinfo`: the mount with the longest mount point holding
/// `path`.
pub fn mount_fs_type(mountinfo: &str, path: &Path) -> Option<String> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let mount_point = unescape_mount_point(left.split(' ').nth(4)?);
            let fs_type = right.split(' ').next()?;
            path.starts_with(&mount_point)
                .then(|| (mount_point.len(), fs_type.to_owned()))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, fs_type)| fs_type)
}

/// Mount points escape space, tab, newline and backslash as `\ooo`.
fn unescape_mount_point(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|digits| bytes[i] == b'\\' && digits.iter().all(|d| (b'0'..=b'7').contains(d)));
        if let Some(digits) = octal {
            let code = digits
                .iter()
                .fold(0u32, |code, d| code * 8 + u32::from(d - b'0'));
            out.push(code as u8);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `dir` is on a network file system, which counts as unwatchable
/// (CMP-R22): on Windows a drive of type remote or any UNC path, on Linux a
/// file system of [`NETWORK_FS_TYPES`].
pub fn is_network_path(dir: &Path) -> bool {
    #[cfg(windows)]
    {
        windows_is_network(dir)
    }
    #[cfg(target_os = "linux")]
    {
        let fs_type = fs::read_to_string("/proc/self/mountinfo")
            .ok()
            .and_then(|info| mount_fs_type(&info, dir));
        fs_type.is_some_and(|t| NETWORK_FS_TYPES.contains(&t.as_str()))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = dir;
        false
    }
}

#[cfg(windows)]
fn windows_is_network(dir: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};

    // `GetDriveTypeW`'s value for a remote (network) drive.
    const DRIVE_REMOTE: u32 = 4;
    let root = match dir.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::UNC(..) | Prefix::VerbatimUNC(..) => return true,
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                format!("{}:\\", char::from(letter))
            }
            _ => return false,
        },
        _ => return false,
    };
    let wide: Vec<u16> = std::ffi::OsStr::new(&root)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
    unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) == DRIVE_REMOTE }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    const MOUNTINFO: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 22 0:35 / /mnt/nas rw,relatime shared:20 - nfs4 nas:/export rw,vers=4.2
41 22 0:36 / /mnt/my\\040games rw,relatime shared:21 - cifs //box/games rw
42 22 0:37 / /home/ada/remote rw,nosuid shared:22 - fuse.sshfs ada@box: rw
43 22 0:38 / /home/ada/Games rw,nosuid shared:23 - fuse.gocryptfs secret rw
";

    #[test]
    fn network_file_systems_are_found_from_mountinfo() {
        // CMP-R22: on Linux a file system whose type in /proc/self/mountinfo
        // is nfs, nfs4, cifs, smb2, smbfs or fuse.sshfs; other FUSE file
        // systems count as watchable
        let network = |path: &str| {
            mount_fs_type(MOUNTINFO, Path::new(path))
                .is_some_and(|t| NETWORK_FS_TYPES.contains(&t.as_str()))
        };
        assert!(network("/mnt/nas/World of Warcraft"));
        assert!(network("/mnt/my games/WoW"));
        assert!(network("/home/ada/remote/WoW"));
        assert!(!network("/home/ada/Games/battlenet/drive_c"));
        assert!(!network("/home/ada/.wine/drive_c"));
        assert!(!network("/mnt/nasty"));
        assert_eq!(
            mount_fs_type(MOUNTINFO, Path::new("/home/ada")).as_deref(),
            Some("ext4")
        );
    }

    #[cfg(windows)]
    #[test]
    fn unc_paths_count_as_network_paths() {
        // CMP-R22: on Windows any UNC path (\\server\share)
        assert!(is_network_path(Path::new(
            r"\\server\share\World of Warcraft"
        )));
        assert!(!is_network_path(&std::env::temp_dir()));
    }

    #[test]
    fn only_entries_appearing_or_going_change_an_entries_folder() {
        // CMP-R15: account folders that appear or go; CMP-R14: any event for
        // product.db, which Battle.net may replace
        use notify::event::{AccessKind, CreateKind, DataChange, RenameMode};
        let entries = Filter::Entries;
        let p = Path::new("/wtf/Account/ADA#1");
        assert!(is_change(
            &entries,
            &EventKind::Create(CreateKind::Folder),
            p
        ));
        assert!(is_change(
            &entries,
            &EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            p
        ));
        assert!(!is_change(
            &entries,
            &EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            p
        ));
        assert!(!is_change(&entries, &EventKind::Access(AccessKind::Any), p));

        let db = Filter::Name("product.db".into());
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        assert!(is_change(&db, &modify, Path::new("/Agent/product.db")));
        assert!(is_change(&db, &modify, Path::new("/Agent/Product.DB")));
        assert!(!is_change(&db, &modify, Path::new("/Agent/.product.db")));
        assert!(!is_change(
            &db,
            &EventKind::Access(AccessKind::Any),
            Path::new("/Agent/product.db")
        ));
    }

    #[test]
    fn polling_notices_a_changed_file_and_a_changed_listing() {
        // CMP-R22: a polled folder is checked every poll interval, and a
        // change counts as an event
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("Agent");
        let account = tmp.path().join("Account");
        fs::create_dir_all(&agent).unwrap();
        fs::create_dir_all(&account).unwrap();
        let mut polled = HashMap::new();
        let db = Filter::Name("product.db".into());
        polled.insert(agent.clone(), (db.clone(), fingerprint(&agent, &db)));
        polled.insert(
            account.clone(),
            (Filter::Entries, fingerprint(&account, &Filter::Entries)),
        );
        let mut watches = Watches {
            watcher: None,
            watched: Arc::default(),
            polled,
        };

        assert!(watches.poll().is_empty());
        fs::write(agent.join("product.db"), "x").unwrap();
        fs::write(agent.join("other.txt"), "x").unwrap();
        assert_eq!(watches.poll(), std::slice::from_ref(&agent));
        fs::create_dir(account.join("ADA#1")).unwrap();
        assert_eq!(watches.poll(), std::slice::from_ref(&account));
        assert!(watches.poll().is_empty());
    }

    #[test]
    fn a_folder_that_cannot_be_watched_is_polled() {
        // CMP-R22: where the OS refuses the watch, the folder is polled
        let tmp = tempfile::tempdir().unwrap();
        let mut watches = Watches::new(|_| {});
        let missing = tmp.path().join("missing");
        watches.set(&[
            (tmp.path().to_path_buf(), Filter::Entries),
            (missing.clone(), Filter::Entries),
        ]);
        assert!(!watches.is_polled(tmp.path()));
        assert!(watches.is_polled(&missing));

        watches.set(&[]);
        assert!(!watches.has_polled());
    }

    #[test]
    fn without_a_watcher_every_folder_is_polled() {
        // CMP-R22: where the OS refuses the watch, the folder is polled
        let tmp = tempfile::tempdir().unwrap();
        let mut watches = Watches::polling_only();
        let db = Filter::Name("product.db".into());
        watches.set(&[(tmp.path().to_path_buf(), db)]);
        assert!(watches.is_polled(tmp.path()));
        fs::write(tmp.path().join("product.db"), "x").unwrap();
        assert_eq!(watches.poll(), [tmp.path().to_path_buf()]);
    }

    #[test]
    fn a_watched_folder_reports_a_new_entry() {
        // CMP-R15: the Companion watches WTF/Account/ itself for account
        // folders that appear or go
        let tmp = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let mut watches = Watches::new(move |dir| {
            let _ = tx.send(dir);
        });
        let account = tmp.path().join("Account");
        fs::create_dir(&account).unwrap();
        watches.set(&[(account.clone(), Filter::Entries)]);

        fs::create_dir(account.join("ADA#1")).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), account);
    }

    #[test]
    fn changing_the_watched_set_while_events_arrive_never_hangs() {
        // CMP-R15, CMP-R20: folders are watched and unwatched as account
        // folders and SavedVariables/ appear or go, while the game saves
        let tmp = tempfile::tempdir().unwrap();
        let busy = tmp.path().join("SavedVariables");
        let other = tmp.path().join("Account");
        fs::create_dir(&busy).unwrap();
        fs::create_dir(&other).unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let (busy, stop) = (busy.clone(), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut n = 0u64;
                // Paced, so events keep arriving without starving the rest
                // of the suite's file I/O (an unpaced loop slowed every
                // test on Windows, where each write is scanned).
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    n += 1;
                    let _ = fs::write(busy.join("Pagle.lua"), n.to_string());
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let (done_tx, done) = mpsc::channel();
        std::thread::spawn(move || {
            let mut watches = Watches::new(|_| {});
            let pagle = Filter::Name("Pagle.lua".into());
            for _ in 0..200 {
                watches.set(&[(busy.clone(), pagle.clone())]);
                watches.set(&[
                    (busy.clone(), pagle.clone()),
                    (other.clone(), Filter::Entries),
                ]);
                watches.set(&[(other.clone(), Filter::Entries)]);
            }
            let _ = done_tx.send(());
        });
        let finished = done.recv_timeout(Duration::from_secs(60));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().unwrap();
        assert!(finished.is_ok(), "Watches::set hung");
    }
}

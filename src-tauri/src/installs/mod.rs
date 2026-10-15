//! Finding WoW Forever installs (CMP-R10 to CMP-R17): Battle.net's
//! `product.db` files, each candidate's `.flavor.info`, the folders the Player
//! adds by hand, and each install's account folders.
//!
//! [`scan`] is the whole of one scan over the file system, with no state of
//! its own; [`Scanner`] runs it at start, every rescan interval, and when
//! something it watches changes (CMP-R14), and tells its subscribers the new
//! set of installs. The window's WoW folders section is in [`window`].
//!
//! It reads only `product.db`, `.flavor.info` and folder listings (CMP-R4).

pub mod flavor_info;
pub mod paths;
pub mod product_db;
mod scanner;
pub mod system;
pub mod watch;
pub mod window;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

pub use scanner::{Inputs, Scanner, Snapshot};

/// The Forever product uid built into the release (CMP Tunables), used until
/// a config read has ever succeeded (CMP-R12). A dev build may name the live
/// product's code here.
pub const BUILT_IN_FOREVER_PRODUCT_UID: &str = "wow_classic_beta";

/// Rescan interval (CMP Tunables, CMP-R14).
pub const RESCAN_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Debounce time (CMP Tunables, CMP-R14, CMP-R21).
pub const DEBOUNCE: Duration = Duration::from_secs(1);

/// Poll interval for folders that can't be watched (CMP Tunables, CMP-R22).
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// What the window says when a picked folder holds no install (CMP-R13).
pub const NOT_A_FOREVER_FOLDER: &str = "This isn't a WoW Forever folder.";

/// One `product.db` to read: Battle.net's `Agent` folder, and on Linux the
/// Wine or Proton prefix it sits in, which its locations map into (CMP-R11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbSource {
    pub agent_dir: PathBuf,
    pub prefix: Option<PathBuf>,
}

/// How an install was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// Listed by a `product.db` (CMP-R11).
    BattleNet,
    /// Added by the Player (CMP-R13).
    Manual,
}

/// One WoW Forever install: an install root and a flavor folder under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Install {
    /// The install root, canonical (CMP-R17).
    pub root: String,
    /// The flavor folder, canonical (CMP-R17). It identifies the install.
    pub flavor: String,
    pub source: Source,
    /// Its account folders, by name.
    pub accounts: Vec<AccountFolder>,
    /// `WTF/Account/` can't be watched and is listed every poll interval
    /// instead (CMP-R22). Set by the [`Scanner`].
    pub accounts_polled: bool,
}

/// A folder `WTF/Account/<ACCOUNT>/` (CMP-R15). Its name is shown and never
/// used to decide anything (CMP-R16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountFolder {
    pub name: String,
    /// Canonical (CMP-R17).
    pub path: String,
}

/// A `product.db` that couldn't be read or decoded (CMP-R11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

/// What one scan found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// Every install, each flavor folder once: Battle.net's first, in
    /// `product.db` order, then the manual ones in the order added.
    pub installs: Vec<Install>,
    /// Manual installs that no longer qualify, as kept in local state
    /// (CMP-R13).
    pub not_found: Vec<String>,
    /// `product.db`s that couldn't be read or decoded.
    pub skipped: Vec<Skipped>,
}

/// The Forever product uid of local state: the last successful config
/// read's, or the built-in one until one has succeeded (CMP-R12, CMP-R70).
pub fn forever_product_uid(state: &crate::state::State) -> String {
    state
        .config
        .as_ref()
        .map_or(BUILT_IN_FOREVER_PRODUCT_UID.to_owned(), |c| {
            c.forever_product_uid.clone()
        })
}

/// Runs CMP-R10 to CMP-R13 and CMP-R15 over `sources` and the manual
/// installs kept in local state, finding installs whose `.flavor.info` names
/// `uid`.
pub fn scan(sources: &[DbSource], manual: &[String], uid: &str) -> Scan {
    let mut result = Scan::default();
    let mut keys = Vec::new();
    let mut add = |result: &mut Scan, flavor: String, source| {
        let key = paths::same_folder_key(&flavor);
        if keys.contains(&key) {
            return;
        }
        keys.push(key);
        result.installs.push(install(flavor, source));
    };

    for source in sources {
        let Some(db) = paths::lookup(&source.agent_dir, "product.db") else {
            continue;
        };
        let candidates = match fs::read(&db) {
            Ok(bytes) => product_db::decode(&bytes).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let candidates = match candidates {
            Ok(candidates) => candidates,
            Err(reason) => {
                result.skipped.push(Skipped { path: db, reason });
                continue;
            }
        };
        for candidate in candidates {
            let Some(root) = paths::map_location(&candidate.location, source.prefix.as_deref())
            else {
                continue;
            };
            let Some(flavor) = paths::lookup(&root, &candidate.name) else {
                continue;
            };
            if let Some(flavor) = qualifying(&flavor, uid) {
                add(&mut result, flavor, Source::BattleNet);
            }
        }
    }

    for entry in manual {
        match qualifying(Path::new(entry), uid) {
            Some(flavor) => add(&mut result, flavor, Source::Manual),
            None => result.not_found.push(entry.clone()),
        }
    }
    result
}

/// The canonical flavor folder when `flavor` is one whose `.flavor.info`
/// names `uid`, byte for byte (CMP-R12).
fn qualifying(flavor: &Path, uid: &str) -> Option<String> {
    if !flavor.is_dir() {
        return None;
    }
    let info = paths::lookup(flavor, ".flavor.info")?;
    let code = flavor_info::product_code(&fs::read(info).ok()?)?;
    if code.as_bytes() != uid.as_bytes() {
        return None;
    }
    paths::canonical(flavor)
}

fn install(flavor: String, source: Source) -> Install {
    let root = match flavor.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(i) => flavor[..i].to_owned(),
        None => flavor.clone(),
    };
    Install {
        root,
        accounts: account_folders(Path::new(&flavor)),
        flavor,
        source,
        accounts_polled: false,
    }
}

/// Every directory directly under `WTF/Account/` except `SavedVariables`
/// (CMP-R15), by name.
pub fn account_folders(flavor: &Path) -> Vec<AccountFolder> {
    let Some(account) = paths::lookup_path(flavor, &["WTF", "Account"]) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&account) else {
        return Vec::new();
    };
    let mut folders: Vec<AccountFolder> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.eq_ignore_ascii_case("SavedVariables") {
                return None;
            }
            Some(AccountFolder {
                path: paths::canonical(&entry.path())?,
                name,
            })
        })
        .collect();
    folders.sort_by(|a, b| a.name.cmp(&b.name));
    folders
}

/// The folder whose entries tell when account folders appear or go:
/// `WTF/Account/`, or while it doesn't exist its nearest existing parent
/// inside the flavor folder (CMP-R15, CMP-R20).
pub fn accounts_watch_dir(flavor: &Path) -> PathBuf {
    let Some(wtf) = paths::lookup(flavor, "WTF").filter(|p| p.is_dir()) else {
        return flavor.to_path_buf();
    };
    paths::lookup(&wtf, "Account")
        .filter(|p| p.is_dir())
        .unwrap_or(wtf)
}

/// The flavor folders a folder picked with "Add a WoW folder…" stands for
/// (CMP-R13): itself if it holds a `.flavor.info`; its parent if it is named
/// `WTF` (any case); otherwise each of its direct subfolders that holds a
/// `.flavor.info`. Those that name `uid` come back canonical; when none does,
/// the error is [`NOT_A_FOREVER_FOLDER`].
pub fn picked_installs(picked: &Path, uid: &str) -> Result<Vec<String>, &'static str> {
    let flavors: Vec<PathBuf> = if paths::lookup(picked, ".flavor.info").is_some() {
        vec![picked.to_path_buf()]
    } else if picked
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("WTF"))
    {
        picked.parent().map(Path::to_path_buf).into_iter().collect()
    } else {
        let mut subdirs: Vec<PathBuf> = fs::read_dir(picked)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        subdirs.sort();
        subdirs
    };
    let installs: Vec<String> = flavors
        .iter()
        .filter_map(|flavor| qualifying(flavor, uid))
        .collect();
    if installs.is_empty() {
        Err(NOT_A_FOREVER_FOLDER)
    } else {
        Ok(installs)
    }
}

/// Adds `installs` to the manual installs of local state, each folder once
/// (CMP-R13, CMP-R14). Returns how many were new.
pub fn add_manual(manual: &mut Vec<String>, installs: &[String]) -> usize {
    let mut added = 0;
    for install in installs {
        let key = paths::same_folder_key(install);
        if !manual.iter().any(|m| paths::same_folder_key(m) == key) {
            manual.push(install.clone());
            added += 1;
        }
    }
    added
}

/// Removes a manual install from local state (CMP-R13). Returns whether
/// there was one.
pub fn remove_manual(manual: &mut Vec<String>, flavor: &str) -> bool {
    let key = paths::same_folder_key(flavor);
    let before = manual.len();
    manual.retain(|m| paths::same_folder_key(m) != key);
    manual.len() != before
}

#[cfg(test)]
pub(crate) mod fixture {
    //! Temporary folder trees laid out like Battle.net's.

    use std::fs;
    use std::path::{Path, PathBuf};

    use super::product_db::fixture as pb;

    pub fn mkdirs(base: &Path, rel: &str) -> PathBuf {
        let dir = base.join(rel);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A flavor folder at `base/rel` whose `.flavor.info` names `uid`.
    pub fn flavor(base: &Path, rel: &str, uid: &str) -> PathBuf {
        let dir = mkdirs(base, rel);
        fs::write(
            dir.join(".flavor.info"),
            format!("Product Flavor!STRING:0\n{uid}\n"),
        )
        .unwrap();
        dir
    }

    /// A `product.db` in `agent_dir` listing `(location, name)` products.
    pub fn product_db(agent_dir: &Path, products: &[(&str, &str)]) {
        fs::create_dir_all(agent_dir).unwrap();
        let products: Vec<Vec<u8>> = products
            .iter()
            .map(|(location, name)| pb::product("wow_x", location, Some(name)))
            .collect();
        fs::write(agent_dir.join("product.db"), pb::db(&products)).unwrap();
    }

    pub fn location(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;

    const UID: &str = BUILT_IN_FOREVER_PRODUCT_UID;

    fn windows_source(agent_dir: &Path) -> DbSource {
        DbSource {
            agent_dir: agent_dir.to_path_buf(),
            prefix: None,
        }
    }

    fn canon(path: &Path) -> String {
        paths::canonical(path).unwrap()
    }

    #[test]
    fn the_built_in_uid_is_the_beta() {
        // CMP Tunables: built-in Forever product uid, wow_classic_beta
        assert_eq!(BUILT_IN_FOREVER_PRODUCT_UID, "wow_classic_beta");
        assert_eq!(RESCAN_INTERVAL, Duration::from_secs(3600));
        assert_eq!(DEBOUNCE, Duration::from_secs(1));
        assert_eq!(POLL_INTERVAL, Duration::from_secs(30));
    }

    #[test]
    fn only_the_candidate_naming_the_forever_uid_is_an_install() {
        // CMP-R11, CMP-R12: of two products, the candidate whose .flavor.info
        // reads wow_classic_beta under Product Flavor is an install, the
        // other is dropped; a BOM and CR are ignored
        let tmp = tempfile::tempdir().unwrap();
        let root = mkdirs(tmp.path(), "World of Warcraft");
        let beta = mkdirs(&root, "_classic_beta_");
        fs::write(
            beta.join(".flavor.info"),
            b"\xEF\xBB\xBFProduct Flavor!STRING:0\r\nwow_classic_beta\r\n",
        )
        .unwrap();
        flavor(&root, "_retail_", "wow");
        let agent = tmp.path().join("ProgramData/Battle.net/Agent");
        product_db(
            &agent,
            &[
                (&location(&root), "_retail_"),
                (&location(&root), "_classic_beta_"),
            ],
        );

        let scan = scan(&[windows_source(&agent)], &[], UID);
        assert_eq!(scan.installs.len(), 1);
        let install = &scan.installs[0];
        assert_eq!(install.flavor, canon(&beta));
        assert_eq!(install.root, canon(&root));
        assert_eq!(install.source, Source::BattleNet);
        assert!(scan.skipped.is_empty());
    }

    #[test]
    fn folder_names_are_never_matched() {
        // CMP-R12: folder names are never matched; only .flavor.info decides
        let tmp = tempfile::tempdir().unwrap();
        let root = mkdirs(tmp.path(), "WoW");
        flavor(&root, "_classic_beta_", "wow_classic_era");
        let renamed = flavor(&root, "my forever copy", UID);
        mkdirs(&root, "_anniversary_"); // no .flavor.info
        let agent = tmp.path().join("Agent");
        product_db(
            &agent,
            &[
                (&location(&root), "_classic_beta_"),
                (&location(&root), "my forever copy"),
                (&location(&root), "_anniversary_"),
            ],
        );

        let scan = scan(&[windows_source(&agent)], &[], UID);
        let flavors: Vec<_> = scan.installs.iter().map(|i| i.flavor.clone()).collect();
        assert_eq!(flavors, [canon(&renamed)]);
    }

    #[test]
    fn the_uid_is_compared_byte_for_byte() {
        // CMP-R12: compared byte for byte
        let tmp = tempfile::tempdir().unwrap();
        let beta = flavor(tmp.path(), "WoW/_classic_beta_", "WOW_CLASSIC_BETA");
        assert_eq!(qualifying(&beta, UID), None);
        assert_eq!(qualifying(&beta, "WOW_CLASSIC_BETA"), Some(canon(&beta)));
    }

    #[test]
    fn a_changed_uid_finds_another_install() {
        // CMP-R12: the uid of the last config read replaces the built-in
        // one, so from the switch on, beta installs are no longer found
        let tmp = tempfile::tempdir().unwrap();
        let root = mkdirs(tmp.path(), "WoW");
        flavor(&root, "_classic_beta_", UID);
        let live = flavor(&root, "_classic_forever_", "wow_classic_forever");
        let agent = tmp.path().join("Agent");
        product_db(
            &agent,
            &[
                (&location(&root), "_classic_beta_"),
                (&location(&root), "_classic_forever_"),
            ],
        );

        let scan = scan(&[windows_source(&agent)], &[], "wow_classic_forever");
        assert_eq!(scan.installs.len(), 1);
        assert_eq!(scan.installs[0].flavor, canon(&live));
    }

    #[test]
    fn installs_in_a_wine_prefix_are_found_through_drive_c() {
        // CMP-R10, CMP-R11: a prefix's product.db, whose C:/Program Files
        // (x86)/World of Warcraft maps to <prefix>/drive_c/…
        let tmp = tempfile::tempdir().unwrap();
        let prefix = mkdirs(tmp.path(), "Games/battlenet");
        let beta = flavor(
            &prefix,
            "drive_c/Program Files (x86)/World of Warcraft/_classic_beta_",
            UID,
        );
        let agent = mkdirs(&prefix, "drive_c/ProgramData/Battle.net/Agent");
        product_db(
            &agent,
            &[("C:/Program Files (x86)/World of Warcraft", "_classic_beta_")],
        );

        let source = DbSource {
            agent_dir: paths::prefix_agent_dir(&prefix).unwrap(),
            prefix: Some(prefix),
        };
        let scan = scan(&[source], &[], UID);
        assert_eq!(scan.installs.len(), 1);
        assert_eq!(scan.installs[0].flavor, canon(&beta));
    }

    #[test]
    fn a_broken_product_db_is_skipped_and_the_others_are_read() {
        // CMP-R11: a product.db that can't be decoded is skipped (and noted
        // in the local log file by the Scanner)
        let tmp = tempfile::tempdir().unwrap();
        let broken = mkdirs(tmp.path(), "C/Agent");
        fs::write(broken.join("product.db"), [0x0a, 0xff, 0xff]).unwrap();
        let root = mkdirs(tmp.path(), "D/WoW");
        let beta = flavor(&root, "_classic_beta_", UID);
        let good = tmp.path().join("D/Agent");
        product_db(&good, &[(&location(&root), "_classic_beta_")]);

        let scan = scan(&[windows_source(&broken), windows_source(&good)], &[], UID);
        assert_eq!(scan.skipped.len(), 1);
        assert_eq!(scan.skipped[0].path, broken.join("product.db"));
        assert_eq!(scan.installs.len(), 1);
        assert_eq!(scan.installs[0].flavor, canon(&beta));
    }

    #[test]
    fn an_agent_folder_without_product_db_is_not_an_error() {
        // CMP-R10: a missing product.db is no install and nothing to note
        let tmp = tempfile::tempdir().unwrap();
        let agent = mkdirs(tmp.path(), "Agent");
        let scan = scan(&[windows_source(&agent)], &[], UID);
        assert_eq!(scan, Scan::default());
    }

    #[test]
    fn a_flavor_folder_listed_twice_counts_once() {
        // CMP-R14: the same flavor folder found twice (two product.dbs) counts
        // once, comparing canonical paths
        let tmp = tempfile::tempdir().unwrap();
        let root = mkdirs(tmp.path(), "WoW");
        flavor(&root, "_classic_beta_", UID);
        let a = tmp.path().join("A");
        let b = tmp.path().join("B");
        product_db(&a, &[(&location(&root), "_classic_beta_")]);
        let roundabout = root.join("..").join("WoW");
        product_db(&b, &[(&location(&roundabout), "_classic_beta_")]);

        let scan = scan(&[windows_source(&a), windows_source(&b)], &[], UID);
        assert_eq!(scan.installs.len(), 1);
    }

    #[test]
    fn an_install_found_and_added_shows_once_as_found_by_battle_net() {
        // CMP-R14: found by Battle.net and also added by hand shows once, as
        // found by Battle.net; its manual entry stays in local state silently
        let tmp = tempfile::tempdir().unwrap();
        let root = mkdirs(tmp.path(), "WoW");
        let beta = flavor(&root, "_classic_beta_", UID);
        let agent = tmp.path().join("Agent");
        product_db(&agent, &[(&location(&root), "_classic_beta_")]);
        let mut manual = Vec::new();
        add_manual(&mut manual, &picked_installs(&root, UID).unwrap());

        let scan = scan(&[windows_source(&agent)], &manual, UID);
        assert_eq!(scan.installs.len(), 1);
        assert_eq!(scan.installs[0].source, Source::BattleNet);
        assert_eq!(scan.installs[0].flavor, canon(&beta));
        assert!(scan.not_found.is_empty());
        assert_eq!(manual.len(), 1);
    }

    #[test]
    fn a_manual_install_shows_as_added_by_you() {
        // CMP-R13: a manual install is checked again at every scan
        let tmp = tempfile::tempdir().unwrap();
        let beta = flavor(tmp.path(), "G/World of Warcraft/_classic_beta_", UID);

        let scan = scan(&[], &[canon(&beta)], UID);
        assert_eq!(scan.installs.len(), 1);
        assert_eq!(scan.installs[0].source, Source::Manual);
    }

    #[test]
    fn a_manual_install_that_stops_qualifying_is_not_found() {
        // CMP-R13: one that no longer qualifies (missing, or no longer naming
        // the Forever product uid) stays in local state and shows Not found
        let tmp = tempfile::tempdir().unwrap();
        let beta = flavor(tmp.path(), "WoW/_classic_beta_", UID);
        let gone = flavor(tmp.path(), "Old/_classic_beta_", UID);
        let manual = vec![canon(&beta), canon(&gone)];
        fs::remove_dir_all(&gone).unwrap();
        fs::write(beta.join(".flavor.info"), "Product Flavor!STRING:0\nwow\n").unwrap();

        let scan = scan(&[], &manual, UID);
        assert!(scan.installs.is_empty());
        assert_eq!(scan.not_found, manual);
    }

    #[test]
    fn no_install_is_an_empty_scan() {
        // CMP-R131: none found is the no-install state
        assert_eq!(scan(&[], &[], UID), Scan::default());
    }

    #[test]
    fn several_installs_are_all_found() {
        // CMP-R131: several (beta and live, two drives, two Wine prefixes)
        // are all installs
        let tmp = tempfile::tempdir().unwrap();
        let c = flavor(tmp.path(), "C/WoW/_classic_beta_", UID);
        let d = flavor(tmp.path(), "D/WoW/_classic_beta_", UID);
        let agent = tmp.path().join("Agent");
        product_db(
            &agent,
            &[
                (&location(&tmp.path().join("C/WoW")), "_classic_beta_"),
                (&location(&tmp.path().join("D/WoW")), "_classic_beta_"),
            ],
        );
        let scan = scan(&[windows_source(&agent)], &[], UID);
        let flavors: Vec<_> = scan.installs.iter().map(|i| i.flavor.clone()).collect();
        assert_eq!(flavors, [canon(&c), canon(&d)]);
    }

    #[test]
    fn adding_an_install_root_adds_its_forever_flavor_folder() {
        // CMP-R13: an install root stands for each direct subfolder holding a
        // .flavor.info; those naming the Forever uid become manual installs
        let tmp = tempfile::tempdir().unwrap();
        let root = mkdirs(tmp.path(), "World of Warcraft");
        let beta = flavor(&root, "_classic_beta_", UID);
        flavor(&root, "_retail_", "wow");
        mkdirs(&root, "Data");

        assert_eq!(picked_installs(&root, UID), Ok(vec![canon(&beta)]));
    }

    #[test]
    fn adding_a_flavor_folder_or_its_wtf_folder_adds_it() {
        // CMP-R13: a flavor folder if it holds a .flavor.info; its parent
        // flavor folder if its name is WTF (any case)
        let tmp = tempfile::tempdir().unwrap();
        let beta = flavor(tmp.path(), "WoW/_classic_beta_", UID);
        let wtf = mkdirs(&beta, "wtf");

        assert_eq!(picked_installs(&beta, UID), Ok(vec![canon(&beta)]));
        assert_eq!(picked_installs(&wtf, UID), Ok(vec![canon(&beta)]));
    }

    #[test]
    fn a_folder_without_a_forever_install_is_not_a_wow_forever_folder() {
        // CMP-R13: if none names the Forever uid, nothing is added and the
        // window says "This isn't a WoW Forever folder."
        let tmp = tempfile::tempdir().unwrap();
        let empty = mkdirs(tmp.path(), "Documents");
        let retail = flavor(tmp.path(), "WoW/_retail_", "wow");

        assert_eq!(
            picked_installs(&empty, UID),
            Err("This isn't a WoW Forever folder.")
        );
        assert_eq!(picked_installs(&retail, UID), Err(NOT_A_FOREVER_FOLDER));
        assert_eq!(
            picked_installs(&tmp.path().join("WoW"), UID),
            Err(NOT_A_FOREVER_FOLDER)
        );
    }

    #[test]
    fn manual_installs_are_kept_once_and_removed_by_canonical_path() {
        // CMP-R13, CMP-R14: remembered as manual, each folder once; Remove
        // deletes it from local state
        let mut manual = Vec::new();
        assert_eq!(
            add_manual(&mut manual, &["G:/WoW/_classic_beta_".into()]),
            1
        );
        assert_eq!(
            add_manual(&mut manual, &["G:/WoW/_classic_beta_".into()]),
            0
        );
        assert_eq!(
            add_manual(&mut manual, &["g:/wow/_classic_beta_".into()]),
            usize::from(!cfg!(windows))
        );

        assert!(remove_manual(&mut manual, "G:/WoW/_classic_beta_"));
        assert_eq!(manual.is_empty(), cfg!(windows));
        assert!(!remove_manual(&mut manual, "G:/elsewhere"));
    }

    #[test]
    fn account_folders_are_found_without_regard_to_case() {
        // CMP-R15, CMP-R17: wtf/account/ADA#1/ is found on a case-sensitive
        // file system; WTF/Account/SavedVariables is not an account folder
        let tmp = tempfile::tempdir().unwrap();
        let beta = flavor(tmp.path(), "WoW/_classic_beta_", UID);
        let ada = mkdirs(&beta, "wtf/account/ADA#1");
        mkdirs(&beta, "wtf/account/savedvariables");
        mkdirs(&beta, "wtf/account/104973978#2/SavedVariables");
        fs::write(beta.join("wtf/account/notes.txt"), "a file").unwrap();

        let folders = account_folders(&beta);
        let names: Vec<_> = folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["104973978#2", "ADA#1"]);
        assert_eq!(folders[1].path, canon(&ada));
        assert!(folders[1].path.ends_with("/wtf/account/ADA#1"));
    }

    #[test]
    fn a_scan_lists_each_installs_account_folders() {
        // CMP-R15: in each install, every directory directly under
        // WTF/Account/ is an account folder
        let tmp = tempfile::tempdir().unwrap();
        let beta = flavor(tmp.path(), "WoW/_classic_beta_", UID);
        mkdirs(&beta, "WTF/Account/SavedVariables");
        mkdirs(&beta, "WTF/Account/1#1");

        let scan = scan(&[], &[canon(&beta)], UID);
        let names: Vec<_> = scan.installs[0]
            .accounts
            .iter()
            .map(|a| a.name.clone())
            .collect();
        assert_eq!(names, ["1#1"]);
    }

    #[test]
    fn account_folders_are_watched_through_the_nearest_existing_folder() {
        // CMP-R15, CMP-R20: WTF/Account/ is watched, or while it doesn't
        // exist its nearest existing parent inside the flavor folder
        let tmp = tempfile::tempdir().unwrap();
        let beta = flavor(tmp.path(), "WoW/_classic_beta_", UID);
        assert_eq!(accounts_watch_dir(&beta), beta);
        let wtf = mkdirs(&beta, "WTF");
        assert_eq!(canon(&accounts_watch_dir(&beta)), canon(&wtf));
        let account = mkdirs(&beta, "WTF/Account");
        assert_eq!(canon(&accounts_watch_dir(&beta)), canon(&account));
    }
}

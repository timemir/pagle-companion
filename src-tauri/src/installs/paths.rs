//! Path rules for finding installs: case-tolerant lookup of fixed path
//! components and canonical path strings (CMP-R17), where Battle.net's
//! `product.db`s live (CMP-R10), and how a Windows location maps into a Wine
//! or Proton prefix (CMP-R11).

use std::fs;
use std::path::{Path, PathBuf};

/// Looks `name` up in `dir`: with its exact case first, then without regard
/// to case among the folder's existing entries, whose own case is kept
/// (CMP-R17). `None` when neither finds an entry.
pub fn lookup(dir: &Path, name: &str) -> Option<PathBuf> {
    let exact = dir.join(name);
    if fs::symlink_metadata(&exact).is_ok() {
        return Some(exact);
    }
    let wanted = name.to_lowercase();
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().to_lowercase() == wanted)
        .map(|entry| entry.path())
}

/// Looks each of `names` up in turn below `base` (CMP-R17).
pub fn lookup_path(base: &Path, names: &[&str]) -> Option<PathBuf> {
    names
        .iter()
        .try_fold(base.to_path_buf(), |dir, name| lookup(&dir, name))
}

/// The canonical form a path is stored and compared in: absolute, links
/// resolved, `/` as separator, without Windows' `\\?\` prefix (CMP-R17).
/// `None` when the path doesn't exist.
pub fn canonical(path: &Path) -> Option<String> {
    let resolved = dunce::canonicalize(path).ok()?;
    Some(slashed(&resolved.to_string_lossy()))
}

/// `path` with `/` as separator and no verbatim prefix.
fn slashed(path: &str) -> String {
    let path = path.replace('\\', "/");
    if let Some(unc) = path.strip_prefix("//?/UNC/") {
        format!("//{unc}")
    } else if let Some(rest) = path.strip_prefix("//?/") {
        rest.to_owned()
    } else {
        path
    }
}

/// What two canonical paths are compared by: without regard to case on
/// Windows, exactly elsewhere (CMP-R14).
pub fn same_folder_key(canonical: &str) -> String {
    if cfg!(windows) {
        canonical.to_lowercase()
    } else {
        canonical.to_owned()
    }
}

/// The path components, below a Wine or Proton prefix, that a Windows
/// `location` such as `C:/…` or `D:\…` maps to (CMP-R11): `C:` to
/// `drive_c`, any other drive `X:` through the symlink `dosdevices/x:`.
/// `None` for a location without a drive letter.
pub fn wine_components(location: &str) -> Option<Vec<String>> {
    let location = location.replace('\\', "/");
    let mut chars = location.chars();
    let (letter, colon) = (chars.next()?, chars.next()?);
    if !letter.is_ascii_alphabetic() || colon != ':' {
        return None;
    }
    let letter = letter.to_ascii_lowercase();
    let mut components = if letter == 'c' {
        vec!["drive_c".to_owned()]
    } else {
        vec!["dosdevices".to_owned(), format!("{letter}:")]
    };
    components.extend(
        chars
            .as_str()
            .split('/')
            .filter(|c| !c.is_empty() && *c != ".")
            .map(str::to_owned),
    );
    Some(components)
}

/// The folder a `product.db` location names: as it is on Windows, mapped
/// into `prefix` on Linux (CMP-R11). `None` when it maps to no existing
/// folder.
pub fn map_location(location: &str, prefix: Option<&Path>) -> Option<PathBuf> {
    let path = match prefix {
        None => PathBuf::from(location),
        Some(prefix) => {
            let components = wine_components(location)?;
            let names: Vec<&str> = components.iter().map(String::as_str).collect();
            lookup_path(prefix, &names)?
        }
    };
    path.is_dir().then_some(path)
}

/// Battle.net's `Agent` folder below a Windows `ProgramData` folder.
const AGENT_BELOW_PROGRAM_DATA: [&str; 2] = ["Battle.net", "Agent"];

/// The `Agent` folders that may hold a `product.db` on Windows (CMP-R10):
/// below `%ProgramData%`, and below `<drive>:\ProgramData` on every other
/// fixed drive. Folders that don't exist are left out, and the drive that
/// holds `%ProgramData%` gives the same folder, which counts once.
pub fn windows_agent_dirs(program_data: Option<&Path>, fixed_drives: &[PathBuf]) -> Vec<PathBuf> {
    let program_datas = program_data.map(Path::to_path_buf).into_iter().chain(
        fixed_drives
            .iter()
            .filter_map(|drive| lookup(drive, "ProgramData")),
    );
    let mut seen = Vec::new();
    let mut dirs = Vec::new();
    for dir in program_datas {
        let Some(agent) = lookup_path(&dir, &AGENT_BELOW_PROGRAM_DATA).filter(|d| d.is_dir())
        else {
            continue;
        };
        let key = canonical(&agent).map(|c| same_folder_key(&c));
        if !seen.contains(&key) {
            seen.push(key);
            dirs.push(agent);
        }
    }
    dirs
}

/// The Wine and Proton prefix globs of CMP Contracts, below the home folder.
/// `*` stands for any one folder.
pub const PREFIX_GLOBS: [&str; 9] = [
    "Games/*",
    ".wine",
    ".local/share/wineprefixes/*",
    ".local/share/Steam/steamapps/compatdata/*/pfx",
    ".var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/compatdata/*/pfx",
    ".local/share/bottles/bottles/*",
    ".var/app/com.usebottles.bottles/data/bottles/bottles/*",
    "Faugus/*",
    "Games/Heroic/Prefixes/default/*",
];

/// Every existing folder below `home` that matches one of the prefix globs
/// (CMP Contracts, Linux prefix globs).
pub fn wine_prefixes(home: &Path) -> Vec<PathBuf> {
    let mut prefixes = Vec::new();
    for glob in PREFIX_GLOBS {
        let mut dirs = vec![home.to_path_buf()];
        for part in glob.split('/') {
            dirs = dirs
                .iter()
                .flat_map(|dir| {
                    if part == "*" {
                        subdirs(dir)
                    } else {
                        dir.join(part)
                            .is_dir()
                            .then(|| dir.join(part))
                            .into_iter()
                            .collect()
                    }
                })
                .collect();
        }
        prefixes.extend(dirs);
    }
    prefixes
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// A prefix's Battle.net `Agent` folder, if it has one (CMP-R10).
pub fn prefix_agent_dir(prefix: &Path) -> Option<PathBuf> {
    lookup_path(prefix, &["drive_c", "ProgramData", "Battle.net", "Agent"]).filter(|d| d.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkdirs(base: &Path, rel: &str) -> PathBuf {
        let dir = base.join(rel);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_component_is_found_without_regard_to_case_and_keeps_its_case() {
        // CMP-R17: looked up with its exact case first, then without regard
        // to case; an existing folder found that way is used as it is
        let tmp = tempfile::tempdir().unwrap();
        mkdirs(tmp.path(), "wtf/account");

        let found = lookup_path(tmp.path(), &["WTF", "Account"]).unwrap();
        assert!(canonical(&found).unwrap().ends_with("/wtf/account"));
        assert_eq!(lookup(tmp.path(), "Interface"), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_exact_case_wins_over_another_case() {
        // CMP-R17: exact case first
        let tmp = tempfile::tempdir().unwrap();
        mkdirs(tmp.path(), "wtf");
        mkdirs(tmp.path(), "WTF");
        assert_eq!(lookup(tmp.path(), "WTF"), Some(tmp.path().join("WTF")));
        assert_eq!(lookup(tmp.path(), "wtf"), Some(tmp.path().join("wtf")));
    }

    #[test]
    fn canonical_paths_use_slashes_and_no_verbatim_prefix() {
        // CMP-R17: canonical absolute paths with / as separator, without
        // Windows' \\?\ prefix
        let tmp = tempfile::tempdir().unwrap();
        let dir = mkdirs(tmp.path(), "World of Warcraft/_classic_beta_");
        let path = canonical(&dir.join(".").join("..").join("_classic_beta_")).unwrap();
        assert!(!path.contains('\\'), "{path}");
        assert!(!path.starts_with("//?/"), "{path}");
        assert!(
            path.ends_with("/World of Warcraft/_classic_beta_"),
            "{path}"
        );
        assert!(Path::new(&path).is_absolute());
        assert_eq!(canonical(&tmp.path().join("missing")), None);
    }

    #[test]
    fn verbatim_prefixes_are_dropped() {
        // CMP-R17: no \\?\ prefix, for disks and UNC shares alike
        assert_eq!(slashed(r"\\?\G:\World of Warcraft"), "G:/World of Warcraft");
        assert_eq!(slashed(r"\\?\UNC\nas\games\WoW"), "//nas/games/WoW");
        assert_eq!(slashed("/home/ada/Games"), "/home/ada/Games");
    }

    #[test]
    fn same_folder_ignores_case_on_windows_only() {
        // CMP-R14: canonical paths compared without regard to case on Windows
        let a = same_folder_key("G:/World of Warcraft/_classic_beta_");
        let b = same_folder_key("g:/world of warcraft/_Classic_Beta_");
        assert_eq!(a == b, cfg!(windows));
    }

    #[test]
    fn drive_c_maps_to_drive_c_and_other_drives_through_dosdevices() {
        // CMP-R11: inside a prefix, C:/… maps to <prefix>/drive_c/…, another
        // drive letter X: through <prefix>/dosdevices/x:
        assert_eq!(
            wine_components("C:/Program Files (x86)/World of Warcraft").unwrap(),
            ["drive_c", "Program Files (x86)", "World of Warcraft"]
        );
        assert_eq!(
            wine_components(r"D:\Games\World of Warcraft\").unwrap(),
            ["dosdevices", "d:", "Games", "World of Warcraft"]
        );
        assert_eq!(wine_components("c:\\WoW").unwrap(), ["drive_c", "WoW"]);
        assert_eq!(wine_components("/home/ada/WoW"), None);
        assert_eq!(wine_components(""), None);
    }

    #[test]
    fn a_c_location_maps_into_drive_c_of_a_fake_prefix() {
        // CMP-R11: C:/Program Files (x86)/World of Warcraft maps to
        // <prefix>/drive_c/…, looked up without regard to case (CMP-R17)
        let tmp = tempfile::tempdir().unwrap();
        let root = mkdirs(tmp.path(), "drive_c/Program Files (x86)/world of warcraft");

        let mapped =
            map_location("C:/Program Files (x86)/World of Warcraft", Some(tmp.path())).unwrap();
        assert_eq!(canonical(&mapped), canonical(&root));
    }

    #[cfg(unix)]
    #[test]
    fn a_d_location_maps_through_the_dosdevices_symlink() {
        // CMP-R11: another drive letter X: maps through the symlink
        // <prefix>/dosdevices/x:
        let tmp = tempfile::tempdir().unwrap();
        let prefix = mkdirs(tmp.path(), "prefix");
        let games = mkdirs(tmp.path(), "mnt/games/World of Warcraft");
        mkdirs(&prefix, "dosdevices");
        std::os::unix::fs::symlink(tmp.path().join("mnt/games"), prefix.join("dosdevices/d:"))
            .unwrap();

        let mapped = map_location("D:/World of Warcraft", Some(&prefix)).unwrap();
        assert_eq!(canonical(&mapped), canonical(&games));
    }

    #[test]
    fn a_location_that_maps_to_no_folder_is_dropped() {
        // CMP-R11: a location that maps to no existing folder is dropped
        let tmp = tempfile::tempdir().unwrap();
        mkdirs(tmp.path(), "drive_c");
        assert_eq!(map_location("C:/Missing", Some(tmp.path())), None);
        assert_eq!(map_location("E:/WoW", Some(tmp.path())), None);
        let missing = tmp.path().join("missing");
        assert_eq!(map_location(&missing.to_string_lossy(), None), None);
        assert_eq!(
            map_location(&tmp.path().to_string_lossy(), None),
            Some(tmp.path().to_path_buf())
        );
    }

    #[test]
    fn windows_agent_folders_are_below_program_data_and_every_other_fixed_drive() {
        // CMP-R10: %ProgramData%\Battle.net\Agent, and
        // <drive>:\ProgramData\Battle.net\Agent on every other fixed drive
        let tmp = tempfile::tempdir().unwrap();
        let program_data = mkdirs(tmp.path(), "C/ProgramData");
        let c_agent = mkdirs(&program_data, "Battle.net/Agent");
        let d = mkdirs(tmp.path(), "D");
        let d_agent = mkdirs(&d, "programdata/battle.net/agent");
        let e = mkdirs(tmp.path(), "E"); // no Battle.net

        let found = windows_agent_dirs(Some(&program_data), &[d, e]);
        let found: Vec<_> = found.iter().map(|p| canonical(p).unwrap()).collect();
        assert_eq!(
            found,
            [canonical(&c_agent).unwrap(), canonical(&d_agent).unwrap()]
        );
    }

    #[test]
    fn the_program_data_drive_is_not_searched_twice() {
        // CMP-R10: every *other* fixed drive: the drive holding %ProgramData%
        // gives the same Agent folder, which counts once
        let tmp = tempfile::tempdir().unwrap();
        let c = mkdirs(tmp.path(), "C");
        let program_data = mkdirs(&c, "ProgramData");
        mkdirs(&program_data, "Battle.net/Agent");

        assert_eq!(windows_agent_dirs(Some(&program_data), &[c]).len(), 1);
    }

    #[test]
    fn every_prefix_glob_is_searched() {
        // CMP Contracts, Linux prefix globs; CMP-R10: every prefix matching
        // them
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let expected = [
            "Games/lutris-wow",
            ".wine",
            ".local/share/wineprefixes/wow",
            ".local/share/Steam/steamapps/compatdata/123/pfx",
            ".var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/compatdata/456/pfx",
            ".local/share/bottles/bottles/Battle.net",
            ".var/app/com.usebottles.bottles/data/bottles/bottles/WoW",
            "Faugus/battlenet",
            "Games/Heroic/Prefixes/default/Battle.net",
        ];
        for rel in expected {
            mkdirs(home, rel);
        }
        // compatdata without pfx is no prefix.
        mkdirs(home, ".local/share/Steam/steamapps/compatdata/789");

        let found: Vec<String> = wine_prefixes(home)
            .iter()
            .map(|p| {
                p.strip_prefix(home)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        for rel in expected {
            assert!(found.iter().any(|f| f == rel), "{rel} not in {found:?}");
        }
        assert!(!found.iter().any(|f| f.ends_with("compatdata/789")));
    }

    #[test]
    fn a_prefix_agent_folder_is_below_drive_c_program_data() {
        // CMP-R10: <prefix>/drive_c/ProgramData/Battle.net/Agent, looked up
        // without regard to case (CMP-R17)
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(prefix_agent_dir(tmp.path()), None);
        let agent = mkdirs(tmp.path(), "drive_c/programdata/Battle.net/Agent");
        assert_eq!(
            prefix_agent_dir(tmp.path()).map(|p| canonical(&p)),
            Some(canonical(&agent))
        );
    }
}

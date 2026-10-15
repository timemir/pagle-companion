//! Where this machine's `product.db`s are (CMP-R10): on Windows below
//! `%ProgramData%` and every fixed drive, on Linux in every Wine or Proton
//! prefix of the prefix globs.

use std::path::Path;

use super::DbSource;
use super::paths;

/// This machine's `product.db` folders, looked for anew at every scan.
pub fn db_sources() -> Vec<DbSource> {
    #[cfg(windows)]
    {
        let program_data = std::env::var_os("ProgramData").map(std::path::PathBuf::from);
        paths::windows_agent_dirs(program_data.as_deref(), &fixed_drives())
            .into_iter()
            .map(|agent_dir| DbSource {
                agent_dir,
                prefix: None,
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        dirs::home_dir()
            .map(|home| prefix_sources(&home))
            .unwrap_or_default()
    }
}

/// The `product.db` folders of every Wine or Proton prefix below `home`.
pub fn prefix_sources(home: &Path) -> Vec<DbSource> {
    paths::wine_prefixes(home)
        .into_iter()
        .filter_map(|prefix| {
            Some(DbSource {
                agent_dir: paths::prefix_agent_dir(&prefix)?,
                prefix: Some(prefix),
            })
        })
        .collect()
}

/// The roots of every fixed local drive, such as `C:\`.
#[cfg(windows)]
fn fixed_drives() -> Vec<std::path::PathBuf> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};

    // `GetDriveTypeW`'s value for a fixed local drive.
    const DRIVE_FIXED: u32 = 3;
    // SAFETY: takes nothing and returns a bit mask.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", char::from(b'A' + i)))
        .filter(|root| {
            let wide: Vec<u16> = std::ffi::OsStr::new(root)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives
            // the call.
            unsafe { GetDriveTypeW(wide.as_ptr()) == DRIVE_FIXED }
        })
        .map(std::path::PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::fixture::mkdirs;
    use super::*;

    #[test]
    fn every_prefix_with_battle_net_is_a_source() {
        // CMP-R10: <prefix>/drive_c/ProgramData/Battle.net/Agent/product.db
        // for every prefix matching the prefix globs
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        mkdirs(home, ".wine/drive_c/ProgramData/Battle.net/Agent");
        mkdirs(home, "Games/other/drive_c/windows");

        let sources = prefix_sources(home);
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].prefix.as_deref(),
            Some(home.join(".wine").as_path())
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_system_drive_is_a_fixed_drive() {
        // CMP-R10: every fixed local drive
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        assert!(fixed_drives().contains(&std::path::PathBuf::from(format!("{system}\\"))));
    }
}

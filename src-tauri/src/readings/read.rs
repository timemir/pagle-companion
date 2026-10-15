//! Reading one account folder's `Pagle.lua` (CMP-R30): whole into memory,
//! with its modification time and size taken just before and just after,
//! tried again up to the read attempts when they differ, the open fails or
//! parsing fails.

use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::installs::paths;
use crate::pagle_log::{self, Found, LocalLog};

/// Read attempts, the first included (CMP Tunables, CMP-R30).
pub const READ_ATTEMPTS: u32 = 3;

/// The wait between two read attempts (CMP Tunables, CMP-R30).
pub const READ_RETRY_DELAY: Duration = Duration::from_secs(2);

/// The watched file's modification time, to the millisecond, and size: what
/// a read is compared by (CMP-R21) and what a run records (CMP Data,
/// `fileMtimeMs` and `fileSize`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStamp {
    /// Unix milliseconds, rounded down.
    pub mtime_ms: i64,
    pub size: u64,
}

impl FileStamp {
    fn of(meta: &Metadata) -> io::Result<Self> {
        Ok(Self {
            mtime_ms: unix_ms(meta.modified()?),
            size: meta.len(),
        })
    }

    /// The as-of time of a local log read from a file of this stamp: its
    /// modification time in whole Unix seconds, rounded down (CMP-R30,
    /// UPL-R35).
    pub fn as_of(&self) -> i64 {
        self.mtime_ms.div_euclid(1000)
    }
}

fn unix_ms(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        // Before 1970: rounded down too.
        Err(e) => {
            let before = e.duration();
            -i64::try_from(before.as_millis()).unwrap_or(i64::MAX)
                - i64::from(before.subsec_nanos() % 1_000_000 != 0)
        }
    }
}

/// What reading one account folder's `Pagle.lua` found.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    /// No `Pagle.lua`, or one without a `PagleLog` assignment: the folder has
    /// no Catch log, which isn't an error (CMP-R30). `file` is the stamp of
    /// the file read, when there was one.
    NoCatchLog { file: Option<FileStamp> },
    /// A Catch log this Companion can't upload (CMP-R31), with its `logId`
    /// when that could be read.
    Invalid {
        log_id: Option<String>,
        file: FileStamp,
    },
    /// Every read attempt failed (CMP-R30). Nothing is tried again until the
    /// next event or scan.
    Unreadable,
    /// A valid local log (CMP-R31).
    Valid(LocalRead),
}

/// A valid local log, as read.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalRead {
    pub log: Arc<LocalLog>,
    /// The as-of time every page of a run from this read carries (CMP-R30,
    /// UPL-R35).
    pub as_of: i64,
    /// The file's stamp taken before the read.
    pub file: FileStamp,
}

impl Reading {
    /// The stamp of the file this reading came from: `Some(None)` for a
    /// folder without `Pagle.lua`, `None` for a read that failed, which
    /// isn't a successful read (CMP-R21).
    pub fn stamp(&self) -> Option<Option<FileStamp>> {
        match self {
            Reading::NoCatchLog { file } => Some(*file),
            Reading::Invalid { file, .. } => Some(Some(*file)),
            Reading::Valid(read) => Some(Some(read.file)),
            Reading::Unreadable => None,
        }
    }

    /// The `logId` the file holds, valid or not, when it could be read.
    pub fn log_id(&self) -> Option<&str> {
        match self {
            Reading::Invalid { log_id, .. } => log_id.as_deref(),
            Reading::Valid(read) => Some(&read.log.log_id),
            Reading::NoCatchLog { .. } | Reading::Unreadable => None,
        }
    }
}

/// `SavedVariables/Pagle.lua` in `account_folder`, each component looked up
/// without regard to case where needed (CMP-R17). `None` when it doesn't
/// exist.
pub fn pagle_lua(account_folder: &Path) -> Option<PathBuf> {
    paths::lookup_path(account_folder, &["SavedVariables", "Pagle.lua"])
}

/// The stamp of the account folder's `Pagle.lua` now, without reading it:
/// `None` when there is none. `Err` when it exists but can't be looked at.
pub fn current_stamp(account_folder: &Path) -> io::Result<Option<FileStamp>> {
    let Some(file) = pagle_lua(account_folder) else {
        return Ok(None);
    };
    match fs::metadata(file) {
        Ok(meta) => FileStamp::of(&meta).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Reads the account folder's `Pagle.lua` as CMP-R30 says, waiting the read
/// retry delay between attempts: it blocks up to (read attempts − 1) × the
/// read retry delay when the file won't read.
pub fn read(account_folder: &Path) -> Reading {
    read_with(account_folder, READ_ATTEMPTS, READ_RETRY_DELAY, &mut || {})
}

/// [`read`] with its tunables given, and `during` run in every attempt
/// between taking the stamp before and reading, where a test changes the
/// file as the game would mid-read (CMP-R130).
pub(crate) fn read_with(
    account_folder: &Path,
    attempts: u32,
    delay: Duration,
    during: &mut dyn FnMut(),
) -> Reading {
    let mut why = String::new();
    for attempt in 1..=attempts.max(1) {
        if attempt > 1 {
            std::thread::sleep(delay);
        }
        match try_once(account_folder, during) {
            Ok(reading) => return reading,
            Err(reason) => why = reason,
        }
    }
    log::warn!(
        "couldn't read Pagle.lua in {} after {attempts} attempt(s): {why}",
        account_folder.display()
    );
    Reading::Unreadable
}

/// One attempt: the reading, or why to try again. The reason never holds
/// the file's text (CMP-R112).
fn try_once(account_folder: &Path, during: &mut dyn FnMut()) -> Result<Reading, String> {
    let Some(file) = pagle_lua(account_folder) else {
        return Ok(Reading::NoCatchLog { file: None });
    };
    let before = match fs::metadata(&file) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Reading::NoCatchLog { file: None });
        }
        Err(e) => return Err(format!("can't look at it: {e}")),
    };
    during();
    // The open may fail: on Windows the game may hold the file.
    let bytes = fs::read(&file).map_err(|e| format!("can't open it: {e}"))?;
    let after = fs::metadata(&file).map_err(|e| format!("can't look at it: {e}"))?;
    let unchanged = before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
        && bytes.len() as u64 == after.len();
    if !unchanged {
        return Err("it changed during the read".to_owned());
    }
    let stamp = FileStamp::of(&before).map_err(|e| format!("no modification time: {e}"))?;
    match pagle_log::parse(&bytes) {
        Err(e) => Err(format!("it doesn't parse: {e}")),
        Ok(Found::NoCatchLog) => Ok(Reading::NoCatchLog { file: Some(stamp) }),
        Ok(Found::Invalid(invalid)) => {
            log::warn!(
                "Pagle.lua in {} holds a Catch log this Companion can't upload ({}): {}",
                account_folder.display(),
                invalid.log_id.as_deref().unwrap_or("no readable logId"),
                invalid.reason
            );
            Ok(Reading::Invalid {
                log_id: invalid.log_id,
                file: stamp,
            })
        }
        Ok(Found::Valid(log)) => Ok(Reading::Valid(LocalRead {
            log: Arc::new(log),
            as_of: stamp.as_of(),
            file: stamp,
        })),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::super::fixture::*;
    use super::*;

    const FAST: Duration = Duration::from_millis(10);

    #[test]
    fn the_read_attempts_are_three_two_seconds_apart() {
        // CMP Tunables: read attempts, 3, 2 s apart (the first included)
        assert_eq!(READ_ATTEMPTS, 3);
        assert_eq!(READ_RETRY_DELAY, Duration::from_secs(2));
    }

    #[test]
    fn a_valid_log_is_read_whole_with_its_stamp() {
        // CMP-R30: read whole into memory, with its modification time and size
        let tmp = tempfile::tempdir().unwrap();
        let account = account_folder(tmp.path(), "ADA#1");
        save(&account, &pagle_lua_text(LOG_A, 3));

        let Reading::Valid(read) = read(&account) else {
            panic!("not valid");
        };
        assert_eq!(read.log.log_id, LOG_A);
        assert_eq!(read.log.len(), 3);
        let meta = fs::metadata(account.join("SavedVariables/Pagle.lua")).unwrap();
        assert_eq!(read.file.size, meta.len());
        assert_eq!(read.file.mtime_ms, unix_ms(meta.modified().unwrap()));
    }

    #[test]
    fn the_as_of_is_the_modification_time_in_whole_seconds_rounded_down() {
        // CMP-R30: the as-of of a file modified at 1791735090.999 is 1791735090
        let tmp = tempfile::tempdir().unwrap();
        let account = account_folder(tmp.path(), "ADA#1");
        save(&account, &pagle_lua_text(LOG_A, 1));
        let file = File::options()
            .write(true)
            .open(account.join("SavedVariables/Pagle.lua"))
            .unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_millis(1_791_735_090_999))
            .unwrap();
        drop(file);

        let Reading::Valid(read) = read(&account) else {
            panic!("not valid");
        };
        assert_eq!(read.as_of, 1_791_735_090);
        assert_eq!(read.file.mtime_ms, 1_791_735_090_999);
    }

    #[test]
    fn times_before_1970_round_down_too() {
        // CMP-R30: whole seconds rounded down
        assert_eq!(unix_ms(UNIX_EPOCH - Duration::from_micros(1500)), -2);
        assert_eq!(
            FileStamp {
                mtime_ms: -1,
                size: 0
            }
            .as_of(),
            -1
        );
    }

    #[test]
    fn a_missing_file_or_folder_is_no_catch_log() {
        // CMP-R30: a missing Pagle.lua means the folder has no Catch log
        let tmp = tempfile::tempdir().unwrap();
        let account = account_folder(tmp.path(), "ADA#1");
        assert_eq!(read(&account), Reading::NoCatchLog { file: None });
        fs::create_dir(account.join("SavedVariables")).unwrap();
        assert_eq!(read(&account), Reading::NoCatchLog { file: None });
        assert_eq!(current_stamp(&account).unwrap(), None);
    }

    #[test]
    fn a_file_without_pagle_log_is_no_catch_log() {
        // CMP-R30: one without a PagleLog assignment means no Catch log
        let tmp = tempfile::tempdir().unwrap();
        let account = account_folder(tmp.path(), "ADA#1");
        save(&account, "PagleDB = { [\"global\"] = {} }\n");
        let stamp = current_stamp(&account).unwrap();
        assert!(stamp.is_some());
        assert_eq!(read(&account), Reading::NoCatchLog { file: stamp });
    }

    #[test]
    fn the_file_is_found_without_regard_to_case() {
        // CMP-R17: savedvariables/pagle.lua is found on a case-sensitive
        // file system
        let tmp = tempfile::tempdir().unwrap();
        let account = account_folder(tmp.path(), "ADA#1");
        fs::create_dir(account.join("savedvariables")).unwrap();
        fs::write(
            account.join("savedvariables/pagle.lua"),
            pagle_lua_text(LOG_A, 1),
        )
        .unwrap();
        assert!(matches!(read(&account), Reading::Valid(_)));
    }

    #[test]
    fn an_invalid_log_keeps_its_log_id() {
        // CMP-R31: its logId's row shows the state when the logId could be read
        let tmp = tempfile::tempdir().unwrap();
        let account = account_folder(tmp.path(), "ADA#1");
        save(
            &account,
            &pagle_lua_text(LOG_A, 1).replace("[\"format\"] = 1", "[\"format\"] = 0"),
        );
        let reading = read(&account);
        assert!(matches!(reading, Reading::Invalid { .. }), "{reading:?}");
        assert_eq!(reading.log_id(), Some(LOG_A));
    }

    #[test]
    fn a_file_whose_size_changes_during_the_read_is_read_again() {
        // CMP-R30, CMP-R130: modification time and size differ before and
        // after, so the read is tried again
        let tmp = tempfile::tempdir().unwrap();
        let account = account_folder(tmp.path(), "ADA#1");
        save(&account, &pagle_lua_text(LOG_A, 1));
        let mut attempts = 0;
        let sv = account.clone();
        let reading = read_with(&account, 3, FAST, &mut || {
            attempts += 1;
            if attempts == 1 {
                fs::write(
                    sv.join("SavedVariables/Pagle.lua"),
                    pagle_lua_text(LOG_A, 2),
                )
                .unwrap();
            }
        });
        assert_eq!(attempts, 2);
        let Reading::Valid(read) = reading else {
            panic!("not valid");
        };
        assert_eq!(read.log.len(), 2);
    }

    #[test]
    fn three_failed_attempts_make_the_folder_unreadable() {
        // CMP-R30: up to the read attempts, then it gives up until the next
        // event
        let tmp = tempfile::tempdir().unwrap();
        let account = account_folder(tmp.path(), "ADA#1");
        save(&account, &pagle_lua_text(LOG_A, 1));
        let mut attempts = 0u64;
        let sv = account.clone();
        let reading = read_with(&account, 3, FAST, &mut || {
            attempts += 1;
            let text = pagle_lua_text(LOG_A, attempts + 1);
            fs::write(sv.join("SavedVariables/Pagle.lua"), text).unwrap();
        });
        assert_eq!(attempts, 3);
        assert_eq!(reading, Reading::Unreadable);
        assert_eq!(reading.stamp(), None);
    }

    #[test]
    fn a_file_that_doesnt_parse_is_tried_again() {
        // CMP-R30: parsing fails, so it waits the read retry delay and tries
        // again, up to the read attempts
        let tmp = tempfile::tempdir().unwrap();
        let account = account_folder(tmp.path(), "ADA#1");
        save(&account, "PagleLog = {");
        let mut attempts = 0;
        let reading = read_with(&account, 3, FAST, &mut || attempts += 1);
        assert_eq!(attempts, 3);
        assert_eq!(reading, Reading::Unreadable);

        // The game finishing its write in between makes it read.
        let mut attempts = 0;
        let sv = account.clone();
        let reading = read_with(&account, 3, FAST, &mut || {
            attempts += 1;
            if attempts == 2 {
                save(&sv, &pagle_lua_text(LOG_A, 1));
            }
        });
        assert!(matches!(reading, Reading::Valid(_)));
    }

    #[test]
    fn the_retry_waits_the_delay_between_attempts() {
        // CMP-R30: it waits the read retry delay and tries again
        let tmp = tempfile::tempdir().unwrap();
        let account = account_folder(tmp.path(), "ADA#1");
        save(&account, "PagleLog = {");
        let started = std::time::Instant::now();
        read_with(&account, 3, Duration::from_millis(100), &mut || {});
        assert!(started.elapsed() >= Duration::from_millis(200));
    }
}

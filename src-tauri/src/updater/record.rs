//! `updater.json` in the state directory (CMP Data): the failed-verification
//! count of DST-R57, `{ "v": 1, "failedVerifications": <n>, "forVersion":
//! "<the latest.json version the count is for>" }`. A verification that
//! succeeds or a new `version` in `latest.json` resets it. It is written
//! whole to a temporary file and renamed, like `state.json`; a missing or
//! unreadable file counts nothing.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use semver::Version;
use serde::{Deserialize, Serialize};

const FORMAT: u32 = 1;

/// DST Tunables: the failed-verification count after which DST-R57's
/// download text shows.
pub const FAILED_VERIFICATION_COUNT: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    v: u32,
    failed_verifications: u32,
    for_version: String,
}

impl Default for Record {
    fn default() -> Self {
        Self {
            v: FORMAT,
            failed_verifications: 0,
            for_version: String::new(),
        }
    }
}

/// The failed-verification count, kept in `updater.json`.
#[derive(Debug)]
pub struct Failures {
    path: PathBuf,
    record: Mutex<Record>,
}

impl Failures {
    /// The count as `updater.json` at `path` holds it.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let record = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
            .filter(|record| record.v == FORMAT)
            .unwrap_or_default();
        Self {
            path,
            record: Mutex::new(record),
        }
    }

    /// How many downloads in a row have failed verification.
    pub fn count(&self) -> u32 {
        self.lock().failed_verifications
    }

    /// `latest.json` names `version`: a new one starts the count again.
    pub fn saw(&self, version: &Version) {
        let mut record = self.lock();
        let version = version.to_string();
        if record.for_version != version {
            record.for_version = version;
            record.failed_verifications = 0;
            self.save(&record);
        }
    }

    /// The download of `version` failed verification. Returns the count.
    pub fn failed(&self, version: &Version) -> u32 {
        self.saw(version);
        let mut record = self.lock();
        record.failed_verifications += 1;
        self.save(&record);
        record.failed_verifications
    }

    /// A download verified: the count starts again.
    pub fn verified(&self) {
        let mut record = self.lock();
        if record.failed_verifications != 0 {
            record.failed_verifications = 0;
            self.save(&record);
        }
    }

    fn save(&self, record: &Record) {
        if let Err(e) = write_whole(&self.path, record) {
            log::error!("couldn't write updater.json: {e}");
        }
    }

    fn lock(&self) -> MutexGuard<'_, Record> {
        self.record.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn write_whole(path: &Path, record: &Record) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
    {
        let mut file = File::create(&tmp)?;
        file.write_all(&json)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(version: &str) -> Version {
        Version::parse(version).unwrap()
    }

    fn on_disk(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn the_count_after_which_the_download_text_shows_is_three() {
        // DST Tunables: failed-verification count, 3 in a row
        assert_eq!(FAILED_VERIFICATION_COUNT, 3);
    }

    #[test]
    fn failures_are_counted_in_updater_json() {
        // DST-R57, CMP Data: the failed-verification count in updater.json,
        // { "v": 1, "failedVerifications": n, "forVersion": "…" }, written
        // whole through a temporary file
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("updater.json");
        let failures = Failures::load(&path);
        assert_eq!(failures.count(), 0);

        assert_eq!(failures.failed(&v("1.2.0")), 1);
        assert_eq!(failures.failed(&v("1.2.0")), 2);
        assert_eq!(
            on_disk(&path),
            serde_json::json!({ "v": 1, "failedVerifications": 2, "forVersion": "1.2.0" })
        );
        assert!(!tmp.path().join("updater.json.tmp").exists());

        // Another process run reads the same count.
        assert_eq!(Failures::load(&path).count(), 2);
    }

    #[test]
    fn a_good_verification_resets_the_count() {
        // DST-R57: reset by a verification that succeeds
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("updater.json");
        let failures = Failures::load(&path);
        failures.failed(&v("1.2.0"));
        failures.failed(&v("1.2.0"));
        failures.verified();
        assert_eq!(failures.count(), 0);
        assert_eq!(on_disk(&path)["failedVerifications"], 0);
    }

    #[test]
    fn a_new_version_in_latest_json_resets_the_count() {
        // DST-R57: reset by a new version in latest.json
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("updater.json");
        let failures = Failures::load(&path);
        failures.failed(&v("1.2.0"));
        failures.failed(&v("1.2.0"));
        failures.saw(&v("1.2.0"));
        assert_eq!(failures.count(), 2);
        failures.saw(&v("1.2.1"));
        assert_eq!(failures.count(), 0);
        assert_eq!(
            on_disk(&path),
            serde_json::json!({ "v": 1, "failedVerifications": 0, "forVersion": "1.2.1" })
        );
        assert_eq!(failures.failed(&v("1.2.1")), 1);
    }

    #[test]
    fn a_missing_or_unreadable_file_counts_nothing() {
        // CMP Data: updater.json is the updater's own; one it can't read
        // starts from nothing
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("updater.json");
        assert_eq!(Failures::load(&path).count(), 0);
        for body in [
            "not json",
            r#"{ "v": 2, "failedVerifications": 5, "forVersion": "1.0.0" }"#,
        ] {
            fs::write(&path, body).unwrap();
            assert_eq!(Failures::load(&path).count(), 0, "{body}");
        }
    }
}

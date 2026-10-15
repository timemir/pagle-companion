//! Local state: `state.json`, version 1 (CMP Data).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The `v` this Companion writes and reads.
pub const VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub v: u64,
    #[serde(default)]
    pub manual_installs: Vec<String>,
    #[serde(default)]
    pub first_run_done: bool,
    #[serde(default)]
    pub kept_app_image_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigRead>,
    #[serde(default)]
    pub settings: Settings,
    /// Null while not linked.
    #[serde(default)]
    pub battle_tag: Option<String>,
    /// Linux only: the store holding the credential (CMP-R62).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_store: Option<CredentialStore>,
    #[serde(default)]
    pub logs: BTreeMap<String, LogState>,
    #[serde(default)]
    pub in_game: InGame,
}

impl Default for State {
    fn default() -> Self {
        Self {
            v: VERSION,
            manual_installs: Vec::new(),
            first_run_done: false,
            kept_app_image_paths: Vec::new(),
            config: None,
            settings: Settings::default(),
            battle_tag: None,
            credential_store: None,
            logs: BTreeMap::new(),
            in_game: InGame::default(),
        }
    }
}

/// The last successful config read (CMP-R12).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigRead {
    pub forever_product_uid: String,
    pub read_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub start_at_login: bool,
    pub game_mode: bool,
}

impl Default for Settings {
    fn default() -> Self {
        // Start at login is on by default (CMP-R3); Game Mode is off (CMP-R90).
        Self {
            start_at_login: true,
            game_mode: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialStore {
    SecretService,
    File,
}

/// What the Companion remembers about one `logId`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogState {
    /// Absent while awaiting (CMP-R40).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<Answer>,
    /// The picked account folder; null when the `logId` sits in one folder
    /// only (CMP-R42).
    #[serde(default)]
    pub folder: Option<String>,
    pub characters: String,
    pub last_folder: String,
    #[serde(default)]
    pub whole_send_pending: bool,
    #[serde(default)]
    pub last_run: Option<LastRun>,
    #[serde(default)]
    pub last_uploaded_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    Upload,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastRun {
    pub result: RunResult,
    pub at: i64,
    pub file_mtime_ms: i64,
    pub file_size: u64,
    pub format: i64,
    pub companion_version: String,
    pub code: Option<String>,
    pub message: Option<String>,
    pub catch_log: Option<CatchLogState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunResult {
    Uploaded,
    UpToDate,
    Divergence,
    Stale,
    Deleted,
    DevLog,
    WaitingForPagle,
    LogCap,
    TooLarge,
    Failed,
}

/// The reply's `catchLog` (CMP-R53), kept as the backend sent it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatchLogState {
    pub verification: String,
    pub pending_reason: Option<String>,
    pub held_characters: u32,
    pub left_off: Option<serde_json::Value>,
}

/// The last successful in-game downloads (CMP-R81). Unlink clears it (CMP-R63).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InGame {
    pub following_at: Option<i64>,
    pub anglers_at: Option<i64>,
    pub anglers_etag: Option<String>,
}

/// What reading `state.json` found.
#[derive(Debug)]
pub enum Loaded {
    /// A file this Companion reads.
    State(State),
    /// No file: a first run (CMP-R100).
    Missing,
    /// A file that can't be parsed, or whose `v` is higher than this Companion
    /// knows. It was renamed `state.json.broken`, and the Companion starts as
    /// on a first run.
    Broken,
}

/// Reads local state from `path`.
pub fn load(path: &Path) -> io::Result<Loaded> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Loaded::Missing),
        Err(e) => return Err(e),
    };
    match parse(&bytes) {
        Some(state) => Ok(Loaded::State(state)),
        None => {
            fs::rename(path, broken_path(path))?;
            Ok(Loaded::Broken)
        }
    }
}

fn parse(bytes: &[u8]) -> Option<State> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let v = value.get("v")?.as_u64()?;
    if v == 0 || v > VERSION {
        return None;
    }
    serde_json::from_value(value).ok()
}

/// Writes local state whole to a temporary file and renames it over the old
/// one, so a crash mid-write never leaves half a file (CMP Data).
pub fn save(path: &Path, state: &State) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    {
        let mut file = File::create(&tmp)?;
        file.write_all(&json)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

fn broken_path(path: &Path) -> std::path::PathBuf {
    path.with_extension("json.broken")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CMP Data's example of `state.json`, version 1, verbatim.
    const EXAMPLE: &str = r#"{ "v": 1,
  "manualInstalls": ["G:/World of Warcraft/_classic_beta_"],
  "firstRunDone": true,
  "keptAppImagePaths": [],
  "config": { "foreverProductUid": "wow_classic_beta", "readAt": 1791735000 },
  "settings": { "startAtLogin": true, "gameMode": false },
  "battleTag": "Ada#1234",
  "credentialStore": "secret-service",
  "logs": {
    "3f9a2c1e7b0d4a65": {
      "answer": "upload",
      "folder": "G:/World of Warcraft/_classic_beta_/WTF/Account/104973978#1",
      "characters": "Ada Brook, Kukli Vail",
      "lastFolder": "G:/World of Warcraft/_classic_beta_/WTF/Account/104973978#1",
      "wholeSendPending": false,
      "lastRun": { "result": "uploaded", "at": 1791735100, "fileMtimeMs": 1791735090123, "fileSize": 812345,
                   "format": 1, "companionVersion": "1.0.0", "code": null, "message": null,
                   "catchLog": { "verification": "pending", "pendingReason": "forever-api", "heldCharacters": 0, "leftOff": null } },
      "lastUploadedAt": 1791735100 } },
  "inGame": { "followingAt": 1791735000, "anglersAt": 1791735000, "anglersEtag": "\"9f86d081…\"" } }"#;

    #[test]
    fn the_example_round_trips() {
        // CMP Data, state.json version 1: the example reads and writes back
        // to the same JSON
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        fs::write(&path, EXAMPLE).unwrap();

        let Loaded::State(state) = load(&path).unwrap() else {
            panic!("the example didn't load");
        };
        assert_eq!(state.battle_tag.as_deref(), Some("Ada#1234"));
        assert_eq!(state.credential_store, Some(CredentialStore::SecretService));
        let log = &state.logs["3f9a2c1e7b0d4a65"];
        assert_eq!(log.answer, Some(Answer::Upload));
        assert_eq!(log.last_run.as_ref().unwrap().result, RunResult::Uploaded);

        save(&path, &state).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let example: serde_json::Value = serde_json::from_str(EXAMPLE).unwrap();
        assert_eq!(written, example);
    }

    #[test]
    fn an_awaiting_log_has_no_answer_key() {
        // CMP Data: answer is absent while awaiting (CMP-R40)
        let log = LogState {
            answer: None,
            folder: None,
            characters: "Ada Brook".into(),
            last_folder: "G:/WoW/_classic_beta_/WTF/Account/1#1".into(),
            whole_send_pending: false,
            last_run: None,
            last_uploaded_at: None,
        };
        let json = serde_json::to_value(&log).unwrap();
        assert!(json.get("answer").is_none());
        assert!(json["folder"].is_null());
    }

    #[test]
    fn a_missing_file_is_a_first_run() {
        // CMP-R100: a start whose local state is missing is a first run
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            load(&tmp.path().join("state.json")).unwrap(),
            Loaded::Missing
        ));
    }

    #[test]
    fn a_corrupt_file_becomes_state_json_broken() {
        // CMP Data: a file that can't be parsed is renamed state.json.broken
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        fs::write(&path, "{ \"v\": 1, \"logs\": ").unwrap();

        assert!(matches!(load(&path).unwrap(), Loaded::Broken));
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(tmp.path().join("state.json.broken")).unwrap(),
            "{ \"v\": 1, \"logs\": "
        );
        // The next start is a first run with fresh state.
        assert!(matches!(load(&path).unwrap(), Loaded::Missing));
    }

    #[test]
    fn a_higher_version_becomes_state_json_broken() {
        // CMP Data: a file whose v is higher than this Companion knows is
        // renamed state.json.broken
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        fs::write(&path, r#"{ "v": 2 }"#).unwrap();

        assert!(matches!(load(&path).unwrap(), Loaded::Broken));
        assert!(tmp.path().join("state.json.broken").exists());
    }

    #[test]
    fn a_file_without_v_becomes_state_json_broken() {
        // CMP Data: a file without a version can't be parsed as version 1
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        fs::write(&path, r#"{ "firstRunDone": true }"#).unwrap();

        assert!(matches!(load(&path).unwrap(), Loaded::Broken));
    }

    #[test]
    fn saving_replaces_the_file_whole_and_leaves_no_temporary_file() {
        // CMP Data: written whole to a temporary file and renamed over the
        // old one
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        fs::write(&path, "old").unwrap();

        let state = State {
            first_run_done: true,
            ..State::default()
        };
        save(&path, &state).unwrap();

        let Loaded::State(read) = load(&path).unwrap() else {
            panic!("the saved file didn't load");
        };
        assert_eq!(read, state);
        let names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["state.json"]);
    }

    #[test]
    fn fresh_state_starts_at_login_and_is_not_linked() {
        // CMP-R3: start at login is on by default; CMP Data: battleTag is
        // null while not linked
        let json = serde_json::to_value(State::default()).unwrap();
        assert_eq!(json["v"], 1);
        assert_eq!(json["settings"]["startAtLogin"], true);
        assert!(json["battleTag"].is_null());
    }
}

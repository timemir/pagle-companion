//! The credential store (CMP-R62; CMP Contracts, Credential store entries):
//! where the Companion keeps its one Companion credential. Windows Credential
//! Manager on Windows; on Linux the Secret Service when it takes the
//! credential without asking anything, else the credential file in the state
//! directory.
//!
//! Every store sits behind [`CredentialStore`], so the logic that picks and
//! uses one ([`Keychain`], `link`) is tested with stand-ins. The credential
//! never appears in an error, a log line or a `Debug` output of this module.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

use crate::state::CredentialStore as StoreKind;
use crate::state_dir::StateDir;

#[cfg(target_os = "linux")]
pub mod secret_service;
#[cfg(windows)]
pub mod windows;

/// One place a credential can be kept.
pub trait CredentialStore: Send + Sync {
    /// How CMP-R61's text names this store: "Windows Credential Manager",
    /// "your keyring" or "its settings folder".
    fn place(&self) -> &'static str;
    /// The credential, or `None` when the store holds none.
    fn read(&self) -> Result<Option<String>, StoreError>;
    /// Replaces the credential.
    fn write(&self, credential: &str) -> Result<(), StoreError>;
    /// Deletes the credential; deleting a missing one succeeds.
    fn delete(&self) -> Result<(), StoreError>;
}

/// Why a store couldn't do what it was asked. Never carries the credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The store doesn't answer: the Secret Service is locked, would prompt,
    /// isn't running, or gave no answer within the D-Bus timeout (CMP-R62).
    Unavailable(String),
    /// Any other failure.
    Failed(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Unavailable(why) => write!(f, "the store doesn't answer: {why}"),
            StoreError::Failed(why) => write!(f, "the store failed: {why}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// The credential file (Linux): `<state directory>/credential`, mode `0600`,
/// the credential and one LF.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The credential file of `dir`.
    pub fn in_state_dir(dir: &StateDir) -> Self {
        Self::new(dir.path().join("credential"))
    }
}

impl CredentialStore for FileStore {
    fn place(&self) -> &'static str {
        "its settings folder"
    }

    fn read(&self) -> Result<Option<String>, StoreError> {
        match fs::read_to_string(&self.path) {
            Ok(text) => {
                let credential = text.trim_end_matches(LINE_END);
                Ok((!credential.is_empty()).then(|| credential.to_owned()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(failed("couldn't read the credential file", &e)),
        }
    }

    /// Writes a private temporary file and renames it over the old one, so
    /// the file is never half written and never readable by others.
    fn write(&self, credential: &str) -> Result<(), StoreError> {
        let tmp = self.path.with_extension("tmp");
        let written = (|| {
            let _ = fs::remove_file(&tmp);
            let mut file = private_file_options().open(&tmp)?;
            file.write_all(credential.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, &self.path)
        })();
        written.map_err(|e| {
            let _ = fs::remove_file(&tmp);
            failed("couldn't write the credential file", &e)
        })
    }

    fn delete(&self) -> Result<(), StoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(failed("couldn't delete the credential file", &e)),
        }
    }
}

/// What ends the credential file's one line.
const LINE_END: [char; 2] = ['\n', '\r'];

fn failed(what: &str, e: &io::Error) -> StoreError {
    StoreError::Failed(format!("{what}: {e}"))
}

/// A new file, mode `0600` on Linux.
fn private_file_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// The stores of this platform, and which one holds the credential.
pub struct Keychain {
    /// Windows Credential Manager on Windows, the Secret Service on Linux.
    os: Box<dyn CredentialStore>,
    /// Linux only: the credential file.
    file: Option<Box<dyn CredentialStore>>,
}

impl Keychain {
    /// `file` is `None` where there is no credential file (Windows).
    pub fn new(os: Box<dyn CredentialStore>, file: Option<Box<dyn CredentialStore>>) -> Self {
        Self { os, file }
    }

    /// This platform's stores, with the real entries of CMP Contracts.
    pub fn platform(dir: &StateDir) -> Self {
        #[cfg(windows)]
        {
            let _ = dir;
            Self::new(Box::new(windows::CredentialManager::new()), None)
        }
        #[cfg(target_os = "linux")]
        {
            Self::new(
                Box::new(secret_service::SecretService::new()),
                Some(Box::new(FileStore::in_state_dir(dir))),
            )
        }
    }

    /// Every store of this platform, the one `kind` names first (CMP-R62):
    /// removal reads the credential from the first that holds it and deletes
    /// it from all of them (CMP-R70 steps 1 and 2).
    pub fn every_store(&self, kind: Option<StoreKind>) -> Vec<&dyn CredentialStore> {
        let current = self.current(kind);
        let mut stores = vec![current];
        for store in std::iter::once(self.os.as_ref()).chain(self.file.as_deref()) {
            if !std::ptr::addr_eq(store, current) {
                stores.push(store);
            }
        }
        stores
    }

    /// The store holding the credential, by `credentialStore` in local state
    /// (CMP-R62). Without a credential file, or without a choice, it is the
    /// OS store.
    pub fn current(&self, kind: Option<StoreKind>) -> &dyn CredentialStore {
        match (kind, &self.file) {
            (Some(StoreKind::File), Some(file)) => file.as_ref(),
            _ => self.os.as_ref(),
        }
    }

    /// Stores a new link's credential (CMP-R61, CMP-R62) and returns the
    /// `credentialStore` to keep in local state. On Windows: Windows
    /// Credential Manager. On Linux: the credential file in Game Mode;
    /// otherwise the Secret Service when it takes the credential (it refuses
    /// when locked or when storing would prompt), else the credential file.
    /// A failure names the store that failed, for CMP-R61's text.
    pub fn store_new(
        &self,
        credential: &str,
        game_mode: bool,
    ) -> Result<Option<StoreKind>, &'static str> {
        let Some(file) = &self.file else {
            return match self.os.write(credential) {
                Ok(()) => Ok(None),
                Err(e) => {
                    log::error!("couldn't store the credential: {e}");
                    Err(self.os.place())
                }
            };
        };
        if !game_mode {
            match self.os.write(credential) {
                Ok(()) => return Ok(Some(StoreKind::SecretService)),
                Err(e) => log::info!(
                    "the Secret Service didn't take the credential ({e}); using the credential file"
                ),
            }
        }
        match file.write(credential) {
            Ok(()) => Ok(Some(StoreKind::File)),
            Err(e) => {
                log::error!("couldn't store the credential: {e}");
                Err(file.place())
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! An in-memory store whose answers a test can change.

    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    struct Inner {
        credential: Option<String>,
        fail_reads: Option<StoreError>,
        fail_writes: Option<StoreError>,
        writes: usize,
    }

    #[derive(Clone)]
    pub struct MemoryStore {
        place: &'static str,
        inner: Arc<Mutex<Inner>>,
    }

    impl MemoryStore {
        pub fn new(place: &'static str) -> Self {
            Self {
                place,
                inner: Arc::default(),
            }
        }

        pub fn credential(&self) -> Option<String> {
            self.lock().credential.clone()
        }

        pub fn fail_reads(&self, error: Option<StoreError>) {
            self.lock().fail_reads = error;
        }

        pub fn fail_writes(&self, error: Option<StoreError>) {
            self.lock().fail_writes = error;
        }

        pub fn writes(&self) -> usize {
            self.lock().writes
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
            self.inner.lock().unwrap()
        }
    }

    impl CredentialStore for MemoryStore {
        fn place(&self) -> &'static str {
            self.place
        }

        fn read(&self) -> Result<Option<String>, StoreError> {
            let inner = self.lock();
            match &inner.fail_reads {
                Some(e) => Err(e.clone()),
                None => Ok(inner.credential.clone()),
            }
        }

        fn write(&self, credential: &str) -> Result<(), StoreError> {
            let mut inner = self.lock();
            if let Some(e) = &inner.fail_writes {
                return Err(e.clone());
            }
            inner.writes += 1;
            inner.credential = Some(credential.to_owned());
            Ok(())
        }

        fn delete(&self) -> Result<(), StoreError> {
            self.lock().credential = None;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::MemoryStore;
    use super::*;

    const CREDENTIAL: &str = "pgc_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";

    fn locked() -> StoreError {
        StoreError::Unavailable("locked".into())
    }

    #[test]
    fn the_credential_file_holds_the_credential_and_one_lf() {
        // CMP Contracts, Credential store entries: <state directory>/credential,
        // the credential and one LF
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::open(tmp.path()).unwrap();
        let store = FileStore::in_state_dir(&dir);

        store.write(CREDENTIAL).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("credential")).unwrap(),
            format!("{CREDENTIAL}\n")
        );
        assert_eq!(store.read().unwrap().as_deref(), Some(CREDENTIAL));
    }

    #[test]
    fn writing_the_credential_file_replaces_it_whole() {
        // CMP-R49: the successor replaces the credential in the same store
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path().join("credential"));

        store.write(CREDENTIAL).unwrap();
        store.write("pgc_successor").unwrap();
        assert_eq!(store.read().unwrap().as_deref(), Some("pgc_successor"));
        let names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["credential"]);
    }

    #[test]
    fn a_missing_credential_file_holds_no_credential() {
        // CMP-R62: a store that answers and holds no credential
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path().join("credential"));
        assert_eq!(store.read().unwrap(), None);
    }

    #[test]
    fn deleting_the_credential_file_removes_it_and_a_second_delete_succeeds() {
        // CMP-R63 step 3: deletes the credential from its store
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path().join("credential"));
        store.write(CREDENTIAL).unwrap();

        store.delete().unwrap();
        assert!(!tmp.path().join("credential").exists());
        store.delete().unwrap();
        assert_eq!(store.read().unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_credential_file_is_private() {
        // CMP Contracts, Credential store entries: mode 0600
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credential");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        FileStore::new(&path).write(CREDENTIAL).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn the_file_store_names_its_settings_folder() {
        // CMP-R61: {store} is "its settings folder" for the credential file
        assert_eq!(FileStore::new("credential").place(), "its settings folder");
    }

    fn linux_keychain() -> (MemoryStore, MemoryStore, Keychain) {
        let secret_service = MemoryStore::new("your keyring");
        let file = MemoryStore::new("its settings folder");
        let keychain = Keychain::new(
            Box::new(secret_service.clone()),
            Some(Box::new(file.clone())),
        );
        (secret_service, file, keychain)
    }

    #[test]
    fn windows_keeps_the_credential_in_credential_manager() {
        // CMP-R62: Windows: Windows Credential Manager; credentialStore is
        // Linux only
        let manager = MemoryStore::new("Windows Credential Manager");
        let keychain = Keychain::new(Box::new(manager.clone()), None);

        assert_eq!(keychain.store_new(CREDENTIAL, false), Ok(None));
        assert_eq!(manager.credential().as_deref(), Some(CREDENTIAL));
        assert_eq!(keychain.current(None).place(), "Windows Credential Manager");
    }

    #[test]
    fn a_failed_credential_manager_write_names_it() {
        // CMP-R61: "couldn't save its link in Windows Credential Manager"
        let manager = MemoryStore::new("Windows Credential Manager");
        manager.fail_writes(Some(StoreError::Failed("denied".into())));
        let keychain = Keychain::new(Box::new(manager), None);

        assert_eq!(
            keychain.store_new(CREDENTIAL, false),
            Err("Windows Credential Manager")
        );
    }

    #[test]
    fn linux_prefers_an_unlocked_secret_service() {
        // CMP-R62: the Secret Service when its default collection is already
        // unlocked and storing asks no prompt
        let (secret_service, file, keychain) = linux_keychain();

        assert_eq!(
            keychain.store_new(CREDENTIAL, false),
            Ok(Some(StoreKind::SecretService))
        );
        assert_eq!(secret_service.credential().as_deref(), Some(CREDENTIAL));
        assert_eq!(file.writes(), 0);
    }

    #[test]
    fn a_locked_keyring_at_link_time_chooses_the_file() {
        // CMP-R62: otherwise the credential file
        let (secret_service, file, keychain) = linux_keychain();
        secret_service.fail_writes(Some(locked()));

        assert_eq!(
            keychain.store_new(CREDENTIAL, false),
            Ok(Some(StoreKind::File))
        );
        assert_eq!(file.credential().as_deref(), Some(CREDENTIAL));
        assert_eq!(secret_service.credential(), None);
    }

    #[test]
    fn game_mode_always_chooses_the_file() {
        // CMP-R62: with "Keep uploading in Game Mode" on, the credential file
        let (secret_service, file, keychain) = linux_keychain();

        assert_eq!(
            keychain.store_new(CREDENTIAL, true),
            Ok(Some(StoreKind::File))
        );
        assert_eq!(secret_service.writes(), 0);
        assert_eq!(file.credential().as_deref(), Some(CREDENTIAL));
    }

    #[test]
    fn a_failed_file_write_names_the_settings_folder() {
        // CMP-R61: "couldn't save its link in its settings folder"
        let (secret_service, file, keychain) = linux_keychain();
        secret_service.fail_writes(Some(locked()));
        file.fail_writes(Some(StoreError::Failed("read-only".into())));

        assert_eq!(
            keychain.store_new(CREDENTIAL, false),
            Err("its settings folder")
        );
    }

    #[test]
    fn credential_store_picks_the_store_read() {
        // CMP-R62: credentialStore in local state names the store holding the
        // credential
        let (_, _, keychain) = linux_keychain();
        assert_eq!(
            keychain.current(Some(StoreKind::SecretService)).place(),
            "your keyring"
        );
        assert_eq!(
            keychain.current(Some(StoreKind::File)).place(),
            "its settings folder"
        );
    }

    #[test]
    fn store_errors_never_carry_the_credential() {
        // CMP-R62: the credential never appears in the local log file
        let tmp = tempfile::tempdir().unwrap();
        // A folder where the file should be makes every write fail.
        let path = tmp.path().join("credential");
        fs::create_dir(&path).unwrap();
        let error = FileStore::new(&path).write(CREDENTIAL).unwrap_err();
        assert!(!error.to_string().contains(CREDENTIAL), "{error}");
    }
}

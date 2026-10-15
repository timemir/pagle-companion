//! Windows Credential Manager (CMP Contracts, Credential store entries): a
//! generic credential for the current user, target name
//! `pagle-companion:credential`, user name `pagle`, persistence local
//! machine. The blob is the credential's UTF-8 bytes.

use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
use windows_sys::Win32::Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW,
    CredWriteW,
};

use super::{CredentialStore, StoreError};

/// The entry's target name.
pub const TARGET: &str = "pagle-companion:credential";

/// The entry's user name.
pub const USER: &str = "pagle";

pub struct CredentialManager {
    target: String,
}

impl CredentialManager {
    /// The Companion's own entry.
    pub fn new() -> Self {
        Self::with_target(TARGET)
    }

    /// Another entry: tests use their own, never the real one.
    pub fn with_target(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
        }
    }
}

impl Default for CredentialManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialStore for CredentialManager {
    fn place(&self) -> &'static str {
        "Windows Credential Manager"
    }

    fn read(&self) -> Result<Option<String>, StoreError> {
        let Some(entry) = Entry::read(&self.target)? else {
            return Ok(None);
        };
        let blob = entry.blob();
        String::from_utf8(blob.to_vec())
            .map(|credential| (!credential.is_empty()).then_some(credential))
            .map_err(|_| StoreError::Failed("the stored credential isn't UTF-8".into()))
    }

    fn write(&self, credential: &str) -> Result<(), StoreError> {
        let mut target = wide(&self.target);
        let mut user = wide(USER);
        let mut blob = credential.as_bytes().to_vec();
        let entry = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: user.as_mut_ptr(),
            // SAFETY: CREDENTIALW is plain data; the other fields stay empty.
            ..unsafe { std::mem::zeroed() }
        };
        // SAFETY: every pointer in `entry` points into a buffer that outlives
        // the call.
        if unsafe { CredWriteW(&entry, 0) } == 0 {
            return Err(last_error("CredWriteW"));
        }
        Ok(())
    }

    fn delete(&self) -> Result<(), StoreError> {
        let target = wide(&self.target);
        // SAFETY: `target` is a NUL-terminated UTF-16 string.
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            // SAFETY: no other call in between.
            if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                return Ok(());
            }
            return Err(last_error("CredDeleteW"));
        }
        Ok(())
    }
}

/// A credential read with `CredReadW`, freed when dropped.
struct Entry(*mut CREDENTIALW);

impl Entry {
    fn read(target: &str) -> Result<Option<Self>, StoreError> {
        let target = wide(target);
        let mut entry: *mut CREDENTIALW = ptr::null_mut();
        // SAFETY: `target` is a NUL-terminated UTF-16 string and `entry` a
        // place for the returned pointer.
        if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut entry) } == 0 {
            // SAFETY: no other call in between.
            if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                return Ok(None);
            }
            return Err(last_error("CredReadW"));
        }
        Ok(Some(Self(entry)))
    }

    fn get(&self) -> &CREDENTIALW {
        // SAFETY: a successful CredReadW returned a valid pointer, kept
        // until drop.
        unsafe { &*self.0 }
    }

    fn blob(&self) -> &[u8] {
        let entry = self.get();
        if entry.CredentialBlob.is_null() || entry.CredentialBlobSize == 0 {
            return &[];
        }
        // SAFETY: Credential Manager hands back a blob of the given size.
        unsafe {
            std::slice::from_raw_parts(entry.CredentialBlob, entry.CredentialBlobSize as usize)
        }
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        // SAFETY: the pointer came from CredReadW and is freed once.
        unsafe { CredFree(self.0.cast()) };
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error(call: &str) -> StoreError {
    StoreError::Failed(format!(
        "{call} failed: {}",
        std::io::Error::last_os_error()
    ))
}

/// The user name and persistence of an entry, for tests.
#[cfg(test)]
fn read_attributes(target: &str) -> Result<(String, u32), StoreError> {
    let entry = Entry::read(target)?.ok_or(StoreError::Failed("no entry".into()))?;
    let entry = entry.get();
    // SAFETY: UserName is a NUL-terminated UTF-16 string or null.
    let user = unsafe {
        if entry.UserName.is_null() {
            String::new()
        } else {
            let len = (0..).take_while(|&i| *entry.UserName.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(entry.UserName, len))
        }
    };
    Ok((user, entry.Persist))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Credential Manager sometimes loses one of two writes that one process
    /// makes to different entries at the same moment (seen as a read right
    /// after a write finding nothing), so these tests take turns.
    static CREDENTIAL_MANAGER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A test-only entry, deleted when dropped, so tests never touch the
    /// Companion's real `pagle-companion:credential`. It holds the turn until
    /// then.
    struct TestEntry(
        CredentialManager,
        #[allow(dead_code)] std::sync::MutexGuard<'static, ()>,
    );

    impl TestEntry {
        fn new(name: &str) -> Self {
            let turn = CREDENTIAL_MANAGER.lock().unwrap_or_else(|e| e.into_inner());
            let target = format!("pagle-companion-test:{name}:{}", std::process::id());
            Self(CredentialManager::with_target(target), turn)
        }
    }

    impl Drop for TestEntry {
        fn drop(&mut self) {
            let _ = self.0.delete();
        }
    }

    #[test]
    fn the_entry_is_the_contracts_generic_credential() {
        // CMP Contracts, Credential store entries: target name
        // pagle-companion:credential, user name pagle
        assert_eq!(TARGET, "pagle-companion:credential");
        assert_eq!(USER, "pagle");
        assert_eq!(CredentialManager::new().target, TARGET);
    }

    #[test]
    fn credential_manager_keeps_reads_back_and_deletes_the_credential() {
        // CMP-R62: Windows keeps the credential in Windows Credential Manager
        let entry = TestEntry::new("round-trip");
        assert_eq!(entry.0.read().unwrap(), None);

        entry.0.write("pgc_first").unwrap();
        assert_eq!(entry.0.read().unwrap().as_deref(), Some("pgc_first"));

        // CMP-R49: the successor replaces it in the same entry
        entry.0.write("pgc_successor").unwrap();
        assert_eq!(entry.0.read().unwrap().as_deref(), Some("pgc_successor"));

        // CMP-R63 step 3: deleted; a second delete still succeeds
        entry.0.delete().unwrap();
        assert_eq!(entry.0.read().unwrap(), None);
        entry.0.delete().unwrap();
    }

    #[test]
    fn the_entry_is_kept_for_the_local_machine_under_user_pagle() {
        // CMP Contracts, Credential store entries: user name pagle,
        // persistence local machine
        let entry = TestEntry::new("persistence");
        entry.0.write("pgc_x").unwrap();

        let (user, persist) = read_attributes(&entry.0.target).unwrap();
        assert_eq!(user, USER);
        assert_eq!(
            persist,
            windows_sys::Win32::Security::Credentials::CRED_PERSIST_LOCAL_MACHINE
        );
    }
}

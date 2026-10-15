//! The Windows entry (CMP-R3): the string value `Pagle Companion` under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, the value
//! `tauri-plugin-autostart` writes (it names it after the product). The
//! Companion writes it itself because the plugin (2.7, through `auto-launch`
//! 0.6) tries `HKLM` before `HKCU`, leaves the path unquoted, and on every
//! enable overrides a Player's "Disabled" in Task Manager.
//!
//! Task Manager keeps that "Disabled" as a value of the same name under
//! `…\Explorer\StartupApproved\Run`. Deleting the entry deletes it too, so a
//! toggle turned off and on again starts the Companion at login, and removal
//! leaves nothing behind (CMP-R70).

use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};

use super::Entry;

/// The current user's `Run` key.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Where Task Manager keeps a startup program's "Disabled".
pub const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// The value's name: the product name, as `tauri-plugin-autostart` names it.
pub const VALUE_NAME: &str = "Pagle Companion";

/// A value under the current user's registry hive.
pub struct RunValue {
    run_key: String,
    approved_key: String,
    name: String,
}

impl RunValue {
    /// The Companion's own entry.
    pub fn new() -> Self {
        Self::under(RUN_KEY, APPROVED_KEY)
    }

    /// The same value under other keys: tests use their own, never the real
    /// `Run` key.
    pub fn under(run_key: &str, approved_key: &str) -> Self {
        Self {
            run_key: run_key.into(),
            approved_key: approved_key.into(),
            name: VALUE_NAME.into(),
        }
    }
}

impl Default for RunValue {
    fn default() -> Self {
        Self::new()
    }
}

impl Entry for RunValue {
    fn read(&self) -> std::io::Result<Option<String>> {
        let key = wide(&self.run_key);
        let name = wide(&self.name);
        let mut buffer: Vec<u16> = vec![0; 260];
        loop {
            let mut bytes = u32::try_from(buffer.len() * 2).unwrap_or(u32::MAX);
            // SAFETY: the strings are NUL-terminated; `buffer` holds `bytes`
            // bytes, which RegGetValueW updates to what it wrote or needs.
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    name.as_ptr(),
                    RRF_RT_REG_SZ,
                    ptr::null_mut(),
                    buffer.as_mut_ptr().cast(),
                    &mut bytes,
                )
            };
            match status {
                ERROR_SUCCESS => {
                    buffer.truncate(bytes as usize / 2);
                    while buffer.last() == Some(&0) {
                        buffer.pop();
                    }
                    return Ok(Some(String::from_utf16_lossy(&buffer)));
                }
                ERROR_FILE_NOT_FOUND => return Ok(None),
                ERROR_MORE_DATA => buffer.resize(bytes as usize / 2 + 1, 0),
                error => return Err(os_error(error)),
            }
        }
    }

    fn write(&self, command: &str) -> std::io::Result<()> {
        let key = wide(&self.run_key);
        let name = wide(&self.name);
        let data = wide(command);
        let bytes = u32::try_from(data.len() * 2).map_err(std::io::Error::other)?;
        // SAFETY: the strings are NUL-terminated and `data` holds `bytes`
        // bytes. RegSetKeyValueW creates the key if it is missing.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                bytes,
            )
        };
        match status {
            ERROR_SUCCESS => Ok(()),
            error => Err(os_error(error)),
        }
    }

    fn delete(&self) -> std::io::Result<()> {
        delete_value(&self.run_key, &self.name)?;
        delete_value(&self.approved_key, &self.name)
    }
}

fn delete_value(key: &str, name: &str) -> std::io::Result<()> {
    let key = wide(key);
    let name = wide(name);
    // SAFETY: the strings are NUL-terminated.
    let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
    match status {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
        error => Err(os_error(error)),
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn os_error(code: u32) -> std::io::Error {
    std::io::Error::from_raw_os_error(code as i32)
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::System::Registry::{RegDeleteKeyW, RegDeleteTreeW};

    use super::*;

    /// A test-only key `HKCU\Software\pagle-companion-test-<pid>-<name>`,
    /// deleted when dropped, so tests never touch the real `Run` key. Each
    /// test has its own key and no parent is shared, so one test's cleanup
    /// never deletes a key another is writing under.
    struct TestKey(String);

    impl TestKey {
        fn new(name: &str) -> Self {
            Self(format!(
                r"Software\pagle-companion-test-{}-{name}",
                std::process::id()
            ))
        }

        fn value(&self) -> RunValue {
            RunValue::under(
                &format!(r"{}\Run", self.0),
                &format!(r"{}\StartupApproved\Run", self.0),
            )
        }
    }

    impl Drop for TestKey {
        fn drop(&mut self) {
            let own = wide(&self.0);
            // SAFETY: the string is NUL-terminated. RegDeleteTreeW empties the
            // key, RegDeleteKeyW deletes it.
            unsafe {
                RegDeleteTreeW(HKEY_CURRENT_USER, own.as_ptr());
                RegDeleteKeyW(HKEY_CURRENT_USER, own.as_ptr());
            }
        }
    }

    #[test]
    fn the_entry_is_the_product_named_value_under_the_users_run_key() {
        // CMP-R3: the Tauri autostart entry under the current user
        assert_eq!(RUN_KEY, r"Software\Microsoft\Windows\CurrentVersion\Run");
        assert_eq!(VALUE_NAME, "Pagle Companion");
    }

    #[test]
    fn a_written_command_reads_back() {
        // CMP-R3: the HKCU Run entry starts "<install path>\pagle-companion.exe"
        // --autostart
        let key = TestKey::new("roundtrip");
        let value = key.value();
        assert_eq!(value.read().unwrap(), None);
        let command = super::super::windows_command(std::path::Path::new(
            r"C:\Users\Tim Smith\AppData\Local\Pagle Companion\pagle-companion.exe",
        ));
        value.write(&command).unwrap();
        assert_eq!(value.read().unwrap(), Some(command));
    }

    #[test]
    fn a_long_command_reads_back_whole() {
        // CMP-R91: the entry's path is compared whole, however long
        let key = TestKey::new("long");
        let value = key.value();
        let command = format!(
            "\"C:\\{}\\pagle-companion.exe\" --autostart",
            "x".repeat(600)
        );
        value.write(&command).unwrap();
        assert_eq!(value.read().unwrap(), Some(command));
    }

    #[test]
    fn deleting_removes_the_value_and_task_managers_disabled() {
        // CMP-R3: turning the toggle off deletes the entry; CMP-R70 leaves
        // nothing behind
        let key = TestKey::new("delete");
        let value = key.value();
        value.write("\"p.exe\" --autostart").unwrap();
        let disabled = RunValue::under(&format!(r"{}\StartupApproved\Run", key.0), "unused");
        disabled.write("disabled").unwrap();

        value.delete().unwrap();
        assert_eq!(value.read().unwrap(), None);
        assert_eq!(disabled.read().unwrap(), None);
        // Deleting a missing entry succeeds.
        value.delete().unwrap();
    }
}

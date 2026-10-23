//! The local log file, `companion.log` (CMP-R112).
//!
//! It holds scans, runs and their results, and errors. It never holds the
//! credential, the BattleTag, entry text or character names; `logId`s, paths
//! and refusal codes are allowed. Callers log through the `log` macros.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// The local log size (CMP Tunables): a file is rotated before it would grow
/// past it.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// `companion.log`, rotated into `companion.log.1` and `companion.log.2`:
/// three files in all (CMP Tunables, the local log files count).
pub struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    /// `None` until the first line, and again once closed.
    inner: Mutex<Option<Open>>,
    closed: AtomicBool,
}

struct Open {
    file: File,
    len: u64,
}

impl RotatingFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_max_bytes(path, MAX_FILE_BYTES)
    }

    fn with_max_bytes(path: impl Into<PathBuf>, max_bytes: u64) -> Self {
        Self {
            path: path.into(),
            max_bytes,
            inner: Mutex::new(None),
            closed: AtomicBool::new(false),
        }
    }

    /// Lets go of the file for good: later lines are dropped, and the file is
    /// never opened (or created) again. Removal closes it before it deletes
    /// the state directory (CMP-R70 step 5), so the directory can go.
    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.closed.store(true, Ordering::SeqCst);
        *inner = None;
    }

    /// Appends one line, rotating first if it would take the file past the
    /// local log size.
    pub fn write_line(&self, line: &str) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let bytes = line.len() as u64 + 1;
        if inner.is_none() {
            *inner = Some(self.open()?);
        }
        let len = inner.as_ref().map_or(0, |open| open.len);
        if len > 0 && len + bytes > self.max_bytes {
            *inner = None;
            self.rotate()?;
            *inner = Some(self.open()?);
        }
        let open = inner.as_mut().expect("opened above");
        open.file.write_all(line.as_bytes())?;
        open.file.write_all(b"\n")?;
        open.len += bytes;
        Ok(())
    }

    fn open(&self) -> io::Result<Open> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let len = file.metadata()?.len();
        Ok(Open { file, len })
    }

    fn rotate(&self) -> io::Result<()> {
        let first = numbered(&self.path, 1);
        let second = numbered(&self.path, 2);
        remove_if_present(&second)?;
        if first.exists() {
            fs::rename(&first, &second)?;
        }
        fs::rename(&self.path, &first)
    }
}

fn numbered(path: &Path, n: u8) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The `log` facade's sink: every record becomes one line of the local log
/// file, in UTC.
pub struct Logger {
    file: RotatingFile,
}

impl Logger {
    /// Sends the `log` macros of the whole process to `path`. Only the first
    /// call in a process takes effect.
    pub fn install(path: impl Into<PathBuf>) {
        if INSTALLED.get().is_some() {
            return;
        }
        let logger: &'static Logger = Box::leak(Box::new(Self {
            file: RotatingFile::new(path),
        }));
        if log::set_logger(logger).is_ok() {
            let _ = INSTALLED.set(logger);
            log::set_max_level(log::LevelFilter::Info);
        }
    }

    /// Closes the installed local log file for the rest of the process
    /// ([`RotatingFile::close`]). Without one it does nothing.
    pub fn close() {
        if let Some(logger) = INSTALLED.get() {
            logger.file.close();
        }
    }
}

/// The logger [`Logger::install`] installed in this process.
static INSTALLED: OnceLock<&'static Logger> = OnceLock::new();

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {:<5} {}",
            humantime::format_rfc3339_seconds(SystemTime::now()),
            record.level(),
            one_line(&record.args().to_string())
        );
        // Nowhere to report a failure to write the log itself.
        let _ = self.file.write_line(&line);
    }

    fn flush(&self) {}
}

/// Whether `c` breaks a line or reorders text without being a control
/// character: the line and paragraph separators, and the bidirectional
/// marks, embeddings, overrides and isolates.
fn is_layout_control(c: char) -> bool {
    matches!(
        c,
        '\u{2028}'
            | '\u{2029}'
            | '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// The most characters of one record's message in the local log file.
pub const MAX_MESSAGE_CHARS: usize = 4096;

/// A record's message as one line of the local log file: every control
/// character, line or paragraph separator and bidirectional control escaped
/// (`\n`, `\u{1b}`, `\u{2028}`, `\u{202e}`), so text a server or a file
/// supplied, such as a refusal's message, can't add lines that look like the
/// Companion's own or reorder how a line reads; and cut to
/// [`MAX_MESSAGE_CHARS`], so it can't push the rest of the log out
/// (CMP-R112).
pub fn one_line(message: &str) -> String {
    let mut line = String::with_capacity(message.len().min(MAX_MESSAGE_CHARS));
    for (n, c) in message.chars().enumerate() {
        if n == MAX_MESSAGE_CHARS {
            line.push('…');
            break;
        }
        if c.is_control() || is_layout_control(c) {
            line.extend(c.escape_default());
        } else {
            line.push(c);
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(len: usize) -> String {
        "x".repeat(len - 1) // plus the line feed
    }

    #[test]
    fn rotates_at_the_local_log_size_into_dot_1_and_dot_2() {
        // CMP-R112: companion.log rotated at the local log size into
        // companion.log.1 and companion.log.2
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("companion.log");
        let log = RotatingFile::with_max_bytes(&path, 100);

        log.write_line(&line(60)).unwrap();
        log.write_line(&line(40)).unwrap(); // exactly 100: no rotation yet
        assert_eq!(fs::metadata(&path).unwrap().len(), 100);
        assert!(!numbered(&path, 1).exists());

        log.write_line("a").unwrap(); // would pass 100: rotates first
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\n");
        assert_eq!(fs::metadata(numbered(&path, 1)).unwrap().len(), 100);
    }

    #[test]
    fn a_message_is_one_line_however_it_was_made() {
        // CMP-R112: a refusal's message is the server's text; it can't add a
        // line of its own
        assert_eq!(
            one_line("refused: bad\n2026-10-23T00:00:00Z INFO  linked\r\u{1b}[2J"),
            r"refused: bad\n2026-10-23T00:00:00Z INFO  linked\r\u{1b}[2J"
        );
        assert_eq!(one_line("tabs\tstay readable"), r"tabs\tstay readable");
        assert_eq!(
            one_line("fake\u{2028}line \u{202e}desrever"),
            r"fake\u{2028}line \u{202e}desrever"
        );
        assert_eq!(one_line("plain é text"), "plain é text");
    }

    #[test]
    fn a_long_message_is_cut() {
        // CMP-R112: one message can't push the rest of the log out
        let line = one_line(&"x".repeat(MAX_MESSAGE_CHARS + 10));
        assert_eq!(line.chars().count(), MAX_MESSAGE_CHARS + 1);
        assert!(line.ends_with('…'));
        assert_eq!(
            one_line(&"x".repeat(MAX_MESSAGE_CHARS)),
            "x".repeat(MAX_MESSAGE_CHARS)
        );
    }

    #[test]
    fn keeps_three_files_in_all() {
        // CMP Tunables: local log files count, 3 files in all
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("companion.log");
        let log = RotatingFile::with_max_bytes(&path, 10);

        for n in 0..5 {
            log.write_line(&format!("line {n} ..")).unwrap();
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "line 4 ..\n");
        assert_eq!(
            fs::read_to_string(numbered(&path, 1)).unwrap(),
            "line 3 ..\n"
        );
        assert_eq!(
            fs::read_to_string(numbered(&path, 2)).unwrap(),
            "line 2 ..\n"
        );
        let mut names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["companion.log", "companion.log.1", "companion.log.2"]
        );
    }

    #[test]
    fn appends_to_the_file_a_previous_process_left() {
        // CMP-R112: one local log file across starts, rotated by size
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("companion.log");
        fs::write(&path, "earlier\n").unwrap();

        RotatingFile::new(&path).write_line("later").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "earlier\nlater\n");
    }

    #[test]
    fn a_closed_file_takes_no_more_lines_and_can_be_deleted() {
        // CMP-R70 step 5: the local log file goes with the state directory;
        // nothing writes it again once closed
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("pagle-companion");
        fs::create_dir(&dir).unwrap();
        let path = dir.join("companion.log");
        let log = RotatingFile::new(&path);
        log.write_line("before").unwrap();

        log.close();
        fs::remove_dir_all(&dir).unwrap();
        log.write_line("after").unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn the_default_size_is_one_mebibyte() {
        // CMP Tunables: local log size, 1 MiB per file
        assert_eq!(MAX_FILE_BYTES, 1_048_576);
    }
}

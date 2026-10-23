//! The data addon folders (CMP-R80, CMP-R83, CMP-R85; IGV-R40, IGV Contracts
//! "The addon folders"; HEV-R50, HEV Contracts "The addon folder"): which
//! installs get `Interface/AddOns/Pagle_Following/`, `Pagle_Anglers/` and
//! `Pagle_Content/`, each folder's TOC, atomic writes of a checked body as the
//! data file, the TOC refresh, the start sweep of leftover `.tmp` files, and
//! deleting the three folders.
//!
//! Everything here works on flavor folders, with the writes and deletes going
//! through the [`Fs`] seam, so it runs on temporary folder trees and can have
//! faults injected in tests. The [`Writer`] serialises the changes and keeps
//! the write failures the window shows; [`window`] connects it to the app.
//!
//! It writes and deletes only inside the three folders (CMP-R4), and reads only
//! each install's `Pagle/Pagle.toc` and folder listings besides. An install
//! may be writable by other OS users (CMP-R132), so a data addon folder is
//! written only while it is a folder of its own, never through a link
//! ([`Pinned`]).

pub mod window;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::in_game::check::ANGLERS_CAP;
use crate::installs::paths;

/// The most bytes of a TOC read: `Pagle/Pagle.toc`, or one this writer
/// wrote. Real ones are a few hundred.
const TOC_CAP: u64 = 64 * 1024;

/// The most bytes of a data file read to hash it: no download larger than
/// the Anglers cap is ever written (CMP-R82).
const DATA_FILE_CAP: u64 = ANGLERS_CAP as u64;

/// One of the three data addon folders and its data file (IGV Contracts, The
/// addon folders; HEV Contracts, The addon folder).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DataFile {
    /// `Pagle_Following/Following.lua`: `GET /api/v1/in-game/following`.
    Following,
    /// `Pagle_Anglers/Anglers.lua`: `GET /api/v1/in-game/anglers`.
    Anglers,
    /// `Pagle_Content/Content.lua`: `GET /api/v1/in-game/content` (HEV-R50).
    Content,
}

impl DataFile {
    pub const ALL: [DataFile; 3] = [Self::Following, Self::Anglers, Self::Content];

    /// The folder's name in `Interface/AddOns/`.
    pub fn folder(self) -> &'static str {
        match self {
            Self::Following => "Pagle_Following",
            Self::Anglers => "Pagle_Anglers",
            Self::Content => "Pagle_Content",
        }
    }

    /// The data file's name.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Following => "Following.lua",
            Self::Anglers => "Anglers.lua",
            Self::Content => "Content.lua",
        }
    }

    /// The TOC's name.
    pub fn toc_name(self) -> &'static str {
        match self {
            Self::Following => "Pagle_Following.toc",
            Self::Anglers => "Pagle_Anglers.toc",
            Self::Content => "Pagle_Content.toc",
        }
    }

    /// The TOC template's lines after the two copied ones (IGV Contracts, The
    /// TOCs; for `Pagle_Content`, HEV Contracts, The addon folder), LF line
    /// ends, the last one included.
    fn toc_rest(self) -> &'static str {
        match self {
            Self::Following => concat!(
                "## Title: Pagle Following\n",
                "## Notes: Anglers you follow on the Pagle website. Written by the Pagle Companion.\n",
                "## Group: Pagle\n",
                "## Category: Professions\n",
                "## Dependencies: Pagle\n",
                "## Version: 1\n",
                "\n",
                "Following.lua\n",
            ),
            Self::Anglers => concat!(
                "## Title: Pagle Anglers\n",
                "## Notes: Published Anglers in your regions, loaded when you first hover a player. Written by the Pagle Companion.\n",
                "## Group: Pagle\n",
                "## Category: Professions\n",
                "## Dependencies: Pagle\n",
                "## LoadOnDemand: 1\n",
                "## Version: 1\n",
                "\n",
                "Anglers.lua\n",
            ),
            // Loaded normally, after Pagle, so the data file has run before
            // Core's PLAYER_LOGIN (HEV-R50, PUSH-R1).
            Self::Content => concat!(
                "## Title: Pagle Content\n",
                "## Notes: Pagle Wagers and other content from Pagle. Written by the Pagle Companion.\n",
                "## Group: Pagle\n",
                "## Category: Professions\n",
                "## Dependencies: Pagle\n",
                "## Version: 1\n",
                "\n",
                "Content.lua\n",
            ),
        }
    }
}

/// The lines a TOC copies from `Pagle.toc`, without their line ends
/// (CMP-R83).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopiedLines {
    pub interface: Option<Vec<u8>>,
    pub allow_load_game_type: Option<Vec<u8>>,
}

const BOM: &[u8] = b"\xEF\xBB\xBF";
const INTERFACE: &[u8] = b"## Interface:";
const ALLOW_LOAD_GAME_TYPE: &[u8] = b"## AllowLoadGameType:";

/// The `## Interface:` and `## AllowLoadGameType:` lines of a `Pagle.toc`
/// (CMP-R83, IGV Contracts, The TOCs): after stripping a leading UTF-8 BOM,
/// the first line that starts exactly with each, verbatim without its line
/// end (LF, or CRLF). Variants such as `## Interface-Classic:` never match.
pub fn copied_lines(pagle_toc: &[u8]) -> CopiedLines {
    let text = pagle_toc.strip_prefix(BOM).unwrap_or(pagle_toc);
    let mut lines = CopiedLines::default();
    for line in text.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if lines.interface.is_none() && line.starts_with(INTERFACE) {
            lines.interface = Some(line.to_vec());
        }
        if lines.allow_load_game_type.is_none() && line.starts_with(ALLOW_LOAD_GAME_TYPE) {
            lines.allow_load_game_type = Some(line.to_vec());
        }
    }
    lines
}

/// A folder's TOC (IGV Contracts, The TOCs; CMP-R83): its template with the
/// copied lines, a line `Pagle.toc` lacks left out. LF line ends, no BOM.
pub fn toc(file: DataFile, lines: &CopiedLines) -> Vec<u8> {
    let mut toc = Vec::new();
    for line in [&lines.interface, &lines.allow_load_game_type]
        .into_iter()
        .flatten()
    {
        toc.extend_from_slice(line);
        toc.push(b'\n');
    }
    toc.extend_from_slice(file.toc_rest().as_bytes());
    toc
}

/// The lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The `ETag` the server sends for a data file of these bytes: its lowercase
/// hex SHA-256 in double quotes (IGV-R32, CMP-R81).
pub fn etag_of(bytes: &[u8]) -> String {
    format!("\"{}\"", sha256_hex(bytes))
}

/// What a write or delete acts on: `shown`, the path the window and the log
/// name it by, and the path the OS resolves. The two are the same except
/// inside a [`Pinned`] folder on Linux, where the OS path goes through the
/// folder's descriptor.
pub struct Target {
    pub shown: PathBuf,
    at: PathBuf,
}

impl Target {
    /// `path` itself, resolved as it is.
    fn plain(path: &Path) -> Self {
        Self {
            shown: path.to_path_buf(),
            at: path.to_path_buf(),
        }
    }
}

/// The writes and deletes the writer makes, so tests can inject faults
/// (a read-only folder doesn't fail writes on Windows, or for root).
pub trait Fs: Send + Sync {
    /// Creates `target`, which must not exist yet, writes `bytes` whole and
    /// flushes them to disk. Nothing already there is followed or written
    /// through.
    fn write_synced(&self, target: &Target, bytes: &[u8]) -> io::Result<()>;
    /// Renames `from` over `to`.
    fn rename(&self, from: &Target, to: &Target) -> io::Result<()>;
    /// Creates one folder.
    fn create_dir(&self, target: &Target) -> io::Result<()>;
    fn remove_file(&self, target: &Target) -> io::Result<()>;
    fn remove_dir_all(&self, target: &Target) -> io::Result<()>;
}

/// The file system itself.
pub struct RealFs;

impl Fs for RealFs {
    fn write_synced(&self, target: &Target, bytes: &[u8]) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target.at)?;
        file.write_all(bytes)?;
        file.sync_all()
    }

    fn rename(&self, from: &Target, to: &Target) -> io::Result<()> {
        fs::rename(&from.at, &to.at)
    }

    fn create_dir(&self, target: &Target) -> io::Result<()> {
        fs::create_dir(&target.at)
    }

    fn remove_file(&self, target: &Target) -> io::Result<()> {
        fs::remove_file(&target.at)
    }

    fn remove_dir_all(&self, target: &Target) -> io::Result<()> {
        fs::remove_dir_all(&target.at)
    }
}

/// A data addon folder, held while it is written or swept (CMP-R83).
///
/// Another OS user who can write to the install (CMP-R132) could put a link
/// where a data addon folder belongs: a symlink, or on Windows a junction,
/// which needs no privilege. Writing through it, then deleting "every other
/// file in the folder", would delete whatever it points at. So a folder is
/// used only when it is a folder of its own, looked at without following a
/// link, and stays pinned while it is used:
/// - on Windows it is held open without delete sharing, so nobody can move
///   or replace it, or a folder above it, meanwhile;
/// - on Linux, where an open folder can still be renamed, every listing,
///   write, rename and delete in it goes through its descriptor
///   (`/proc/self/fd/<fd>/<name>`), which the kernel resolves to the pinned
///   folder itself, never to whatever has been swapped in at its path.
///
/// Either way [`Pinned::check`] also stops a write once the path names
/// another folder.
struct Pinned {
    path: PathBuf,
    handle: File,
}

impl Pinned {
    /// Pins the folder at `path`: `Ok(None)` when nothing is there, an
    /// error when a link or anything but a folder is.
    fn open(path: &Path) -> io::Result<Option<Self>> {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(not_a_folder_of_its_own()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
        let handle = open_folder_itself(path)?;
        if !handle.metadata()?.is_dir() {
            return Err(not_a_folder_of_its_own());
        }
        Ok(Some(Self {
            path: path.to_path_buf(),
            handle,
        }))
    }

    /// Pins the folder at `path`, creating it through `fs` when nothing is
    /// there.
    fn open_or_create(fs: &dyn Fs, path: &Path) -> io::Result<Self> {
        if let Some(pinned) = Self::open(path)? {
            return Ok(pinned);
        }
        fs.create_dir(&Target::plain(path))?;
        Self::open(path)?.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    /// The path the OS resolves to the pinned folder itself.
    #[cfg(windows)]
    fn anchor(&self) -> PathBuf {
        self.path.clone()
    }

    /// The path the OS resolves to the pinned folder itself: its descriptor
    /// in `/proc`.
    #[cfg(target_os = "linux")]
    fn anchor(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", self.handle.as_raw_fd()))
    }

    /// The entry `name` of the pinned folder.
    fn target(&self, name: impl AsRef<Path>) -> Target {
        Target {
            shown: self.path.join(&name),
            at: self.anchor().join(name),
        }
    }

    /// Fails unless the path still names the pinned folder.
    fn check(&self) -> io::Result<()> {
        let meta = fs::symlink_metadata(&self.path)?;
        if meta.is_dir() && is_same_folder(&meta, &self.handle) {
            Ok(())
        } else {
            Err(io::Error::other(
                "the folder was moved or replaced while in use",
            ))
        }
    }
}

fn not_a_folder_of_its_own() -> io::Error {
    io::Error::other("a link or a file is where the folder belongs")
}

/// Opens the folder at `path` as itself, never what a link there points at.
#[cfg(windows)]
fn open_folder_itself(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    OpenOptions::new()
        .read(true)
        // Without FILE_SHARE_DELETE, nobody can move, rename or delete the
        // folder, or a folder above it, while this handle is open.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        // A junction or symlink opens as itself, so its metadata shows it.
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Opens the folder at `path` as itself, never what a link there points at.
#[cfg(target_os = "linux")]
fn open_folder_itself(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

/// Whether `meta`, looked up by path, is the folder `handle` holds open.
/// On Windows the handle keeps the folder where it is, so the path can name
/// no other.
#[cfg(windows)]
fn is_same_folder(_meta: &fs::Metadata, _handle: &File) -> bool {
    true
}

/// Whether `meta`, looked up by path, is the folder `handle` holds open.
#[cfg(target_os = "linux")]
fn is_same_folder(meta: &fs::Metadata, handle: &File) -> bool {
    use std::os::unix::fs::MetadataExt;
    handle
        .metadata()
        .is_ok_and(|held| held.dev() == meta.dev() && held.ino() == meta.ino())
}

/// Whether a folder of its own, not a link, is at `path`.
fn is_folder_of_its_own(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// An install that has the Pagle addon (CMP-R80): its `Interface/AddOns/`
/// and `Pagle/Pagle.toc`, as found (CMP-R17).
struct Addon {
    addons: PathBuf,
    pagle_toc: PathBuf,
}

impl Addon {
    /// `None` when `flavor` has no `Interface/AddOns/Pagle/Pagle.toc` now.
    fn of(flavor: &str) -> Option<Self> {
        let addons = paths::lookup_path(Path::new(flavor), &["Interface", "AddOns"])?;
        let pagle_toc = paths::lookup_path(&addons, &["Pagle", "Pagle.toc"])?;
        pagle_toc.is_file().then_some(Self { addons, pagle_toc })
    }

    /// The folder for `file`: the existing one, found without regard to case,
    /// or where a new one goes, with the case given (CMP-R17).
    fn folder(&self, file: DataFile) -> PathBuf {
        paths::lookup(&self.addons, file.folder())
            .unwrap_or_else(|| self.addons.join(file.folder()))
    }

    /// The folder for `file` when it is a folder of its own holding its data
    /// file, neither of them a link.
    fn folder_with_data(&self, file: DataFile) -> Option<PathBuf> {
        let folder = self.folder(file);
        let data = fs::symlink_metadata(folder.join(file.file_name()));
        (is_folder_of_its_own(&folder) && data.is_ok_and(|meta| meta.is_file())).then_some(folder)
    }

    fn toc(&self, file: DataFile) -> io::Result<Vec<u8>> {
        Ok(toc(
            file,
            &copied_lines(&paths::read_file(&self.pagle_toc, TOC_CAP)?),
        ))
    }
}

/// Whether `flavor` has the Pagle addon now, so it gets the data addon
/// folders (CMP-R80).
pub fn qualifies(flavor: &str) -> bool {
    Addon::of(flavor).is_some()
}

/// `path` in the OS's own form, as the window shows it (CMP-R83).
pub fn os_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.into_owned()
    }
}

/// What the window shows for a folder a write failed in (CMP-R83).
pub fn failure_text(folder: &str) -> String {
    format!("Couldn't write the in-game files to {folder}.")
}

/// Writes `bytes` whole to `.{name}.tmp` in `folder`, flushed, then renames
/// it over `name` (CMP-R83). The `.tmp` is always a new file: whatever is
/// at its name first, a leftover or a link someone put there, is deleted,
/// never written through. On failure the old file stays, and the `.tmp` is
/// deleted if it can be.
fn write_atomic(fs: &dyn Fs, folder: &Pinned, name: &str, bytes: &[u8]) -> io::Result<()> {
    folder.check()?;
    let tmp = folder.target(format!(".{name}.tmp"));
    if fs::symlink_metadata(&tmp.at).is_ok() {
        fs.remove_file(&tmp)?;
    }
    let written = fs
        .write_synced(&tmp, bytes)
        .and_then(|()| fs.rename(&tmp, &folder.target(name)));
    if written.is_err() && fs::symlink_metadata(&tmp.at).is_ok() {
        let _ = fs.remove_file(&tmp);
    }
    written
}

/// Whether `name` is one of the two files `file`'s folder holds. Windows
/// keeps a file's old case when it is replaced, so there case is ignored.
fn is_kept(name: &str, file: DataFile) -> bool {
    [file.file_name(), file.toc_name()].iter().any(|kept| {
        if cfg!(windows) {
            name.eq_ignore_ascii_case(kept)
        } else {
            name == *kept
        }
    })
}

/// Writes `body` as `file`'s data file into one install, then its TOC, then
/// deletes everything else in the folder (CMP-R83, IGV-R40). The folder is
/// created only here, at a data file write (CMP-R80).
fn write_install(fs: &dyn Fs, addon: &Addon, file: DataFile, body: &[u8]) -> io::Result<()> {
    let toc = addon.toc(file)?;
    let folder = Pinned::open_or_create(fs, &addon.folder(file))?;
    // The data file first, so no folder ever holds a TOC alone (CMP-R80).
    write_atomic(fs, &folder, file.file_name(), body)?;
    write_atomic(fs, &folder, file.toc_name(), &toc)?;
    for entry in fs::read_dir(folder.anchor())?.filter_map(Result::ok) {
        if is_kept(&entry.file_name().to_string_lossy(), file) {
            continue;
        }
        folder.check()?;
        let target = folder.target(entry.file_name());
        let deleted = if entry.file_type().is_ok_and(|t| t.is_dir()) {
            fs.remove_dir_all(&target)
        } else {
            fs.remove_file(&target)
        };
        if let Err(e) = deleted {
            log::warn!("couldn't delete {}: {e}", target.shown.display());
        }
    }
    Ok(())
}

/// Deletes leftover `.*.tmp` files in the three folders of an install that
/// has the Pagle addon (CMP-R83).
fn sweep_tmp(fs: &dyn Fs, addon: &Addon) {
    for file in DataFile::ALL {
        let folder = match Pinned::open(&addon.folder(file)) {
            Ok(Some(folder)) => folder,
            Ok(None) => continue,
            Err(e) => {
                log::warn!("not sweeping {}: {e}", addon.folder(file).display());
                continue;
            }
        };
        let Ok(entries) = fs::read_dir(folder.anchor()) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !(name.starts_with('.')
                && name.ends_with(".tmp")
                && entry.file_type().is_ok_and(|t| t.is_file()))
            {
                continue;
            }
            if let Err(e) = folder.check() {
                log::warn!("stopped sweeping {}: {e}", folder.path.display());
                break;
            }
            let target = folder.target(entry.file_name());
            if let Err(e) = fs.remove_file(&target) {
                log::warn!("couldn't delete {}: {e}", target.shown.display());
            }
        }
    }
}

/// Deletes the three data addon folders entirely in every install of
/// `flavors` where they exist, whether or not the Pagle addon is still there
/// (CMP-R85). Every entry of `Interface/AddOns/` named like any of them, in
/// any case, goes, so the addon reads them as absent (IGV-R44; PUSH-R13 for
/// `Pagle_Content`, which then falls back to its rotation, HNT-R153).
fn delete_folders(fs: &dyn Fs, flavors: &[String]) {
    for flavor in flavors {
        let Some(addons) = paths::lookup_path(Path::new(flavor), &["Interface", "AddOns"]) else {
            continue;
        };
        let Ok(entries) = fs::read_dir(&addons) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if !DataFile::ALL
                .iter()
                .any(|f| f.folder().to_lowercase() == name)
            {
                continue;
            }
            // `remove_dir_all` deletes a link here, never what it points at.
            let path = entry.path();
            match fs.remove_dir_all(&Target::plain(&path)) {
                Ok(()) => log::info!("deleted {}", path.display()),
                Err(e) => log::warn!("couldn't delete {}: {e}", path.display()),
            }
        }
    }
}

/// Deletes the three data addon folders in every install of `flavors`
/// (CMP-R85):
/// the one delete for Unlink, losing the link and removal. Removal (CMP-R70
/// step 4) passes every install its scan found or local state names.
pub fn delete_data_addon_folders(flavors: &[String]) {
    delete_folders(&RealFs, flavors);
}

/// Every install the Companion knows: the flavor folders a scan `found`,
/// then each install local state names (`manual`) that isn't among them
/// already (CMP-R85, as CMP-R70 has it).
pub fn known_installs(found: Vec<String>, manual: Vec<String>) -> Vec<String> {
    let mut flavors = found;
    for manual in manual {
        let key = paths::same_folder_key(&manual);
        if !flavors.iter().any(|f| paths::same_folder_key(f) == key) {
            flavors.push(manual);
        }
    }
    flavors
}

/// Which installs a data file was written in (CMP-R83). Each is a flavor
/// folder as passed in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteReport {
    pub written: Vec<String>,
    /// Installs with the Pagle addon whose write failed; each keeps its old
    /// file and shows the write-failure message.
    pub failed: Vec<String>,
}

impl WriteReport {
    /// The download succeeded: its data file is written in at least one
    /// install (CMP-R81). A body written nowhere waits one following interval.
    pub fn succeeded(&self) -> bool {
        !self.written.is_empty()
    }
}

/// Which installs qualify now (CMP-R80), and which of them lack each data
/// file. No in-game download while none qualifies; one that lacks a data
/// file makes that download due at once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Qualifying {
    pub installs: Vec<String>,
    pub lacking_following: Vec<String>,
    pub lacking_anglers: Vec<String>,
    pub lacking_content: Vec<String>,
}

impl Qualifying {
    pub fn any(&self) -> bool {
        !self.installs.is_empty()
    }

    pub fn lacking(&self, file: DataFile) -> &[String] {
        match file {
            DataFile::Following => &self.lacking_following,
            DataFile::Anglers => &self.lacking_anglers,
            DataFile::Content => &self.lacking_content,
        }
    }
}

/// A folder a write failed in, as the window's WoW folders section shows it
/// (CMP-R83, CMP UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteFailure {
    /// The install's flavor folder, as the scanner has it.
    pub flavor: String,
    /// The data addon folder, in the OS's own form.
    pub folder: String,
    /// "Couldn't write the in-game files to {folder}."
    pub text: String,
}

/// What failed in a folder: a data file write is cleared only by the next
/// data file write there, a TOC refresh also by the next refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cause {
    Write,
    TocRefresh,
}

#[derive(Debug, Clone)]
struct Failure {
    folder: String,
    cause: Cause,
}

/// Writes the data addon folders, one change at a time, and keeps the write
/// failures the window shows. Reach it with `app.state::<Writer>()`, or
/// through [`window`]'s functions, which pass it the scanner's installs.
pub struct Writer {
    fs: Box<dyn Fs>,
    /// Held across every change to the folders; true once stopped.
    busy: Mutex<bool>,
    failures: Mutex<BTreeMap<(String, DataFile), Failure>>,
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Writer {
    pub fn new() -> Self {
        Self::with_fs(Box::new(RealFs))
    }

    /// A writer whose writes and deletes go through `fs`.
    pub fn with_fs(fs: Box<dyn Fs>) -> Self {
        Self {
            fs,
            busy: Mutex::new(false),
            failures: Mutex::default(),
        }
    }

    /// Writes a body that passed CMP-R82 byte for byte as `file`'s data file
    /// into every install of `flavors` that has the Pagle addon now, with the
    /// folder's TOC, then deletes everything else in the folder (CMP-R80,
    /// CMP-R83). An install whose write fails keeps its old file and shows
    /// the write-failure message until a write there succeeds.
    pub fn write(&self, flavors: &[String], file: DataFile, body: &[u8]) -> WriteReport {
        let stopped = lock(&self.busy);
        let mut report = WriteReport::default();
        if *stopped {
            return report;
        }
        for flavor in flavors {
            let Some(addon) = Addon::of(flavor) else {
                continue;
            };
            match write_install(self.fs.as_ref(), &addon, file, body) {
                Ok(()) => {
                    lock(&self.failures).remove(&(flavor.clone(), file));
                    report.written.push(flavor.clone());
                }
                Err(e) => {
                    let folder = addon.folder(file);
                    log::warn!(
                        "couldn't write {} to {}: {e}",
                        file.file_name(),
                        folder.display()
                    );
                    self.fail(flavor, file, &folder, Cause::Write);
                    report.failed.push(flavor.clone());
                }
            }
        }
        // A folder no longer written to shows no message (CMP-R80).
        lock(&self.failures).retain(|(flavor, f), _| {
            *f != file || report.failed.contains(flavor) || report.written.contains(flavor)
        });
        report
    }

    /// Rewrites the TOC of each folder that holds its data file whenever it
    /// differs from what `Pagle.toc` now gives (CMP-R83). The timer calls it
    /// at start and every in-game check interval.
    pub fn refresh_tocs(&self, flavors: &[String]) {
        let stopped = lock(&self.busy);
        if !*stopped {
            self.refresh_tocs_locked(flavors);
        }
    }

    fn refresh_tocs_locked(&self, flavors: &[String]) {
        for flavor in flavors {
            let Some(addon) = Addon::of(flavor) else {
                continue;
            };
            for file in DataFile::ALL {
                let Some(folder) = addon.folder_with_data(file) else {
                    continue;
                };
                let refreshed = addon.toc(file).and_then(|want| {
                    let pinned = Pinned::open(&folder)?
                        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
                    let have = paths::read_file(&pinned.target(file.toc_name()).at, TOC_CAP).ok();
                    if have.as_deref() == Some(want.as_slice()) {
                        return Ok(());
                    }
                    log::info!("rewriting {}", folder.join(file.toc_name()).display());
                    write_atomic(self.fs.as_ref(), &pinned, file.toc_name(), &want)
                });
                match refreshed {
                    Ok(()) => {
                        let key = (flavor.clone(), file);
                        let mut failures = lock(&self.failures);
                        if failures
                            .get(&key)
                            .is_some_and(|f| f.cause == Cause::TocRefresh)
                        {
                            failures.remove(&key);
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "couldn't write {} to {}: {e}",
                            file.toc_name(),
                            folder.display()
                        );
                        self.fail(flavor, file, &folder, Cause::TocRefresh);
                    }
                }
            }
        }
    }

    /// At start: deletes leftover `.*.tmp` files in installs that have the
    /// Pagle addon, then refreshes the TOCs (CMP-R83).
    pub fn start(&self, flavors: &[String]) {
        let stopped = lock(&self.busy);
        if *stopped {
            return;
        }
        for addon in flavors.iter().filter_map(|f| Addon::of(f)) {
            sweep_tmp(self.fs.as_ref(), &addon);
        }
        self.refresh_tocs_locked(flavors);
    }

    /// Stops writing for good, once a change under way has ended: removal on
    /// Linux stops watching before it deletes the folders (CMP-R71). Later
    /// writes, TOC refreshes and start sweeps do nothing.
    pub fn stop(&self) {
        *lock(&self.busy) = true;
    }

    /// Deletes the three folders in every install of `flavors` where they
    /// exist (CMP-R85), which leaves no write failure to show.
    pub fn delete_folders(&self, flavors: &[String]) {
        let _busy = lock(&self.busy);
        delete_folders(self.fs.as_ref(), flavors);
        lock(&self.failures).clear();
    }

    /// Which of `flavors` qualify now, and which of those lack each data file
    /// (CMP-R80).
    pub fn qualifying(&self, flavors: &[String]) -> Qualifying {
        let mut report = Qualifying::default();
        for flavor in flavors {
            let Some(addon) = Addon::of(flavor) else {
                continue;
            };
            report.installs.push(flavor.clone());
            if addon.folder_with_data(DataFile::Following).is_none() {
                report.lacking_following.push(flavor.clone());
            }
            if addon.folder_with_data(DataFile::Anglers).is_none() {
                report.lacking_anglers.push(flavor.clone());
            }
            if addon.folder_with_data(DataFile::Content).is_none() {
                report.lacking_content.push(flavor.clone());
            }
        }
        report
    }

    /// Each qualifying install's `file`, as the lowercase hex SHA-256 of its
    /// data file; `None` where it is missing or can't be read.
    pub fn data_file_hashes(
        &self,
        flavors: &[String],
        file: DataFile,
    ) -> Vec<(String, Option<String>)> {
        flavors
            .iter()
            .filter_map(|flavor| {
                let addon = Addon::of(flavor)?;
                let hash = addon
                    .folder_with_data(file)
                    .and_then(|folder| {
                        paths::read_file(&folder.join(file.file_name()), DATA_FILE_CAP).ok()
                    })
                    .map(|bytes| sha256_hex(&bytes));
                Some((flavor.clone(), hash))
            })
            .collect()
    }

    /// Whether `file` in every qualifying install hashes to `etag`, the kept
    /// `ETag` (a quoted lowercase hex SHA-256, a weak `W/` ignored): only then
    /// does the download send `If-None-Match` (CMP-R81). False while no
    /// install qualifies.
    pub fn data_files_hash_to(&self, flavors: &[String], file: DataFile, etag: &str) -> bool {
        let etag = etag.strip_prefix("W/").unwrap_or(etag);
        let Some(wanted) = etag.strip_prefix('"').and_then(|e| e.strip_suffix('"')) else {
            return false;
        };
        let hashes = self.data_file_hashes(flavors, file);
        !hashes.is_empty()
            && hashes.iter().all(|(_, hash)| {
                hash.as_deref()
                    .is_some_and(|h| h.eq_ignore_ascii_case(wanted))
            })
    }

    /// The write failures to show, one per failing folder (CMP-R83).
    pub fn failures(&self) -> Vec<WriteFailure> {
        lock(&self.failures)
            .iter()
            .map(|((flavor, _), failure)| WriteFailure {
                flavor: flavor.clone(),
                folder: failure.folder.clone(),
                text: failure_text(&failure.folder),
            })
            .collect()
    }

    fn fail(&self, flavor: &str, file: DataFile, folder: &Path, cause: Cause) {
        let mut failures = lock(&self.failures);
        let failure = failures
            .entry((flavor.to_owned(), file))
            .or_insert(Failure {
                folder: os_path(folder),
                cause,
            });
        if cause == Cause::Write {
            failure.cause = Cause::Write;
        }
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    //! Installs with or without the Pagle addon, and an [`Fs`] that fails
    //! where asked to and records what it does.

    use std::sync::Arc;

    use super::*;

    /// A flavor folder at `base/rel` with `Interface/AddOns/`, and
    /// `Pagle/Pagle.toc` holding `pagle_toc` if given. Canonical (CMP-R17).
    pub fn install(base: &Path, rel: &str, pagle_toc: Option<&[u8]>) -> String {
        let flavor = base.join(rel);
        let addons = flavor.join("Interface/AddOns");
        fs::create_dir_all(&addons).unwrap();
        if let Some(bytes) = pagle_toc {
            fs::create_dir_all(addons.join("Pagle")).unwrap();
            fs::write(addons.join("Pagle/Pagle.toc"), bytes).unwrap();
        }
        paths::canonical(&flavor).unwrap()
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Op {
        WriteSynced,
        Rename,
        CreateDir,
        RemoveFile,
        RemoveDirAll,
    }

    type Fails = Box<dyn Fn(Op, &Path) -> bool + Send + Sync>;

    /// The real file system, except that each operation `fails` says yes to
    /// (given the path it works on, a rename's source) fails without doing
    /// anything. Every operation that ran is recorded.
    pub struct FaultyFs {
        fails: Fails,
        ops: Arc<Mutex<Vec<(Op, PathBuf)>>>,
    }

    impl FaultyFs {
        pub fn new(fails: impl Fn(Op, &Path) -> bool + Send + Sync + 'static) -> Self {
            Self {
                fails: Box::new(fails),
                ops: Arc::default(),
            }
        }

        pub fn ops(&self) -> Arc<Mutex<Vec<(Op, PathBuf)>>> {
            Arc::clone(&self.ops)
        }

        /// Runs `real` unless `fails` says no to `target`, by the path it
        /// is shown as.
        fn run(
            &self,
            op: Op,
            target: &Target,
            real: impl FnOnce() -> io::Result<()>,
        ) -> io::Result<()> {
            if (self.fails)(op, &target.shown) {
                return Err(io::Error::other("injected fault"));
            }
            lock(&self.ops).push((op, target.shown.clone()));
            real()
        }
    }

    impl Fs for FaultyFs {
        fn write_synced(&self, target: &Target, bytes: &[u8]) -> io::Result<()> {
            self.run(Op::WriteSynced, target, || {
                RealFs.write_synced(target, bytes)
            })
        }

        fn rename(&self, from: &Target, to: &Target) -> io::Result<()> {
            self.run(Op::Rename, from, || RealFs.rename(from, to))
        }

        fn create_dir(&self, target: &Target) -> io::Result<()> {
            self.run(Op::CreateDir, target, || RealFs.create_dir(target))
        }

        fn remove_file(&self, target: &Target) -> io::Result<()> {
            self.run(Op::RemoveFile, target, || RealFs.remove_file(target))
        }

        fn remove_dir_all(&self, target: &Target) -> io::Result<()> {
            self.run(Op::RemoveDirAll, target, || RealFs.remove_dir_all(target))
        }
    }
}

#[cfg(test)]
mod tests;

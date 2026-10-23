//! The data addon folders' tests, over temporary folder trees.

use std::fs;
use std::path::Path;

use super::fixture::*;
use super::*;

const TOC_LINES: &[u8] = b"## Interface: 11508\n## Title: Pagle\n## AllowLoadGameType: classic\n";
const BODY: &[u8] = b"PagleAnglersData = {\nok = true,\n}\n";

fn following_toc(interface: &str, game_type: &str) -> String {
    format!(
        "{interface}{game_type}## Title: Pagle Following\n\
         ## Notes: Anglers you follow on the Pagle website. Written by the Pagle Companion.\n\
         ## Group: Pagle\n## Category: Professions\n## Dependencies: Pagle\n## Version: 1\n\
         \nFollowing.lua\n"
    )
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn folder(flavor: &str, file: DataFile) -> std::path::PathBuf {
    Path::new(flavor)
        .join("Interface/AddOns")
        .join(file.folder())
}

#[test]
fn the_following_toc_is_the_template_with_the_copied_lines() {
    // IGV Contracts, The TOCs; CMP-R83: the template byte for byte, with the
    // ## Interface: and ## AllowLoadGameType: lines of Pagle.toc
    let toc = toc(DataFile::Following, &copied_lines(TOC_LINES));
    assert_eq!(
        String::from_utf8(toc).unwrap(),
        following_toc("## Interface: 11508\n", "## AllowLoadGameType: classic\n")
    );
}

#[test]
fn the_anglers_toc_is_load_on_demand_and_ends_with_an_lf() {
    // IGV Contracts, The TOCs; CMP-R83: Pagle_Anglers' template, its last
    // line Anglers.lua ending with an LF
    let toc = toc(DataFile::Anglers, &copied_lines(TOC_LINES));
    assert_eq!(
        String::from_utf8(toc).unwrap(),
        "## Interface: 11508\n## AllowLoadGameType: classic\n## Title: Pagle Anglers\n\
         ## Notes: Published Anglers in your regions, loaded when you first hover a player. \
         Written by the Pagle Companion.\n\
         ## Group: Pagle\n## Category: Professions\n## Dependencies: Pagle\n\
         ## LoadOnDemand: 1\n## Version: 1\n\nAnglers.lua\n"
    );
}

#[test]
fn the_content_toc_is_hev_s_template_with_the_copied_lines() {
    // HEV Contracts, The addon folder; HEV-R50, CMP-R83: the template byte
    // for byte, loaded normally after Pagle (no LoadOnDemand), with a blank
    // line before Content.lua as in IGV's two
    let text = toc(DataFile::Content, &copied_lines(TOC_LINES));
    assert_eq!(
        String::from_utf8(text).unwrap(),
        "## Interface: 11508\n## AllowLoadGameType: classic\n## Title: Pagle Content\n\
         ## Notes: Pagle Wagers and other content from Pagle. Written by the Pagle Companion.\n\
         ## Group: Pagle\n## Category: Professions\n## Dependencies: Pagle\n## Version: 1\n\
         \nContent.lua\n"
    );
    // A line Pagle.toc lacks is left out here too.
    let toc = toc(DataFile::Content, &copied_lines(b"## Title: Pagle\n"));
    assert!(toc.starts_with(b"## Title: Pagle Content\n"));
}

#[test]
fn the_content_folder_is_written_like_the_others() {
    // HEV-R50, CMP-R80, CMP-R83: Pagle_Content/ in every qualifying install,
    // the body byte for byte as Content.lua with its TOC, written atomically
    // (data file first), then everything else in the folder deleted
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "C/WoW/_classic_beta_", Some(TOC_LINES));
    let b = install(tmp.path(), "D/WoW/_classic_beta_", None);
    let dir = folder(&a, DataFile::Content);
    fs::create_dir_all(dir.join("old")).unwrap();
    fs::write(dir.join("old.lua"), "old").unwrap();
    fs::write(dir.join(".Content.lua.tmp"), "half").unwrap();
    let fs_ = FaultyFs::new(|_, _| false);
    let ops = fs_.ops();
    let writer = Writer::with_fs(Box::new(fs_));

    let report = writer.write(&[a.clone(), b.clone()], DataFile::Content, BODY);
    assert_eq!(report.written, std::slice::from_ref(&a));
    assert_eq!(entries(&dir), ["Content.lua", "Pagle_Content.toc"]);
    assert_eq!(fs::read(dir.join("Content.lua")).unwrap(), BODY);
    assert_eq!(
        fs::read(dir.join("Pagle_Content.toc")).unwrap(),
        toc(DataFile::Content, &copied_lines(TOC_LINES))
    );
    let written: Vec<String> = ops
        .lock()
        .unwrap()
        .iter()
        .filter(|(op, _)| matches!(op, Op::WriteSynced | Op::Rename))
        .map(|(_, path)| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        written,
        [
            ".Content.lua.tmp",
            ".Content.lua.tmp",
            ".Pagle_Content.toc.tmp",
            ".Pagle_Content.toc.tmp"
        ]
    );
    assert!(!folder(&b, DataFile::Content).exists());
    // The other two folders aren't touched by it.
    assert!(!folder(&a, DataFile::Following).exists());
    assert!(!folder(&a, DataFile::Anglers).exists());
}

#[test]
fn a_line_pagle_toc_lacks_is_left_out() {
    // CMP-R83: a line Pagle.toc lacks is left out
    let toc = toc(DataFile::Following, &copied_lines(b"## Interface: 11508\n"));
    assert_eq!(
        String::from_utf8(toc).unwrap(),
        following_toc("## Interface: 11508\n", "")
    );
    let toc = toc_of(b"## Title: Pagle\n");
    assert_eq!(toc, following_toc("", ""));
}

fn toc_of(pagle_toc: &[u8]) -> String {
    String::from_utf8(toc(DataFile::Following, &copied_lines(pagle_toc))).unwrap()
}

#[test]
fn copied_lines_carry_no_cr() {
    // CMP-R83: copied verbatim without their line ends; every file has LF
    // line ends, so a CRLF Pagle.toc gives no CR
    let toc = toc_of(b"## Interface: 11508\r\n## AllowLoadGameType: classic\r\n");
    assert!(!toc.contains('\r'), "{toc:?}");
    assert_eq!(
        toc,
        following_toc("## Interface: 11508\n", "## AllowLoadGameType: classic\n")
    );
}

#[test]
fn only_the_first_exact_line_is_copied_after_a_bom() {
    // CMP-R83, IGV Contracts, The TOCs: after stripping a BOM, the first line
    // starting exactly with ## Interface: is copied; variants such as
    // ## Interface-Classic: never are
    let toc = toc_of(
        b"\xEF\xBB\xBF## Interface-Classic: 11508\n## Interface: 11508\n## Interface: 99999\n",
    );
    assert_eq!(toc, following_toc("## Interface: 11508\n", ""));
    // A BOM right before the line itself is stripped, so it is copied.
    let toc = toc_of(b"\xEF\xBB\xBF## Interface: 11508\n");
    assert_eq!(toc, following_toc("## Interface: 11508\n", ""));
    // Lines are copied verbatim, spaces and all.
    let toc = toc_of(b"##Interface: 1\n## Interface:11508 \n");
    assert_eq!(toc, following_toc("## Interface:11508 \n", ""));
}

#[test]
fn qualifying_installs_get_the_folder_and_others_nothing() {
    // CMP-R80, IGV-R40: every install whose Interface/AddOns/Pagle/Pagle.toc
    // exists gets the folder with its TOC and the body byte for byte; an
    // install without Pagle.toc gets nothing
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "C/WoW/_classic_beta_", Some(TOC_LINES));
    let b = install(tmp.path(), "D/WoW/_classic_beta_", Some(TOC_LINES));
    let c = install(tmp.path(), "E/WoW/_classic_beta_", None);
    let writer = Writer::new();

    let report = writer.write(
        &[a.clone(), b.clone(), c.clone()],
        DataFile::Following,
        BODY,
    );
    assert_eq!(report.written, [a.clone(), b.clone()]);
    assert!(report.failed.is_empty());
    assert!(report.succeeded());
    for flavor in [&a, &b] {
        let dir = folder(flavor, DataFile::Following);
        assert_eq!(entries(&dir), ["Following.lua", "Pagle_Following.toc"]);
        assert_eq!(fs::read(dir.join("Following.lua")).unwrap(), BODY);
        assert_eq!(
            fs::read_to_string(dir.join("Pagle_Following.toc")).unwrap(),
            following_toc("## Interface: 11508\n", "## AllowLoadGameType: classic\n")
        );
    }
    assert_eq!(
        entries(&Path::new(&c).join("Interface/AddOns")),
        [] as [&str; 0]
    );
    assert!(!folder(&a, DataFile::Anglers).exists());
}

#[test]
fn no_qualifying_install_is_no_success() {
    // CMP-R80, CMP-R81: a download is successful once its data file is
    // written in at least one install
    let tmp = tempfile::tempdir().unwrap();
    let c = install(tmp.path(), "WoW/_classic_beta_", None);
    let report = Writer::new().write(&[c], DataFile::Anglers, BODY);
    assert_eq!(report, WriteReport::default());
    assert!(!report.succeeded());
}

#[test]
fn other_files_and_subfolders_are_deleted_on_the_next_write() {
    // IGV-R40, CMP-R83: after writing, every other file and subfolder in the
    // folder is deleted; the TOC ends with an LF after Anglers.lua
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    let dir = folder(&a, DataFile::Anglers);
    fs::create_dir_all(dir.join("old/deeper")).unwrap();
    fs::write(dir.join("old/deeper/x.lua"), "x").unwrap();
    fs::write(dir.join("old.lua"), "old").unwrap();
    fs::write(dir.join(".Anglers.lua.tmp"), "half").unwrap();

    Writer::new().write(std::slice::from_ref(&a), DataFile::Anglers, BODY);
    assert_eq!(entries(&dir), ["Anglers.lua", "Pagle_Anglers.toc"]);
    let toc = fs::read(dir.join("Pagle_Anglers.toc")).unwrap();
    assert!(toc.ends_with(b"\n\nAnglers.lua\n"));
    assert!(!toc.starts_with(b"\xEF\xBB\xBF"));
}

#[test]
fn the_data_file_is_written_before_the_toc_through_temporary_files() {
    // CMP-R83, CMP-R80: each file goes whole to .{name}.tmp in its folder,
    // flushed, then renamed over the target; the data file first, so no
    // folder ever holds a TOC alone
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    let fs = FaultyFs::new(|_, _| false);
    let ops = fs.ops();
    let writer = Writer::with_fs(Box::new(fs));

    writer.write(std::slice::from_ref(&a), DataFile::Following, BODY);
    let dir = folder(&a, DataFile::Following);
    let names: Vec<(Op, String)> = ops
        .lock()
        .unwrap()
        .iter()
        .map(|(op, path)| {
            (
                *op,
                path.file_name().unwrap().to_string_lossy().into_owned(),
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            (Op::CreateDir, "Pagle_Following".to_owned()),
            (Op::WriteSynced, ".Following.lua.tmp".to_owned()),
            (Op::Rename, ".Following.lua.tmp".to_owned()),
            (Op::WriteSynced, ".Pagle_Following.toc.tmp".to_owned()),
            (Op::Rename, ".Pagle_Following.toc.tmp".to_owned()),
        ]
    );
    assert_eq!(entries(&dir), ["Following.lua", "Pagle_Following.toc"]);
}

#[test]
fn no_folder_is_ever_created_with_only_a_toc() {
    // CMP-R80: a folder is never created holding a TOC alone; a data file
    // write that fails writes no TOC
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    let writer = Writer::with_fs(Box::new(FaultyFs::new(|op, path| {
        op == Op::WriteSynced && path.ends_with(".Anglers.lua.tmp")
    })));

    let report = writer.write(std::slice::from_ref(&a), DataFile::Anglers, BODY);
    assert_eq!(report.failed, std::slice::from_ref(&a));
    let dir = folder(&a, DataFile::Anglers);
    assert!(!dir.join("Pagle_Anglers.toc").exists());
    assert!(!dir.exists() || entries(&dir).is_empty());
}

#[test]
fn an_interrupted_write_leaves_the_old_file_and_a_tmp_the_next_start_deletes() {
    // CMP-R83: a write cut off before its rename leaves the old file whole
    // and a .Anglers.lua.tmp; leftover .*.tmp files in the two folders are
    // deleted at start
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    Writer::new().write(std::slice::from_ref(&a), DataFile::Anglers, b"old\n");
    // Cut off before the rename: the rename and the clean-up never happen.
    let cut = Writer::with_fs(Box::new(FaultyFs::new(|op, path| {
        matches!(op, Op::Rename | Op::RemoveFile) && path.ends_with(".Anglers.lua.tmp")
    })));
    cut.write(std::slice::from_ref(&a), DataFile::Anglers, BODY);

    let dir = folder(&a, DataFile::Anglers);
    assert_eq!(fs::read(dir.join("Anglers.lua")).unwrap(), b"old\n");
    assert_eq!(fs::read(dir.join(".Anglers.lua.tmp")).unwrap(), BODY);

    Writer::new().start(std::slice::from_ref(&a));
    assert_eq!(entries(&dir), ["Anglers.lua", "Pagle_Anglers.toc"]);
}

#[test]
fn the_start_sweep_leaves_installs_without_the_addon_alone() {
    // CMP-R83, CMP-R80: leftover .tmp files are deleted only in installs
    // that have the Pagle addon; folders in other installs aren't touched
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "C/WoW/_classic_beta_", Some(TOC_LINES));
    let b = install(tmp.path(), "D/WoW/_classic_beta_", Some(TOC_LINES));
    Writer::new().write(&[a.clone(), b.clone()], DataFile::Following, BODY);
    for flavor in [&a, &b] {
        fs::write(
            folder(flavor, DataFile::Following).join(".Following.lua.tmp"),
            "x",
        )
        .unwrap();
        fs::write(
            folder(flavor, DataFile::Following).join(".Pagle_Following.toc.tmp"),
            "x",
        )
        .unwrap();
    }
    fs::remove_dir_all(Path::new(&b).join("Interface/AddOns/Pagle")).unwrap();

    Writer::new().start(&[a.clone(), b.clone()]);
    assert_eq!(
        entries(&folder(&a, DataFile::Following)),
        ["Following.lua", "Pagle_Following.toc"]
    );
    assert_eq!(
        entries(&folder(&b, DataFile::Following)),
        [
            ".Following.lua.tmp",
            ".Pagle_Following.toc.tmp",
            "Following.lua",
            "Pagle_Following.toc"
        ]
    );
}

#[test]
fn a_failed_install_keeps_its_old_file_and_shows_the_message() {
    // CMP-R83: a write that fails in one install leaves its old file and
    // shows "Couldn't write the in-game files to {data addon folder path}.";
    // the other installs are written as usual, and the next successful write
    // to that folder clears the message
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "C/WoW/_classic_beta_", Some(TOC_LINES));
    let b = install(tmp.path(), "D/WoW/_classic_beta_", Some(TOC_LINES));
    let both = [a.clone(), b.clone()];
    Writer::new().write(&both, DataFile::Anglers, b"old\n");
    let broken = Path::new(&b).join("Interface");
    let writer = Writer::with_fs(Box::new(FaultyFs::new(move |_, path| {
        path.starts_with(&broken)
    })));

    let report = writer.write(&both, DataFile::Anglers, BODY);
    assert_eq!(report.written, std::slice::from_ref(&a));
    assert_eq!(report.failed, std::slice::from_ref(&b));
    assert!(report.succeeded());
    assert_eq!(
        fs::read(folder(&a, DataFile::Anglers).join("Anglers.lua")).unwrap(),
        BODY
    );
    let b_dir = folder(&b, DataFile::Anglers);
    assert_eq!(fs::read(b_dir.join("Anglers.lua")).unwrap(), b"old\n");
    assert_eq!(entries(&b_dir), ["Anglers.lua", "Pagle_Anglers.toc"]);

    let shown = os_path(&b_dir);
    assert_eq!(
        writer.failures(),
        [WriteFailure {
            flavor: b.clone(),
            folder: shown.clone(),
            text: format!("Couldn't write the in-game files to {shown}."),
        }]
    );
    if cfg!(windows) {
        assert!(shown.contains('\\') && !shown.contains('/'), "{shown}");
    }

    // A failure of the other file shows its own folder.
    writer.write(&both, DataFile::Following, BODY);
    assert_eq!(writer.failures().len(), 2);

    // The next successful write to that folder clears its message.
    let fixed = Writer::with_fs(Box::new(FaultyFs::new(|_, _| false)));
    *fixed.failures.lock().unwrap() = writer.failures.lock().unwrap().clone();
    fixed.write(&both, DataFile::Anglers, BODY);
    assert_eq!(
        fs::read(b_dir.join("Anglers.lua")).unwrap(),
        BODY,
        "written again"
    );
    let left: Vec<_> = fixed.failures().into_iter().map(|f| f.folder).collect();
    assert_eq!(left, [os_path(&folder(&b, DataFile::Following))]);
}

#[test]
fn a_content_write_failure_shows_its_own_folder() {
    // CMP-R83, CMP UI: the WoW folders section lists each failing data addon
    // folder, Pagle_Content's as the other two
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    let writer = Writer::with_fs(Box::new(FaultyFs::new(|op, path| {
        op == Op::WriteSynced && path.ends_with(".Content.lua.tmp")
    })));
    writer.write(std::slice::from_ref(&a), DataFile::Following, BODY);
    let report = writer.write(std::slice::from_ref(&a), DataFile::Content, BODY);
    assert!(!report.succeeded());
    let shown = os_path(&folder(&a, DataFile::Content));
    assert!(shown.ends_with("Pagle_Content"), "{shown}");
    assert_eq!(
        writer.failures(),
        [WriteFailure {
            flavor: a.clone(),
            folder: shown.clone(),
            text: format!("Couldn't write the in-game files to {shown}."),
        }]
    );
}

#[test]
fn a_write_failing_everywhere_is_no_success() {
    // CMP-R83, CMP-R81: when the write fails in every install, the download
    // isn't successful
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "C/WoW/_classic_beta_", Some(TOC_LINES));
    let b = install(tmp.path(), "D/WoW/_classic_beta_", Some(TOC_LINES));
    let writer = Writer::with_fs(Box::new(FaultyFs::new(|_, _| true)));
    let report = writer.write(&[a.clone(), b.clone()], DataFile::Following, BODY);
    assert_eq!(report.failed, [a, b]);
    assert!(!report.succeeded());
    assert_eq!(writer.failures().len(), 2);
}

#[test]
fn a_toc_write_failing_after_the_data_file_fails_the_install() {
    // CMP-R83: a TOC write that fails after the data file was renamed in is
    // a failure of that install too
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    let writer = Writer::with_fs(Box::new(FaultyFs::new(|op, path| {
        op == Op::Rename && path.ends_with(".Pagle_Following.toc.tmp")
    })));

    let report = writer.write(std::slice::from_ref(&a), DataFile::Following, BODY);
    assert_eq!(report.failed, std::slice::from_ref(&a));
    assert!(!report.succeeded());
    assert_eq!(writer.failures().len(), 1);
    // Its .tmp is deleted when it can be.
    let dir = folder(&a, DataFile::Following);
    assert_eq!(entries(&dir), ["Following.lua"]);
}

#[test]
fn the_toc_refresh_rewrites_tocs_whose_copied_lines_changed() {
    // CMP-R83, IGV-R40: at start and every in-game check, the TOC of each
    // folder holding its data file is rewritten when the lines copied from
    // Pagle.toc differ, without a download
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    let flavors = [a.clone()];
    let writer = Writer::new();
    for file in DataFile::ALL {
        writer.write(&flavors, file, BODY);
    }
    let pagle_toc = Path::new(&a).join("Interface/AddOns/Pagle/Pagle.toc");
    fs::write(
        &pagle_toc,
        "## Interface: 11509\n## AllowLoadGameType: classic\n",
    )
    .unwrap();

    writer.refresh_tocs(&flavors);
    for file in DataFile::ALL {
        let dir = folder(&a, file);
        let toc = fs::read_to_string(dir.join(file.toc_name())).unwrap();
        assert!(toc.starts_with("## Interface: 11509\n## AllowLoadGameType: classic\n"));
        assert_eq!(fs::read(dir.join(file.file_name())).unwrap(), BODY);
        assert_eq!(entries(&dir).len(), 2);
    }
}

#[test]
fn the_toc_refresh_writes_nothing_when_nothing_changed() {
    // CMP-R83: rewritten only when the copied lines differ from the TOC's
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    let flavors = [a.clone()];
    Writer::new().write(&flavors, DataFile::Following, BODY);
    let fs = FaultyFs::new(|_, _| false);
    let ops = fs.ops();
    Writer::with_fs(Box::new(fs)).refresh_tocs(&flavors);
    assert!(ops.lock().unwrap().is_empty());
}

#[test]
fn the_toc_refresh_never_makes_a_toc_alone() {
    // CMP-R83, CMP-R80: only folders that hold their data file get a TOC; a
    // folder without one gets none, and none is created
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    let flavors = [a.clone()];
    let writer = Writer::new();
    writer.write(&flavors, DataFile::Following, BODY);
    fs::remove_file(folder(&a, DataFile::Following).join("Following.lua")).unwrap();
    fs::write(
        folder(&a, DataFile::Following).join("Pagle_Following.toc"),
        "stale",
    )
    .unwrap();

    writer.refresh_tocs(&flavors);
    assert_eq!(
        fs::read_to_string(folder(&a, DataFile::Following).join("Pagle_Following.toc")).unwrap(),
        "stale"
    );
    assert!(!folder(&a, DataFile::Anglers).exists());
}

#[test]
fn a_failed_toc_refresh_shows_the_message_until_one_succeeds() {
    // CMP-R83: a write that fails shows the message for that folder, cleared
    // at the next successful write to it
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    let flavors = [a.clone()];
    Writer::new().write(&flavors, DataFile::Anglers, BODY);
    fs::write(
        Path::new(&a).join("Interface/AddOns/Pagle/Pagle.toc"),
        "## Interface: 2\n",
    )
    .unwrap();
    let writer = Writer::with_fs(Box::new(FaultyFs::new(|_, _| true)));
    writer.refresh_tocs(&flavors);
    assert_eq!(writer.failures().len(), 1);

    let toc = folder(&a, DataFile::Anglers).join("Pagle_Anglers.toc");
    assert!(
        fs::read_to_string(&toc)
            .unwrap()
            .starts_with("## Interface: 11508\n")
    );
    let fixed = Writer::new();
    *fixed.failures.lock().unwrap() = writer.failures.lock().unwrap().clone();
    fixed.refresh_tocs(&flavors);
    assert!(
        fs::read_to_string(&toc)
            .unwrap()
            .starts_with("## Interface: 2\n")
    );
    assert!(fixed.failures().is_empty());
}

#[test]
fn a_toc_refresh_failure_never_hides_a_data_file_failure() {
    // CMP-R83: the message clears at the next successful write of that
    // folder's data file; a TOC refresh doesn't write it
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    let flavors = [a.clone()];
    Writer::new().write(&flavors, DataFile::Anglers, BODY);
    let failing = Writer::with_fs(Box::new(FaultyFs::new(|_, _| true)));
    failing.write(&flavors, DataFile::Anglers, BODY);
    let writer = Writer::new();
    *writer.failures.lock().unwrap() = failing.failures.lock().unwrap().clone();
    fs::write(
        Path::new(&a).join("Interface/AddOns/Pagle/Pagle.toc"),
        "## Interface: 2\n",
    )
    .unwrap();

    writer.refresh_tocs(&flavors);
    assert_eq!(writer.failures().len(), 1);
}

#[test]
fn the_folders_are_found_without_regard_to_case_on_linux_layouts() {
    // CMP-R17: interface/addons/pagle/pagle.toc and an existing pagle_anglers
    // folder are found without regard to case and used as they are; only a
    // missing folder is created with the case given
    let tmp = tempfile::tempdir().unwrap();
    let flavor = tmp.path().join("drive_c/WoW/_classic_beta_");
    let pagle = flavor.join("interface/addons/pagle");
    fs::create_dir_all(&pagle).unwrap();
    fs::write(pagle.join("pagle.toc"), TOC_LINES).unwrap();
    let anglers = flavor.join("interface/addons/pagle_anglers");
    fs::create_dir_all(&anglers).unwrap();
    let flavor = crate::installs::paths::canonical(&flavor).unwrap();
    let flavors = [flavor.clone()];
    let writer = Writer::new();

    assert!(qualifies(&flavor));
    writer.write(&flavors, DataFile::Anglers, BODY);
    writer.write(&flavors, DataFile::Following, BODY);
    let addons = Path::new(&flavor).join("interface/addons");
    let mut found = entries(&addons);
    found.sort_by_key(|n| n.to_lowercase());
    assert_eq!(found, ["pagle", "pagle_anglers", "Pagle_Following"]);
    assert_eq!(fs::read(anglers.join("Anglers.lua")).unwrap(), BODY);
    assert_eq!(entries(Path::new(&flavor)), ["interface"]);

    let report = writer.qualifying(&flavors);
    assert_eq!(report.installs, flavors);
    assert!(report.lacking(DataFile::Anglers).is_empty());

    writer.delete_folders(&flavors);
    assert_eq!(entries(&addons), ["pagle"]);
}

#[test]
fn folders_left_where_the_addon_was_removed_are_not_written() {
    // CMP-R80: an install without Pagle.toc gets neither folder, and folders
    // left behind where the Pagle addon was removed aren't touched
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    let flavors = [a.clone()];
    Writer::new().write(&flavors, DataFile::Anglers, b"old\n");
    let dir = folder(&a, DataFile::Anglers);
    fs::write(dir.join("stray.lua"), "x").unwrap();
    fs::remove_dir_all(Path::new(&a).join("Interface/AddOns/Pagle")).unwrap();
    let fs_ = FaultyFs::new(|_, _| false);
    let ops = fs_.ops();
    let writer = Writer::with_fs(Box::new(fs_));

    assert!(!writer.write(&flavors, DataFile::Anglers, BODY).succeeded());
    writer.refresh_tocs(&flavors);
    writer.start(&flavors);
    assert!(ops.lock().unwrap().is_empty());
    assert_eq!(fs::read(dir.join("Anglers.lua")).unwrap(), b"old\n");
    assert!(writer.qualifying(&flavors).installs.is_empty());
}

#[test]
fn the_writer_lists_qualifying_installs_lacking_each_data_file() {
    // CMP-R80: which installs qualify, and which of them lack each data file
    // (that download is due at once)
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "C/WoW/_classic_beta_", Some(TOC_LINES));
    let b = install(tmp.path(), "D/WoW/_classic_beta_", Some(TOC_LINES));
    let c = install(tmp.path(), "E/WoW/_classic_beta_", None);
    let all = [a.clone(), b.clone(), c.clone()];
    let writer = Writer::new();
    assert_eq!(
        writer.qualifying(&all),
        Qualifying {
            installs: vec![a.clone(), b.clone()],
            lacking_following: vec![a.clone(), b.clone()],
            lacking_anglers: vec![a.clone(), b.clone()],
            lacking_content: vec![a.clone(), b.clone()],
        }
    );

    writer.write(&all, DataFile::Following, BODY);
    writer.write(std::slice::from_ref(&a), DataFile::Anglers, BODY);
    let report = writer.qualifying(&all);
    assert!(report.lacking(DataFile::Following).is_empty());
    assert_eq!(report.lacking(DataFile::Anglers), std::slice::from_ref(&b));
    assert_eq!(report.lacking(DataFile::Content), [a.clone(), b.clone()]);
    writer.write(std::slice::from_ref(&b), DataFile::Content, BODY);
    let report = writer.qualifying(&all);
    assert_eq!(report.lacking(DataFile::Content), std::slice::from_ref(&a));
    assert!(report.any());
    assert!(!writer.qualifying(&[c]).any());
}

#[test]
fn deleting_removes_the_three_folders_wherever_they_exist() {
    // CMP-R85: on Unlink, losing the link and removal, the three folders
    // (Pagle_Content too, PUSH-R91) are deleted entirely in every install
    // where they exist, whether or not the Pagle addon is still there
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "C/WoW/_classic_beta_", Some(TOC_LINES));
    let b = install(tmp.path(), "D/WoW/_classic_beta_", Some(TOC_LINES));
    let c = install(tmp.path(), "E/WoW/_classic_beta_", None);
    let gone = tmp.path().join("F/missing").to_string_lossy().into_owned();
    let all = [a.clone(), b.clone(), c.clone(), gone];
    let writer = Writer::with_fs(Box::new(FaultyFs::new(|op, _| op == Op::WriteSynced)));
    for file in DataFile::ALL {
        Writer::new().write(&all, file, BODY);
        writer.write(&all, file, BODY);
    }
    fs::create_dir_all(folder(&a, DataFile::Anglers).join("sub")).unwrap();
    fs::remove_dir_all(Path::new(&b).join("Interface/AddOns/Pagle")).unwrap();
    assert!(!writer.failures().is_empty());
    assert!(folder(&a, DataFile::Content).join("Content.lua").is_file());
    // A folder in another case is the same folder (CMP-R17).
    fs::rename(
        folder(&b, DataFile::Content),
        Path::new(&b).join("Interface/AddOns/pagle_content"),
    )
    .unwrap();

    writer.delete_folders(&all);
    for flavor in [&a, &b] {
        assert_eq!(
            entries(&Path::new(flavor).join("Interface/AddOns")),
            if flavor == &a {
                ["Pagle"].as_slice()
            } else {
                [].as_slice()
            }
        );
    }
    assert!(Path::new(&c).join("Interface/AddOns").is_dir());
    // Nothing is left to write to, so no write failure shows.
    assert!(writer.failures().is_empty());
}

#[test]
fn the_free_delete_is_the_one_removal_calls() {
    // CMP-R85, CMP-R70 step 4: removal deletes the three folders in every
    // install the scan found or local state names, through the same delete
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    Writer::new().write(std::slice::from_ref(&a), DataFile::Anglers, BODY);
    Writer::new().write(std::slice::from_ref(&a), DataFile::Content, BODY);
    delete_data_addon_folders(std::slice::from_ref(&a));
    assert!(!folder(&a, DataFile::Anglers).exists());
    assert!(!folder(&a, DataFile::Content).exists());
    assert!(
        Path::new(&a)
            .join("Interface/AddOns/Pagle/Pagle.toc")
            .is_file()
    );
}

#[test]
fn a_stopped_writer_writes_nothing_more() {
    // CMP-R71: removal on Linux stops watching before it deletes the
    // folders, so nothing writes them again
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "WoW/_classic_beta_", Some(TOC_LINES));
    let writer = Writer::new();
    writer.stop();

    let report = writer.write(std::slice::from_ref(&a), DataFile::Following, BODY);
    assert!(!report.succeeded());
    writer.start(std::slice::from_ref(&a));
    writer.refresh_tocs(std::slice::from_ref(&a));
    assert_eq!(entries(&Path::new(&a).join("Interface/AddOns")), ["Pagle"]);
}

#[test]
fn known_installs_are_the_scans_then_those_local_state_names() {
    // CMP-R85, CMP-R70 step 4: every install the scan found or local state
    // names, each once
    let found = vec!["C:/WoW/_classic_beta_".to_owned()];
    let manual = vec![
        "C:/WoW/_classic_beta_".to_owned(),
        "D:/Games/WoW/_classic_beta_".to_owned(),
    ];
    assert_eq!(
        known_installs(found, manual),
        ["C:/WoW/_classic_beta_", "D:/Games/WoW/_classic_beta_"]
    );
    assert_eq!(
        known_installs(Vec::new(), vec!["E:/WoW/_classic_beta_".to_owned()]),
        ["E:/WoW/_classic_beta_"]
    );
}

#[test]
fn hashes_are_lowercase_hex_sha256() {
    // IGV-R32, CMP-R81: the ETag is the lowercase hex SHA-256 of the file, in
    // double quotes
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        etag_of(b"abc"),
        "\"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\""
    );
}

#[test]
fn the_etag_holds_only_when_every_qualifying_install_hashes_to_it() {
    // CMP-R81: If-None-Match is sent only when Anglers.lua of every install
    // of CMP-R80 hashes to the kept ETag
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "C/WoW/_classic_beta_", Some(TOC_LINES));
    let b = install(tmp.path(), "D/WoW/_classic_beta_", Some(TOC_LINES));
    let c = install(tmp.path(), "E/WoW/_classic_beta_", None);
    let all = [a.clone(), b.clone(), c.clone()];
    let writer = Writer::new();
    let etag = etag_of(BODY);
    assert!(!writer.data_files_hash_to(&all, DataFile::Anglers, &etag));

    writer.write(&all, DataFile::Anglers, BODY);
    assert!(writer.data_files_hash_to(&all, DataFile::Anglers, &etag));
    assert!(writer.data_files_hash_to(&all, DataFile::Anglers, &etag.to_uppercase()));
    assert!(writer.data_files_hash_to(&all, DataFile::Anglers, &format!("W/{etag}")));
    assert!(!writer.data_files_hash_to(&all, DataFile::Anglers, &etag_of(b"other")));
    assert_eq!(
        writer.data_file_hashes(&all, DataFile::Anglers),
        [
            (a.clone(), Some(sha256_hex(BODY))),
            (b.clone(), Some(sha256_hex(BODY)))
        ]
    );

    // One install's file edited, or deleted: a full body is due.
    let b_file = folder(&b, DataFile::Anglers).join("Anglers.lua");
    fs::write(&b_file, b"edited").unwrap();
    assert!(!writer.data_files_hash_to(&all, DataFile::Anglers, &etag));
    fs::remove_file(&b_file).unwrap();
    assert!(!writer.data_files_hash_to(&all, DataFile::Anglers, &etag));
    assert_eq!(
        writer.data_file_hashes(&all, DataFile::Anglers)[1],
        (b, None)
    );
    // No qualifying install holds it at all.
    assert!(!writer.data_files_hash_to(&[c], DataFile::Anglers, &etag));
}

#[test]
fn the_content_etag_holds_only_when_every_content_lua_hashes_to_it() {
    // CMP-R81: the Content download sends If-None-Match as the Anglers
    // download does, with the ETag of the Content.lua it holds (HEV-R37)
    let tmp = tempfile::tempdir().unwrap();
    let a = install(tmp.path(), "C/WoW/_classic_beta_", Some(TOC_LINES));
    let b = install(tmp.path(), "D/WoW/_classic_beta_", Some(TOC_LINES));
    let all = [a.clone(), b.clone()];
    let writer = Writer::new();
    let etag = etag_of(BODY);
    writer.write(&all, DataFile::Anglers, BODY);
    assert!(!writer.data_files_hash_to(&all, DataFile::Content, &etag));
    writer.write(&all, DataFile::Content, BODY);
    assert!(writer.data_files_hash_to(&all, DataFile::Content, &etag));
    fs::write(folder(&b, DataFile::Content).join("Content.lua"), b"x").unwrap();
    assert!(!writer.data_files_hash_to(&all, DataFile::Content, &etag));
}

#[test]
fn the_folder_and_file_names_are_the_contracts() {
    // IGV Contracts, The addon folders: Pagle_Following/Pagle_Following.toc
    // and Following.lua; Pagle_Anglers/Pagle_Anglers.toc and Anglers.lua;
    // HEV Contracts, The addon folder: Pagle_Content/Pagle_Content.toc and
    // Content.lua
    let names: Vec<_> = DataFile::ALL
        .iter()
        .map(|f| (f.folder(), f.toc_name(), f.file_name()))
        .collect();
    assert_eq!(
        names,
        [
            ("Pagle_Following", "Pagle_Following.toc", "Following.lua"),
            ("Pagle_Anglers", "Pagle_Anglers.toc", "Anglers.lua"),
            ("Pagle_Content", "Pagle_Content.toc", "Content.lua"),
        ]
    );
}

#[test]
fn the_message_uses_the_os_path_form() {
    // CMP-R83: one message per failing folder path, in the OS's own form
    let path = Path::new("G:/World of Warcraft/_classic_beta_/Interface/AddOns/Pagle_Anglers");
    let shown = os_path(path);
    if cfg!(windows) {
        assert_eq!(
            shown,
            r"G:\World of Warcraft\_classic_beta_\Interface\AddOns\Pagle_Anglers"
        );
    } else {
        assert_eq!(
            shown,
            "G:/World of Warcraft/_classic_beta_/Interface/AddOns/Pagle_Anglers"
        );
    }
    assert_eq!(
        failure_text(&shown),
        format!("Couldn't write the in-game files to {shown}.")
    );
}

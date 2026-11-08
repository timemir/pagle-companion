//! The check of `Pagle_Following`'s news block (CMP-R82, IGV-R41, IGV-R60
//! to IGV-R65): IGV Contracts' news-aware example, then one change per rule
//! of `news-block` and `news-line` to a good file built here as the contract
//! lays it out. A body without the block is the other tests' (`tests`), which
//! hold it to the grammar exactly as before the block existed.

use super::*;
use crate::in_game::backend_bodies::{FOLLOWING, FOLLOWING_EXAMPLE, FOLLOWING_NEWS_EXAMPLE};

/// The example's one angler line, LF included.
const ADA: &str = "{ name = \"Ada Brook\", more = 1, hardcore = false, title = \"Warden of Feralas Waters\", level = 23, prestige = 1, ledger = 37, legends = 3, legendsTotal = 12, waters = 4, catches = 5120, asOf = 20261012, guids = { \"Player-4618-0A1B2C3D\", \"Player-4618-0A1B2C3E\" } },\n";

/// The example's `monster` line, LF included.
const MONSTER: &str = "{ id = \"3f9a0c1d2e4b5a69\", kind = \"monster\", time = 1791733000, angler = 1, name = \"Ada Brook\", region = \"\", item = 6291, rank = 0, key = \"\", text = \"\", zone = 0, length = 544 },\n";

/// The example's `hunt` line, LF included.
const HUNT: &str = "{ id = \"a01b2c3d4e5f6071\", kind = \"hunt\", time = 1791702000, angler = 1, name = \"Ada Brook\", region = \"\", item = 0, rank = 0, key = \"night_run\", text = \"The Night Run\", zone = 0, length = 0 },\n";

/// The example's `region` headline, LF included.
const REGION: &str = "{ id = \"77c0e1f2a3b4c5d6\", kind = \"region\", time = 1791690000, angler = 0, name = \"Grimtusk\", region = \"eu\", item = 6358, rank = 0, key = \"\", text = \"\", zone = 0, length = 0 },\n";

/// A news-aware `Pagle_Following` body: `count`, `anglers`, then a news
/// block holding `news`.
fn with_news(count: usize, anglers: &str, news: &str) -> String {
    format!(
        "PagleFollowingData = {{\nv = 1,\ngeneratedAt = 1791735100,\ncount = {count},\nanglers = {{\n{anglers}}},\nnews = {{\n{news}}},\nok = true,\n}}\n"
    )
}

/// The example with its `monster` line's text `from` replaced by `to`.
fn monster_with(from: &str, to: &str) -> String {
    assert!(MONSTER.contains(from), "{from:?} not in the monster line");
    with_news(1, ADA, &MONSTER.replacen(from, to, 1))
}

/// `n` distinct news lines of Ada.
fn news_lines(n: usize) -> String {
    (0..n)
        .map(|i| MONSTER.replacen("3f9a0c1d2e4b5a69", &format!("{i:016x}"), 1))
        .collect()
}

fn fails(body: impl AsRef<[u8]>, reason: Reason) -> Rejection {
    let rejection = check_following(body.as_ref()).expect_err("should be rejected");
    assert_eq!(rejection.reason, reason, "{rejection}");
    assert_eq!(rejection.file, DataFile::Following);
    rejection
}

fn passes(body: impl AsRef<[u8]>) {
    let body = body.as_ref();
    assert_eq!(
        check_following(body),
        Ok(()),
        "{}",
        String::from_utf8_lossy(body)
    );
}

// --- with and without the block (CMP-R82, IGV-R65) ---

#[test]
fn the_news_aware_example_passes() {
    // IGV Contracts, `Pagle_Following`: the news-aware example is a valid file
    passes(FOLLOWING_NEWS_EXAMPLE);
    assert_eq!(
        check(DataFile::Following, FOLLOWING_NEWS_EXAMPLE.as_bytes()),
        Ok(())
    );
    assert_eq!(
        with_news(1, ADA, &[MONSTER, HUNT, REGION].concat()),
        FOLLOWING_NEWS_EXAMPLE
    );
}

#[test]
fn a_file_without_the_block_still_passes() {
    // CMP-R82, IGV-R65: a body may hold the news block or not; every body the
    // backend writes without it passes as before
    for body in FOLLOWING {
        passes(body);
    }
    // The example without its block is the old example but for the name.
    let without = FOLLOWING_NEWS_EXAMPLE.replace(&[MONSTER, HUNT, REGION].concat(), "");
    let without = without.replace("news = {\n},\n", "");
    passes(&without);
    assert_eq!(without.replace("Ada Brook", "Ada"), FOLLOWING_EXAMPLE);
}

#[test]
fn an_empty_block_passes() {
    // IGV-R65: a news-aware Companion's file always has the block, with
    // nothing between `news = {` and `},` when there is no news
    passes(with_news(1, ADA, ""));
    passes(with_news(0, "", ""));
}

#[test]
fn an_old_files_rejections_are_unchanged() {
    // CMP-R82: a body without the block fails where and why it did before
    // the block existed, so what reaches the local log doesn't change
    let after_anglers = FOLLOWING_EXAMPLE.find("},\nok").unwrap() + 3;
    for (from, to, reason) in [
        (
            "ok = true,",
            "ok = false,",
            Reason::ExpectedText("ok = true,"),
        ),
        ("ok = true,\n", "", Reason::ExpectedText("ok = true,")),
        (
            "ok = true,",
            "news = true,",
            Reason::ExpectedText("ok = true,"),
        ),
        (
            "ok = true,",
            "nok = true,",
            Reason::ExpectedText("ok = true,"),
        ),
    ] {
        let rejection = fails(FOLLOWING_EXAMPLE.replacen(from, to, 1), reason);
        assert_eq!(rejection.line, 8, "{to:?}");
        assert!(rejection.offset >= after_anglers, "{to:?}");
    }
}

#[test]
fn the_block_goes_only_between_anglers_and_ok() {
    // IGV Contracts, `Pagle_Following`: `[ news-block ]` once, after the
    // anglers and before `ok = true,`
    let block = "news = {\n},\n";
    fails(
        FOLLOWING_EXAMPLE.replacen("anglers = {\n", &format!("{block}anglers = {{\n"), 1),
        Reason::ExpectedText("anglers = {"),
    );
    fails(
        with_news(1, ADA, "").replacen(block, &format!("{block}{block}"), 1),
        Reason::ExpectedText("ok = true,"),
    );
    fails(
        FOLLOWING_EXAMPLE.replacen("ok = true,\n", &format!("ok = true,\n{block}"), 1),
        Reason::ExpectedText("}"),
    );
}

#[test]
fn the_blocks_own_lines_are_byte_for_byte() {
    // IGV Contracts, `Pagle_Following`: news-block
    let good = with_news(1, ADA, MONSTER);
    fails(
        good.replacen("news = {\n", "news = {\r\n", 1),
        Reason::Expected("LF"),
    );
    fails(
        good.replacen("news = {\n", "news = { \n", 1),
        Reason::Expected("LF"),
    );
    fails(
        good.replacen("news = {\n", "news = {}\n", 1),
        Reason::Expected("LF"),
    );
    fails(
        good.replacen(&format!("{MONSTER}}},\n"), &format!("{MONSTER}}}\n"), 1),
        Reason::ExpectedText("},"),
    );
    fails(
        good.replacen(&format!("{MONSTER}}},\n"), MONSTER, 1),
        Reason::ExpectedText("},"),
    );
    fails(
        good.replacen("news = {\n", "News = {\n", 1),
        Reason::ExpectedText("ok = true,"),
    );
    fails(
        good.replacen("news = {\n", "news = {\n\n", 1),
        Reason::ExpectedText("},"),
    );
}

// --- news-line (IGV Contracts, IGV-R63) ---

#[test]
fn every_kind_of_the_table_passes() {
    // IGV Contracts, `Pagle_Following`: the news line's fields by kind
    let lines = [
        (
            "first",
            "region = \"us\", item = 6358, rank = 0, key = \"\", text = \"\", zone = 0, length = 0",
        ),
        (
            "legend",
            "region = \"\", item = 6358, rank = 0, key = \"\", text = \"\", zone = 0, length = 0",
        ),
        (
            "prestige",
            "region = \"\", item = 0, rank = 3, key = \"\", text = \"\", zone = 0, length = 0",
        ),
        (
            "ledger",
            "region = \"\", item = 0, rank = 0, key = \"\", text = \"\", zone = 0, length = 0",
        ),
        (
            "colour",
            "region = \"\", item = 6291, rank = 9, key = \"golden\", text = \"\", zone = 0, length = 0",
        ),
        (
            "hunt",
            "region = \"\", item = 0, rank = 0, key = \"night_run\", text = \"The \\\"Night\\\" Run\\010\", zone = 0, length = 0",
        ),
        (
            "zone",
            "region = \"\", item = 0, rank = 0, key = \"\", text = \"\", zone = 1637, length = 0",
        ),
        (
            "monster",
            "region = \"\", item = 6291, rank = 0, key = \"\", text = \"\", zone = 0, length = 544",
        ),
        (
            "region",
            "region = \"tw\", item = 6358, rank = 0, key = \"\", text = \"\", zone = 0, length = 0",
        ),
    ];
    let news: String = lines
        .iter()
        .enumerate()
        .map(|(i, (kind, rest))| {
            format!("{{ id = \"{i:016x}\", kind = \"{kind}\", time = 1791733000, angler = 1, name = \"Ada Brook\", {rest} }},\n")
        })
        .collect();
    passes(with_news(1, ADA, &news));
}

#[test]
fn an_unknown_lowercase_kind_passes() {
    // CMP-R82: `kind` is checked only as a lowercase word, never against a
    // list, so a later kind needs no Companion release
    for kind in ["tide", "a", "abcdefghijklmnop", "newkindofnews"] {
        passes(monster_with(
            "kind = \"monster\"",
            &format!("kind = \"{kind}\""),
        ));
    }
}

#[test]
fn a_kind_that_isnt_a_lowercase_word_fails() {
    // IGV Contracts, `Pagle_Following`: kind = DQUOTE 1*16LOWER DQUOTE
    for kind in [
        "Monster",
        "MONSTER",
        "m0nster",
        "mon_ster",
        "mon ster",
        "mon-ster",
        "",
        "ä",
        "abcdefghijklmnopq",
    ] {
        fails(
            monster_with("kind = \"monster\"", &format!("kind = \"{kind}\"")),
            Reason::BadKind,
        );
    }
    fails(
        monster_with("kind = \"monster\"", "kind = monster"),
        Reason::ExpectedText("\""),
    );
    fails(
        monster_with("kind = \"monster\"", "kind = \"mon\\115ter\""),
        Reason::BadKind,
    );
}

#[test]
fn an_id_is_16_lowercase_hex_digits() {
    // IGV-R63, IGV Contracts: id = DQUOTE 16HEXLOW DQUOTE
    for id in ["0123456789abcdef", "0000000000000000", "ffffffffffffffff"] {
        passes(monster_with("3f9a0c1d2e4b5a69", id));
    }
    for id in [
        "3f9a0c1d2e4b5a6",
        "3f9a0c1d2e4b5a690",
        "3F9A0C1D2E4B5A69",
        "3f9a0c1d2e4b5a6g",
        "",
        "3f9a0c1d-e4b5a69",
    ] {
        fails(monster_with("3f9a0c1d2e4b5a69", id), Reason::BadNewsId);
    }
}

#[test]
fn a_region_is_empty_or_two_lowercase_letters() {
    // IGV Contracts, `Pagle_Following`: region = DQUOTE [ 2LOWER ] DQUOTE
    for region in ["us", "eu", "kr", "tw", "cn"] {
        passes(monster_with(
            "region = \"\"",
            &format!("region = \"{region}\""),
        ));
    }
    for region in ["EU", "e", "eur", "e1", "e\\117"] {
        fails(
            monster_with("region = \"\"", &format!("region = \"{region}\"")),
            Reason::BadRegion,
        );
    }
}

#[test]
fn an_angler_index_is_at_most_count() {
    // IGV-R41, CMP-R82: each line's `angler` is at most the file's `count`;
    // 0 is a headline's Angler not followed
    passes(monster_with("angler = 1", "angler = 0"));
    let two = [ADA, ADA.replace("Ada Brook", "Bo").as_str()].concat();
    passes(with_news(
        2,
        &two,
        &MONSTER.replace("angler = 1", "angler = 2"),
    ));

    let rejection = fails(monster_with("angler = 1", "angler = 2"), Reason::NewsAngler);
    let body = monster_with("angler = 1", "angler = 2");
    assert_eq!(
        &body.as_bytes()[rejection.offset..rejection.offset + 1],
        b"2"
    );
    assert_eq!(rejection.line, 9);
    fails(with_news(0, "", MONSTER), Reason::NewsAngler);
    fails(
        monster_with("angler = 1", "angler = 123456789012345678901234567890"),
        Reason::NewsAngler,
    );
    fails(
        monster_with("angler = 1", "angler = 01"),
        Reason::LeadingZero,
    );
}

#[test]
fn a_news_lines_fields_come_in_the_grammars_order_and_spacing() {
    // IGV Contracts, `Pagle_Following`: news-line, byte for byte
    for (from, to, reason) in [
        ("{ id", "{id", Reason::ExpectedText("{ id = ")),
        (
            "kind = \"monster\", time = 1791733000",
            "time = 1791733000, kind = \"monster\"",
            Reason::ExpectedText(", kind = "),
        ),
        (", rank = 0", "", Reason::ExpectedText(", rank = ")),
        (", length = 544", "", Reason::ExpectedText(", length = ")),
        (
            ", length = 544",
            ", length = 544, extra = 1",
            Reason::ExpectedText(" },"),
        ),
        (" },\n", "},\n", Reason::ExpectedText(" },")),
        (" },\n", " }\n", Reason::ExpectedText(" },")),
        (" },\n", " },\r\n", Reason::Expected("LF")),
        ("time = 1791733000", "time = -1", Reason::Expected("a nat")),
        (
            "time = 1791733000",
            "time = 01791733000",
            Reason::LeadingZero,
        ),
        (
            "item = 6291",
            "item = 62.91",
            Reason::ExpectedText(", rank = "),
        ),
        ("zone = 0", "zone = ", Reason::Expected("a nat")),
        (
            "name = \"Ada Brook\"",
            "name = 'Ada Brook'",
            Reason::Expected("a Lua string"),
        ),
        ("key = \"\"", "key = nil", Reason::Expected("a Lua string")),
        ("text = \"\"", "text = \"\\x41\"", Reason::BadEscape),
        ("text = \"\"", "text = \"a\u{7f}\"", Reason::ControlByte),
        ("text = \"\"", "text = \"\\256\"", Reason::EscapeTooLarge),
        // an unclosed string runs into the line's LF
        ("text = \"\"", "text = \"open", Reason::ControlByte),
    ] {
        fails(monster_with(from, to), reason);
    }
    // strings take the same Lua strings as an angler line's: escapes and
    // bytes from 0x80 up
    passes(monster_with("text = \"\"", "text = \"\\\"\\\\\\010 Æ\""));
}

// --- the news cap (IGV-R41, IGV-R64) ---

#[test]
fn at_most_the_news_cap_of_lines_passes() {
    // IGV-R41, IGV-R64, CMP-R82: at most the news cap of news lines
    assert_eq!(NEWS_CAP, 100);
    passes(with_news(1, ADA, &news_lines(NEWS_CAP)));

    let over = with_news(1, ADA, &news_lines(NEWS_CAP + 1));
    let rejection = fails(&over, Reason::NewsCap);
    // the header to `anglers = {`, Ada, `},`, `news = {`, then the lines
    let lines_before = 5 + 1 + 1 + 1 + NEWS_CAP;
    assert_eq!(rejection.line, lines_before + 1);
    assert_eq!(
        &over.as_bytes()[rejection.offset..rejection.offset + 6],
        b"{ id ="
    );
}

#[test]
fn the_news_order_isnt_checked() {
    // CMP-R82, IGV-R64: the order is the server's only, so a later order
    // needs no Companion release
    passes(with_news(1, ADA, &[REGION, HUNT, MONSTER].concat()));
    passes(with_news(1, ADA, &[MONSTER, MONSTER].concat()));
}

#[test]
fn the_news_followed_cap_and_count_are_separate() {
    // IGV-R41: the news cap counts news lines, the followed cap Anglers, and
    // `count` stays the number of angler lines
    fails(with_news(2, ADA, MONSTER), Reason::CountMismatch);
    fails(with_news(1, ADA, ADA), Reason::ExpectedText("{ id = "));
    let many: String = (0..FOLLOWED_CAP).map(|_| ADA).collect();
    passes(with_news(FOLLOWED_CAP, &many, &news_lines(NEWS_CAP)));
}

#[test]
fn a_news_rejection_names_its_position_and_rule_and_no_content() {
    // CMP-R112, CMP-R82: the failure goes to the local log, which never
    // holds names, ids or GUIDs
    for (body, rule) in [
        (
            monster_with("angler = 1", "angler = 7"),
            "(IGV-R41, CMP-R82)",
        ),
        (
            monster_with("kind = \"monster\"", "kind = \"Monster\""),
            "(IGV Contracts, Pagle_Following)",
        ),
        (
            with_news(1, ADA, &news_lines(NEWS_CAP + 1)),
            "(IGV-R41, CMP-R82)",
        ),
    ] {
        let shown = check_following(body.as_bytes()).unwrap_err().to_string();
        assert!(
            shown.starts_with("Pagle_Following body rejected at line "),
            "{shown}"
        );
        assert!(shown.ends_with(rule), "{shown}");
        for secret in ["Ada", "Brook", "3f9a0c1d", "Player-", "onster"] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }
}

#[test]
fn every_news_reason_names_a_rule() {
    // CMP-R82: the log line says why, by rule
    for reason in [
        Reason::NewsCap,
        Reason::NewsAngler,
        Reason::BadNewsId,
        Reason::BadKind,
        Reason::BadRegion,
    ] {
        assert!(!reason.to_string().is_empty());
        assert!(reason.rule().starts_with("IGV"), "{reason:?}");
    }
}

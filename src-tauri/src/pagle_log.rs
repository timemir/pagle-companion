//! A local log: what the Companion reads from `PagleLog` in one watched
//! `Pagle.lua`, and whether it is valid (CMP-R31). Pure: the caller reads
//! the file (CMP-R30) and hands its bytes here.

use std::collections::HashSet;
use std::fmt;

use crate::lua::{self, ParseError, Table, Value};

/// The largest number an upload document carries (UPL Contracts).
pub const MAX_NUMBER: u64 = (1 << 53) - 1;

/// The first format that stores its entries as `blocks` (the one-string-per-
/// entry formats 1 to 3 overflow Lua 5.1's constant limit in a large log).
/// Every format from here on keeps that layout (LOG-R17); a later encoding
/// shows itself as a block's reserved first byte.
const BLOCK_FORMAT: u64 = 4;

/// What one `Pagle.lua` holds.
#[derive(Debug, Clone, PartialEq)]
pub enum Found {
    /// No `PagleLog` assignment: the folder has no Catch log, which isn't an
    /// error (CMP-R30).
    NoCatchLog,
    /// A Catch log this Companion can't upload (CMP-R31).
    Invalid(InvalidLog),
    Valid(LocalLog),
}

/// An invalid local log: never uploaded, read again when its file changes
/// or after the Companion updates (CMP-R31).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidLog {
    /// The `logId`, when it could be read, so its row can show the state.
    pub log_id: Option<String>,
    pub reason: Invalid,
}

/// Which of CMP-R31's conditions failed. Its text names indexes and `seq`s
/// only, never entry text or character names (CMP-R112).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    /// `PagleLog` isn't a table.
    NotATable,
    /// `logId` isn't 16 lowercase hex characters.
    LogId,
    /// `format` isn't an integer literal of at least 1.
    Format,
    /// `characters` is missing, or not a list of tables at `1 … n`.
    Characters,
    /// `entries` (formats 1 to 3) is missing, or not a list of strings at
    /// `1 … n`.
    Entries,
    /// `blockSize` (format 4) isn't an integer literal of at least 1.
    BlockSize,
    /// `count` (format 4) isn't an integer literal, or differs from the
    /// entries the blocks hold.
    Count,
    /// `blocks` (format 4) is missing, or not a list of strings at `1 … n`.
    Blocks,
    /// The block at this index is empty, or holds the wrong number of
    /// entries: all but the last hold `blockSize`, the last 1 … `blockSize`.
    Block(usize),
    /// The block at this index starts with a byte below 0x20, which a later
    /// block encoding reserves; this Companion can't read it.
    ReservedBlock(usize),
    /// The character at this index can't be written as a character line.
    Character(usize),
    /// The character at this index has the GUID of an earlier one.
    DuplicateGuid(usize),
    /// The entry at this `seq` doesn't start with an entry header.
    EntryHeader(u64),
    /// The entry at this `seq` names a character index `characters` lacks.
    EntryChar(u64),
    /// The entry at this `seq` holds a line feed. Only formats 1 to 3 can
    /// say so: format 4 splits its blocks on line feeds, so none of its
    /// entries holds one.
    LineFeed(u64),
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Invalid::NotATable => write!(f, "PagleLog isn't a table"),
            Invalid::LogId => write!(f, "logId isn't 16 lowercase hex characters"),
            Invalid::Format => write!(f, "format isn't an integer literal of at least 1"),
            Invalid::Characters => write!(f, "characters isn't a list of tables"),
            Invalid::Entries => write!(f, "entries isn't a list of strings"),
            Invalid::BlockSize => write!(f, "blockSize isn't an integer literal of at least 1"),
            Invalid::Count => write!(f, "count isn't the number of entries the blocks hold"),
            Invalid::Blocks => write!(f, "blocks isn't a list of strings"),
            Invalid::Block(i) => write!(f, "block {i} doesn't hold a valid number of entries"),
            Invalid::ReservedBlock(i) => write!(f, "block {i} starts with a reserved byte"),
            Invalid::Character(i) => write!(f, "character {i} can't be a character line"),
            Invalid::DuplicateGuid(i) => write!(f, "character {i} repeats a GUID"),
            Invalid::EntryHeader(seq) => write!(f, "entry {seq} has no entry header"),
            Invalid::EntryChar(seq) => write!(f, "entry {seq} names a missing character"),
            Invalid::LineFeed(seq) => write!(f, "entry {seq} holds a line feed"),
        }
    }
}

/// A valid local log (CMP-R31).
#[derive(Debug, Clone, PartialEq)]
pub struct LocalLog {
    pub log_id: String,
    pub format: u64,
    pub test: Option<bool>,
    /// `PagleLog.characters[1 … n]`.
    pub characters: Vec<Character>,
    /// `PagleLog.entries[1 … L]` (formats 1 to 3), or the lines of
    /// `PagleLog.blocks` in order (format 4), each its bytes after escapes
    /// (UPL-R12).
    pub(crate) entries: Vec<Vec<u8>>,
}

/// One `PagleLog.characters[i]`, as its character line writes it (CMP
/// Contracts, Building an upload document).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Character {
    pub guid: String,
    pub name: String,
    pub surname: Option<String>,
    pub class_id: u64,
    pub race_id: u64,
    pub faction: String,
    pub realm_id: u64,
    pub hardcore: Option<bool>,
    pub self_found: Option<bool>,
    pub first_seen: u64,
    pub last_seen: u64,
}

/// Parses a whole `Pagle.lua` and interprets its `PagleLog` (CMP-R31). A
/// parse failure is CMP-R30's, which reads the file again.
pub fn parse(source: &[u8]) -> Result<Found, ParseError> {
    parse_with_entries(source, true)
}

pub fn parse_recovery(source: &[u8]) -> Result<Found, ParseError> {
    parse_with_entries(source, false)
}

fn parse_with_entries(source: &[u8], strict: bool) -> Result<Found, ParseError> {
    let mut all = lua::parse(source)?;
    Ok(match all.remove("PagleLog") {
        None => Found::NoCatchLog,
        Some(value) => match interpret(value, strict) {
            Ok(log) => Found::Valid(log),
            Err(invalid) => Found::Invalid(invalid),
        },
    })
}

fn interpret(value: Value, strict: bool) -> Result<LocalLog, InvalidLog> {
    let Value::Table(table) = value else {
        return Err(InvalidLog {
            log_id: None,
            reason: Invalid::NotATable,
        });
    };
    let log_id = match table.get("logId") {
        Some(Value::String(id)) if is_log_id(id) => {
            String::from_utf8(id.clone()).expect("hex is ASCII")
        }
        _ => {
            return Err(InvalidLog {
                log_id: None,
                reason: Invalid::LogId,
            });
        }
    };
    check(table, &log_id, strict).map_err(|reason| InvalidLog {
        log_id: Some(log_id.clone()),
        reason,
    })
}

fn check(mut table: Table, log_id: &str, strict: bool) -> Result<LocalLog, Invalid> {
    let format = table
        .get("format")
        .and_then(integer)
        .filter(|&f| f >= 1)
        .ok_or(Invalid::Format)?;

    let test = optional_bool(&table, "test").ok_or(Invalid::Format)?;
    let characters = list(&mut table, "characters")
        .ok_or(Invalid::Characters)?
        .into_iter()
        .map(|v| match v {
            Value::Table(t) => Ok(t),
            _ => Err(Invalid::Characters),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let entries = if format >= BLOCK_FORMAT {
        block_entries(&mut table)?
    } else {
        strings(&mut table, "entries", Invalid::Entries)?
    };

    let mut guids = HashSet::new();
    let characters = characters
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let character = character(t).ok_or(Invalid::Character(i + 1))?;
            if !guids.insert(character.guid.clone()) {
                return Err(Invalid::DuplicateGuid(i + 1));
            }
            Ok(character)
        })
        .collect::<Result<Vec<_>, _>>()?;

    for (seq, entry) in (1u64..).zip(&entries) {
        if strict {
            let header = entry_header(entry).ok_or(Invalid::EntryHeader(seq))?;
            if header.char > characters.len() as u64 {
                return Err(Invalid::EntryChar(seq));
            }
        }
        if entry.contains(&b'\n') {
            return Err(Invalid::LineFeed(seq));
        }
    }

    Ok(LocalLog {
        log_id: log_id.to_owned(),
        format,
        test,
        characters,
        entries,
    })
}

/// Takes `table[name]` as a list of strings at `1 … n`, or `invalid`.
fn strings(table: &mut Table, name: &str, invalid: Invalid) -> Result<Vec<Vec<u8>>, Invalid> {
    list(table, name)
        .ok_or(invalid)?
        .into_iter()
        .map(|v| match v {
            Value::String(s) => Ok(s),
            _ => Err(invalid),
        })
        .collect()
}

/// The entries of a format 4 or later log: the lines of `blocks`, in order. Every
/// block but the last holds `blockSize` lines, and the last holds 1 …
/// `blockSize`; a block's first byte is 0x20 or above; `count` is the
/// number of entries.
fn block_entries(table: &mut Table) -> Result<Vec<Vec<u8>>, Invalid> {
    let blocks = strings(table, "blocks", Invalid::Blocks)?;
    let block_size = table
        .get("blockSize")
        .and_then(integer)
        .filter(|&size| size >= 1)
        .ok_or(Invalid::BlockSize)?;
    let count = table.get("count").and_then(integer).ok_or(Invalid::Count)?;

    let mut entries = Vec::new();
    for (i, block) in (1usize..).zip(&blocks) {
        match block.first() {
            None => return Err(Invalid::Block(i)),
            Some(&first) if first < 0x20 => return Err(Invalid::ReservedBlock(i)),
            Some(_) => {}
        }
        let lines = block.iter().filter(|&&b| b == b'\n').count() as u64 + 1;
        if lines != block_size && (i != blocks.len() || lines > block_size) {
            return Err(Invalid::Block(i));
        }
        entries.extend(block.split(|&b| b == b'\n').map(<[u8]>::to_vec));
    }
    if entries.len() as u64 != count {
        return Err(Invalid::Count);
    }
    Ok(entries)
}

/// Takes the values of `table[name]` when it is a list at `1 … n` with no
/// hole and no other key.
fn list(table: &mut Table, name: &str) -> Option<Vec<Value>> {
    match table.remove(name)? {
        Value::Table(t) => t.into_list(),
        _ => None,
    }
}

/// `16hexlc` (UPL Contracts, LOG-R15).
fn is_log_id(id: &[u8]) -> bool {
    id.len() == 16 && id.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// An integer literal as CMP-R31 defines it: decimal digits only, with no
/// sign, point, exponent or hex, and no leading zero, at most 2^53 − 1, so
/// it is written as read and matches UPL's `nat`.
fn integer(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) if !n.negative => nat(n.literal.as_bytes()),
        _ => None,
    }
}

/// UPL's `nat`: `0`, or decimal digits without a leading zero, at most
/// 2^53 − 1.
fn nat(text: &[u8]) -> Option<u64> {
    let ok = match text {
        [b'0'] => true,
        [b'1'..=b'9', rest @ ..] => rest.len() < 16 && rest.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    if !ok {
        return None;
    }
    std::str::from_utf8(text)
        .ok()?
        .parse()
        .ok()
        .filter(|&n| n <= MAX_NUMBER)
}

/// An entry header, `<tag>|<char>|<time>|<tie>` followed by the end or a
/// `|` (UPL's `entry` grammar, LOG-R11).
struct Header<'a> {
    char: u64,
    time: &'a [u8],
    tie: &'a [u8],
}

fn entry_header(entry: &[u8]) -> Option<Header<'_>> {
    let mut fields = entry.splitn(5, |&b| b == b'|');
    let tag = fields.next()?;
    if tag.is_empty() || !tag.iter().all(u8::is_ascii_alphanumeric) {
        return None;
    }
    let char = nat(fields.next()?).filter(|&c| c >= 1)?;
    let time = fields.next()?;
    let tie = fields.next()?;
    nat(time)?;
    nat(tie)?;
    Some(Header { char, time, tie })
}

/// A character as its character line writes it, or `None` when it can't be
/// written (CMP Contracts, Building an upload document).
fn character(table: &Table) -> Option<Character> {
    let number = |name: &str| table.get(name).and_then(integer);
    let surname = match table.get("surname") {
        None => None,
        Some(Value::String(s)) => Some(field_text(s)?),
        Some(_) => return None,
    };
    let hardcore = match table.get("hardcore") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return None,
    };
    let guid = match table.get("guid")? {
        Value::String(g) if is_guid(g) => String::from_utf8(g.clone()).ok()?,
        _ => return None,
    };
    let name = match table.get("name")? {
        Value::String(n) if !n.is_empty() => field_text(n)?,
        _ => return None,
    };
    let faction = match table.get("faction")? {
        Value::String(f) if !f.is_empty() && f.iter().all(u8::is_ascii_alphabetic) => {
            String::from_utf8(f.clone()).ok()?
        }
        _ => return None,
    };
    Some(Character {
        guid,
        name,
        surname,
        class_id: number("classID")?,
        race_id: number("raceID")?,
        faction,
        realm_id: number("realmID")?,
        hardcore,
        self_found: optional_bool(table, "selfFound")?,
        first_seen: number("firstSeen")?,
        last_seen: number("lastSeen")?,
    })
}

fn optional_bool(table: &Table, name: &str) -> Option<Option<bool>> {
    match table.get(name) {
        None => Some(None),
        Some(Value::Bool(value)) => Some(Some(*value)),
        _ => None,
    }
}

pub(crate) fn valid_snapshot_character(c: &Character) -> bool {
    is_guid(c.guid.as_bytes())
        && !c.name.is_empty()
        && field_text(c.name.as_bytes()).is_some()
        && c.surname
            .as_ref()
            .is_none_or(|s| field_text(s.as_bytes()).is_some())
        && !c.faction.is_empty()
        && c.faction.bytes().all(|b| b.is_ascii_alphabetic())
}

/// UPL's `guid`: `Player-` digits `-` digits or upper-case hex.
fn is_guid(guid: &[u8]) -> bool {
    let Some(rest) = guid.strip_prefix(b"Player-") else {
        return false;
    };
    let mut parts = rest.splitn(2, |&b| b == b'-');
    let (Some(realm), Some(id)) = (parts.next(), parts.next()) else {
        return false;
    };
    !realm.is_empty()
        && realm.iter().all(u8::is_ascii_digit)
        && !id.is_empty()
        && id.iter().all(|b| matches!(b, b'0'..=b'9' | b'A'..=b'F'))
}

/// UPL's `field-byte`s that are also valid UTF-8: no control byte and no `|`.
fn field_text(bytes: &[u8]) -> Option<String> {
    let allowed = bytes
        .iter()
        .all(|&b| matches!(b, 0x20..=0x7b | 0x7d..=0x7e | 0x80..=0xff));
    if !allowed {
        return None;
    }
    String::from_utf8(bytes.to_vec()).ok()
}

impl LocalLog {
    /// `L`, the number of entries.
    pub fn len(&self) -> u64 {
        self.entries.len() as u64
    }

    /// No entries yet: not offered, counted or uploaded (CMP-R32).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry at `seq` (1-based), as its bytes.
    pub fn entry(&self, seq: u64) -> Option<&[u8]> {
        let index = usize::try_from(seq.checked_sub(1)?).ok()?;
        self.entries.get(index).map(Vec::as_slice)
    }

    /// The entry ID at `seq`: `<GUID at its char>-<time>-<tie>` (UPL-R13,
    /// LOG-R13).
    pub fn entry_id(&self, seq: u64) -> Option<String> {
        let header = entry_header(self.entry(seq)?)?;
        let character = self
            .characters
            .get(usize::try_from(header.char - 1).ok()?)?;
        Some(format!(
            "{}-{}-{}",
            character.guid,
            String::from_utf8_lossy(header.time),
            String::from_utf8_lossy(header.tie)
        ))
    }

    /// Cumulative content checkpoint over exact entries and indexed GUIDs.
    pub fn checkpoint(&self, upto: u64) -> Option<String> {
        let mut digest = crate::integrity::checkpoint_initial(&self.log_id, self.format).ok()?;
        for seq in 1..=upto {
            let entry = self.entry(seq)?;
            let header = entry_header(entry)?;
            let guid = &self
                .characters
                .get(usize::try_from(header.char - 1).ok()?)?
                .guid;
            digest = crate::integrity::checkpoint_next(&digest, seq, guid, entry).ok()?;
        }
        Some(digest)
    }

    /// The characters as the confirm question lists them: "Name Surname" or
    /// "Name", joined with ", " (CMP-R40).
    pub fn characters_display(&self) -> String {
        self.characters
            .iter()
            .map(|c| match &c.surname {
                Some(surname) => format!("{} {surname}", c.name),
                None => c.name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADA: [(&str, &str); 11] = [
        ("guid", "\"Player-4618-0A1B2C3D\""),
        ("name", "\"Ada\""),
        ("surname", "\"Brook\""),
        ("classID", "1"),
        ("raceID", "1"),
        ("faction", "\"Alliance\""),
        ("realmID", "4618"),
        ("hardcore", "false"),
        ("selfFound", "false"),
        ("firstSeen", "1791731680"),
        ("lastSeen", "1791735090"),
    ];
    const BO: &str = r#"{ guid = "Player-4618-0A1B2C3E", name = "Bo", classID = 8, raceID = 4,
        faction = "Alliance", realmID = 4618, firstSeen = 1791731700, lastSeen = 1791731800 }"#;
    const ENTRY_1: &str = r#""C1|1|1791735050|0|c|494|1439|1|148|453|86|25|263|||6358x1""#;
    const ENTRY_2: &str = r#""S1|2|1791735090|0|87|150|25""#;

    fn file(format: &str, log_id: &str, characters: &str, entries: &str) -> String {
        format!(
            "PagleDB = {{ [\"global\"] = {{ [\"decodeErrors\"] = 0 }} }}\n\
             PagleLog = {{\n  [\"format\"] = {format},\n  [\"logId\"] = {log_id},\n  \
             [\"characters\"] = {characters},\n  [\"entries\"] = {entries},\n  [\"test\"] = true,\n}}\n"
        )
    }

    fn found(source: &str) -> Found {
        parse(source.as_bytes()).expect("parses")
    }

    fn valid(source: &str) -> LocalLog {
        match found(source) {
            Found::Valid(log) => log,
            other => panic!("not valid: {other:?}"),
        }
    }

    fn invalid(source: &str) -> InvalidLog {
        match found(source) {
            Found::Invalid(invalid) => invalid,
            other => panic!("not invalid: {other:?}"),
        }
    }

    fn with_entries(entries: &str) -> String {
        file(
            "1",
            "\"3f9a2c1e7b0d4a65\"",
            &format!("{{ {}, {BO} }}", ada()),
            entries,
        )
    }

    fn with_characters(characters: &str) -> String {
        file(
            "1",
            "\"3f9a2c1e7b0d4a65\"",
            characters,
            &format!("{{ {ENTRY_1} }}"),
        )
    }

    /// Ada's character table with `field = value` replaced, or left out when
    /// `value` is empty.
    fn ada_with(field: &str, value: &str) -> String {
        let items: Vec<String> = ADA
            .iter()
            .map(|&(f, v)| (f, if f == field { value } else { v }))
            .filter(|(_, v)| !v.is_empty())
            .map(|(f, v)| format!("[\"{f}\"] = {v}"))
            .collect();
        format!("{{ {} }}", items.join(", "))
    }

    fn ada() -> String {
        ada_with("", "")
    }

    fn example() -> String {
        with_entries(&format!("{{ {ENTRY_1}, [2] = {ENTRY_2} }}"))
    }

    #[test]
    fn a_log_meeting_every_condition_is_valid() {
        // CMP-R31: format, logId, characters and entries interpreted, the rest ignored
        let log = valid(&example());
        assert_eq!(log.log_id, "3f9a2c1e7b0d4a65");
        assert_eq!(log.format, 1);
        assert_eq!(log.len(), 2);
        assert_eq!(log.entry(2), Some(&b"S1|2|1791735090|0|87|150|25"[..]));
        assert_eq!(log.entry(0), None);
        assert_eq!(log.entry(3), None);
        assert_eq!(
            log.characters[0],
            Character {
                guid: "Player-4618-0A1B2C3D".into(),
                name: "Ada".into(),
                surname: Some("Brook".into()),
                class_id: 1,
                race_id: 1,
                faction: "Alliance".into(),
                realm_id: 4618,
                hardcore: Some(false),
                self_found: Some(false),
                first_seen: 1791731680,
                last_seen: 1791735090,
            }
        );
        assert_eq!(log.characters[1].surname, None);
        assert_eq!(log.characters[1].hardcore, None);
    }

    #[test]
    fn a_file_without_pagle_log_has_no_catch_log() {
        // CMP-R30: one without a PagleLog assignment has no Catch log; not an error
        assert_eq!(found("PagleDB = { a = 1 }"), Found::NoCatchLog);
        assert_eq!(found("PagleLog = nil"), Found::NoCatchLog);
        assert_eq!(found(""), Found::NoCatchLog);
    }

    #[test]
    fn a_file_outside_the_subset_is_a_parse_failure() {
        // CMP-R31: the whole file must parse, PagleDB included
        assert!(
            parse(format!("{}\nPagleDB = {{ f = function() end }}", example()).as_bytes()).is_err()
        );
    }

    #[test]
    fn a_format_that_isnt_an_integer_literal_of_at_least_one_is_invalid() {
        // CMP-R31: format an integer literal (decimal digits only) of at least 1
        for format in [
            "1.0",
            "0x1",
            "-1",
            "0",
            "1e0",
            "01",
            "\"1\"",
            "true",
            "9007199254740992",
        ] {
            let log = invalid(&file(
                format,
                "\"3f9a2c1e7b0d4a65\"",
                &format!("{{ {} }}", ada()),
                &format!("{{ {ENTRY_1} }}"),
            ));
            assert_eq!(log.reason, Invalid::Format, "format {format}");
            assert_eq!(log.log_id.as_deref(), Some("3f9a2c1e7b0d4a65"));
        }
        assert_eq!(
            valid(&file(
                "3",
                "\"3f9a2c1e7b0d4a65\"",
                &format!("{{ {} }}", ada()),
                "{}"
            ))
            .format,
            3
        );
        assert_eq!(
            valid(&blocks_file("256", "0", &[]).replace("[\"format\"] = 4,", "[\"format\"] = 12,"))
                .format,
            12
        );
    }

    #[test]
    fn a_log_id_that_isnt_sixteen_lowercase_hex_characters_is_invalid() {
        // CMP-R31: logId is 16 lowercase hex characters (LOG-R15)
        for log_id in [
            "\"3F9A2C1E7B0D4A65\"",
            "\"3f9a2c1e7b0d4a6\"",
            "\"3f9a2c1e7b0d4a650\"",
            "\"3f9a2c1e7b0d4a6g\"",
            "1234567890123456",
        ] {
            let log = invalid(&file(
                "1",
                log_id,
                &format!("{{ {} }}", ada()),
                &format!("{{ {ENTRY_1} }}"),
            ));
            assert_eq!(
                log,
                InvalidLog {
                    log_id: None,
                    reason: Invalid::LogId
                },
                "logId {log_id}"
            );
        }
    }

    #[test]
    fn missing_fields_or_a_pagle_log_that_isnt_a_table_are_invalid() {
        // CMP-R31: characters and entries are both present
        assert_eq!(invalid("PagleLog = 1").reason, Invalid::NotATable);
        assert_eq!(
            invalid(r#"PagleLog = { format = 1, logId = "3f9a2c1e7b0d4a65", entries = {} }"#)
                .reason,
            Invalid::Characters
        );
        assert_eq!(
            invalid(r#"PagleLog = { format = 1, logId = "3f9a2c1e7b0d4a65", characters = {} }"#)
                .reason,
            Invalid::Entries
        );
        assert_eq!(
            invalid(r#"PagleLog = { logId = "3f9a2c1e7b0d4a65", characters = {}, entries = {} }"#)
                .reason,
            Invalid::Format
        );
    }

    #[test]
    fn a_hole_or_another_key_in_entries_is_invalid() {
        // CMP-R31: entries a list at 1 … n with no hole and no other key, strings only
        for entries in [
            format!("{{ [1] = {ENTRY_1}, [3] = {ENTRY_2} }}"),
            format!("{{ {ENTRY_1}, nil, {ENTRY_2} }}"),
            format!("{{ {ENTRY_1}, x = {ENTRY_2} }}"),
            format!("{{ [0] = {ENTRY_1}, {ENTRY_2} }}"),
            format!("{{ {ENTRY_1}, 5 }}"),
            "\"not a table\"".to_owned(),
        ] {
            assert_eq!(
                invalid(&with_entries(&entries)).reason,
                Invalid::Entries,
                "{entries}"
            );
        }
    }

    #[test]
    fn a_hole_or_another_key_in_characters_is_invalid() {
        // CMP-R31: characters a list at 1 … n with no hole and no other key, tables only
        assert_eq!(
            invalid(&with_characters(&format!(
                "{{ [1] = {}, [3] = {BO} }}",
                ada()
            )))
            .reason,
            Invalid::Characters
        );
        assert_eq!(
            invalid(&with_characters(&format!("{{ {}, \"Bo\" }}", ada()))).reason,
            Invalid::Characters
        );
    }

    #[test]
    fn keyed_and_positional_items_give_the_same_list() {
        // CMP Contracts, Parsing `Pagle.lua`: either way the key is the index (seq)
        let keyed = valid(&with_entries(&format!(
            "{{ [2] = {ENTRY_2}, [1] = {ENTRY_1} }}"
        )));
        assert_eq!(keyed, valid(&example()));
    }

    #[test]
    fn an_entry_without_an_entry_header_is_invalid() {
        // CMP-R31: each entry starts with an entry header matching UPL's entry grammar
        for (entry, ok) in [
            ("C1|1|1791735050|0", true),
            ("C1|1|1791735050|0|", true),
            ("A1|2|0|12|x|\r|\t", true),
            ("C1|0|1791735050|0", false),
            ("C1|01|1791735050|0", false),
            ("C1|1|01791735050|0", false),
            ("C1|1|1791735050|00", false),
            ("C1|1|1791735050", false),
            ("C1|1|1791735050|0x", false),
            ("C1|1|-1|0|c", false),
            ("C_1|1|1791735050|0", false),
            ("|1|1791735050|0", false),
            ("", false),
            ("C1|1|9007199254740992|0", false),
        ] {
            let source = with_entries(&format!("{{ {:?} }}", entry));
            match found(&source) {
                Found::Valid(_) => assert!(ok, "{entry:?} should be invalid"),
                Found::Invalid(i) => {
                    assert!(!ok, "{entry:?} should be valid");
                    assert_eq!(i.reason, Invalid::EntryHeader(1), "{entry:?}");
                }
                Found::NoCatchLog => unreachable!(),
            }
        }
    }

    #[test]
    fn an_entry_naming_a_character_index_characters_lacks_is_invalid() {
        // CMP-R31: a char index that characters has (LOG-R11)
        assert_eq!(
            invalid(&with_entries(&format!(
                "{{ {ENTRY_1}, \"C1|3|1791735060|0|g\" }}"
            )))
            .reason,
            Invalid::EntryChar(2)
        );
    }

    #[test]
    fn an_entry_holding_a_line_feed_is_invalid() {
        // CMP-R31: no entry contains a line feed (UPL-R12, wow-fish #76)
        assert_eq!(
            invalid(&with_entries(r#"{ "C1|1|1791735050|0|a\nb" }"#)).reason,
            Invalid::LineFeed(1)
        );
        valid(&with_entries(r#"{ "C1|1|1791735050|0|a\rb\0" }"#));
    }

    #[test]
    fn two_characters_with_one_guid_are_invalid() {
        // CMP-R31: no two characters share a GUID (UPL-R10 step 8)
        assert_eq!(
            invalid(&with_characters(&format!("{{ {0}, {0} }}", ada()))).reason,
            Invalid::DuplicateGuid(2)
        );
    }

    #[test]
    fn a_character_that_cant_be_a_character_line_is_invalid() {
        // CMP Contracts, Building an upload document: a missing field, a
        // non-integer number, a `|` or invalid UTF-8 in a name
        for (field, value) in [
            ("guid", ""),
            ("guid", "\"Player-4618-0a1b2c3d\""),
            ("guid", "\"Creature-4618-0A1B\""),
            ("guid", "\"Player--0A1B\""),
            ("name", ""),
            ("name", "\"\""),
            ("name", "\"A|da\""),
            ("name", "\"Ad\\255a\""),
            ("name", "\"Ad\\na\""),
            ("name", "\"Ad\\127a\""),
            ("name", "7"),
            ("surname", "\"Br|ook\""),
            ("surname", "\"Br\\192\""),
            ("surname", "false"),
            ("classID", ""),
            ("classID", "1.5"),
            ("raceID", "-1"),
            ("realmID", "0x120A"),
            ("firstSeen", "1e9"),
            ("lastSeen", "\"1791735090\""),
            ("lastSeen", "9007199254740992"),
            ("faction", ""),
            ("faction", "\"\""),
            ("faction", "\"Horde2\""),
            ("hardcore", "1"),
            ("hardcore", "\"true\""),
        ] {
            let source = with_characters(&format!("{{ {} }}", ada_with(field, value)));
            assert_eq!(
                invalid(&source).reason,
                Invalid::Character(1),
                "{field} = {value:?}"
            );
        }
    }

    #[test]
    fn nil_surname_and_hardcore_and_extra_fields_still_make_a_character_line() {
        // CMP Contracts, Building an upload document: a nil surname is empty,
        // hardcore empty for nil, selfFound and any other field left out
        let log = valid(&with_characters(&format!(
            "{{ {} }}",
            ada_with("surname", "")
                .replace("[\"selfFound\"] = false", "[\"selfFound\"] = false, x = {}")
        )));
        assert_eq!(log.characters[0].surname, None);
        let log = valid(&with_characters(&format!(
            "{{ {} }}",
            ada_with("hardcore", "true")
        )));
        assert_eq!(log.characters[0].hardcore, Some(true));
        let log = valid(&with_characters(&format!(
            "{{ {} }}",
            ada_with("hardcore", "")
        )));
        assert_eq!(log.characters[0].hardcore, None);
        let log = valid(&with_characters(&format!(
            "{{ {} }}",
            ada_with("name", "\"Ad\\195\\169la\"")
        )));
        assert_eq!(log.characters[0].name, "Adéla");
    }

    #[test]
    fn a_log_with_no_entries_is_valid_and_empty() {
        // CMP-R32: a local log with no entries is not offered or uploaded
        let log = valid(&with_entries("{}"));
        assert!(log.is_empty());
        assert!(!valid(&example()).is_empty());
    }

    #[test]
    fn an_invalid_log_keeps_its_log_id_when_it_could_be_read() {
        // CMP-R31: its logId's row shows the state when the logId could be read
        let log = invalid(&with_entries(r#"{ "C1|9|1791735050|0" }"#));
        assert_eq!(log.log_id.as_deref(), Some("3f9a2c1e7b0d4a65"));
    }

    #[test]
    fn the_entry_id_at_a_seq_is_the_guid_at_its_char_then_time_and_tie() {
        // UPL-R13: entry header plus the GUID at its char index (LOG-R13)
        let log = valid(&example());
        assert_eq!(
            log.entry_id(1).as_deref(),
            Some("Player-4618-0A1B2C3D-1791735050-0")
        );
        assert_eq!(
            log.entry_id(2).as_deref(),
            Some("Player-4618-0A1B2C3E-1791735090-0")
        );
        assert_eq!(log.entry_id(0), None);
        assert_eq!(log.entry_id(3), None);
    }

    #[test]
    fn the_characters_display_as_name_and_surname_joined_with_commas() {
        // CMP-R40: "Name Surname" (or "Name" without a surname), joined with ", "
        assert_eq!(valid(&example()).characters_display(), "Ada Brook, Bo");
    }

    #[test]
    fn invalid_reasons_never_name_entry_text_or_characters() {
        // CMP-R112: the local log file never holds entry text or character names
        assert_eq!(
            Invalid::Character(2).to_string(),
            "character 2 can't be a character line"
        );
        assert_eq!(
            Invalid::LineFeed(7).to_string(),
            "entry 7 holds a line feed"
        );
    }

    // Format 4: the same entries stored as blocks (issue #57).

    /// A Lua string literal for `text`, whatever bytes it holds.
    fn lua_string(text: &str) -> String {
        let mut out = String::from("\"");
        for c in text.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\{}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    /// A file whose `PagleLog` is format 4 with exactly these `blockSize`,
    /// `count` and `blocks` values (Lua source for the first two).
    fn blocks_file(block_size: &str, count: &str, blocks: &[String]) -> String {
        let blocks: Vec<String> = blocks.iter().map(|b| lua_string(b)).collect();
        format!(
            "PagleDB = {{}}\nPagleLog = {{\n  [\"format\"] = 4,\n  [\"logId\"] = \"3f9a2c1e7b0d4a65\",\n  \
             [\"characters\"] = {{ {}, {BO} }},\n  [\"blockSize\"] = {block_size},\n  \
             [\"count\"] = {count},\n  [\"blocks\"] = {{ {} }},\n}}\n",
            ada(),
            blocks.join(", ")
        )
    }

    /// `n` distinct entries of character 1, 2 or 3 alternating (3 is
    /// missing from `characters`, so none uses it).
    fn entries_of(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("C1|{}|{}|{}|c", 1 + i % 2, 1791735000 + i, i % 3))
            .collect()
    }

    /// The format-4 form of a format-3 entry list: `size` entries to a block.
    fn blocks_of(entries: &[String], size: usize) -> Vec<String> {
        entries.chunks(size).map(|chunk| chunk.join("\n")).collect()
    }

    /// The format-4 file holding `entries`, with a consistent `count`.
    fn format4(entries: &[String], size: usize) -> String {
        blocks_file(
            &size.to_string(),
            &entries.len().to_string(),
            &blocks_of(entries, size),
        )
    }

    /// The format-3 file holding `entries`.
    fn format3(entries: &[String]) -> String {
        let quoted: Vec<String> = entries.iter().map(|e| lua_string(e)).collect();
        file(
            "3",
            "\"3f9a2c1e7b0d4a65\"",
            &format!("{{ {}, {BO} }}", ada()),
            &format!("{{ {} }}", quoted.join(", ")),
        )
    }

    #[test]
    fn a_format_4_log_expands_into_the_same_entries_as_format_3() {
        // CMP-R31: format 4's blocks expand into the entries (issue #57)
        for n in [1, 2, 255, 256, 257, 511, 512, 513, 1000] {
            let entries = entries_of(n);
            let four = valid(&format4(&entries, 256));
            let three = valid(&format3(&entries));
            assert_eq!(four.format, 4);
            assert_eq!(four.len(), n as u64, "n = {n}");
            assert_eq!(four.entries, three.entries, "n = {n}");
            assert_eq!(four.characters, three.characters);
            assert_eq!(four.entry(n as u64), three.entry(n as u64));
            assert_eq!(four.entry(n as u64 + 1), None);
            assert_eq!(
                four.entry_id(257.min(n as u64)),
                three.entry_id(257.min(n as u64))
            );
        }
    }

    #[test]
    fn addressing_follows_the_block_edges() {
        // CMP-R31: block b holds seqs (b-1)·blockSize+1 … b·blockSize (issue #57)
        let entries = entries_of(7);
        let log = valid(&format4(&entries, 3));
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(
                log.entry(i as u64 + 1),
                Some(entry.as_bytes()),
                "seq {}",
                i + 1
            );
        }
    }

    #[test]
    fn an_empty_format_4_log_is_valid() {
        // CMP-R32: count 0 with blocks {} is an empty log (issue #57)
        let log = valid(&blocks_file("256", "0", &[]));
        assert!(log.is_empty());
        assert_eq!(log.format, 4);
    }

    #[test]
    fn a_count_that_differs_from_what_the_blocks_hold_is_invalid() {
        // CMP-R31: count equals the entries the blocks hold (issue #57)
        let entries = entries_of(300);
        let blocks = blocks_of(&entries, 256);
        for count in ["299", "301", "0", "256", "512"] {
            assert_eq!(
                invalid(&blocks_file("256", count, &blocks)).reason,
                Invalid::Count,
                "count {count}"
            );
        }
        assert_eq!(
            invalid(&blocks_file("256", "1", &[])).reason,
            Invalid::Count
        );
        assert_eq!(
            invalid(&blocks_file("256", "0", &blocks_of(&entries_of(1), 256))).reason,
            Invalid::Count
        );
    }

    #[test]
    fn a_block_but_the_last_that_isnt_full_is_invalid() {
        // CMP-R31: every block but the last holds exactly blockSize lines (issue #57)
        let entries = entries_of(10);
        let mut blocks = blocks_of(&entries, 4); // 4, 4, 2
        blocks.swap(1, 2); // 4, 2, 4
        assert_eq!(
            invalid(&blocks_file("4", "10", &blocks)).reason,
            Invalid::Block(2)
        );
        // a short first block, with the count that matches
        let short = vec![entries[..3].join("\n"), entries[3..7].join("\n")];
        assert_eq!(
            invalid(&blocks_file("4", "7", &short)).reason,
            Invalid::Block(1)
        );
    }

    #[test]
    fn a_last_block_over_block_size_is_invalid() {
        // CMP-R31: the last block holds 1 … blockSize lines (issue #57)
        let entries = entries_of(5);
        let blocks = vec![entries[..4].join("\n"), entries[4..].join("\n")];
        assert_eq!(
            invalid(&blocks_file("3", "5", &blocks)).reason,
            Invalid::Block(1)
        );
        assert_eq!(
            invalid(&blocks_file("4", "5", &[entries.join("\n")])).reason,
            Invalid::Block(1)
        );
    }

    #[test]
    fn an_empty_block_is_invalid_wherever_it_stands() {
        // CMP-R31: a block is never an empty string (issue #57)
        let entries = entries_of(8);
        let mut blocks = blocks_of(&entries, 4);
        blocks.push(String::new());
        assert_eq!(
            invalid(&blocks_file("4", "8", &blocks)).reason,
            Invalid::Block(3)
        );
        let mut blocks = blocks_of(&entries, 4);
        blocks.insert(1, String::new());
        assert_eq!(
            invalid(&blocks_file("4", "8", &blocks)).reason,
            Invalid::Block(2)
        );
        assert_eq!(
            invalid(&blocks_file("4", "0", &[String::new()])).reason,
            Invalid::Block(1)
        );
    }

    #[test]
    fn a_block_starting_below_0x20_is_an_unreadable_log() {
        // CMP-R31: a block whose first byte is below 0x20 is reserved, so the log
        // is unreadable (issue #57)
        let entries = entries_of(6);
        for first in ['\u{1}', '\u{8}', '\u{1f}', '\n', '\r', '\t', '\0'] {
            let mut blocks = blocks_of(&entries, 3);
            blocks[1] = format!("{first}{}", blocks[1]);
            assert_eq!(
                invalid(&blocks_file("3", "6", &blocks)).reason,
                Invalid::ReservedBlock(2),
                "first byte {:#x}",
                first as u32
            );
            // reserved wins over a count that doesn't match
            assert_eq!(
                invalid(&blocks_file("3", "99", &blocks)).reason,
                Invalid::ReservedBlock(2)
            );
        }
        // 0x20 and above is not reserved: the entry header decides
        let mut blocks = blocks_of(&entries, 3);
        blocks[0] = format!(" {}", blocks[0]);
        assert_eq!(
            invalid(&blocks_file("3", "6", &blocks)).reason,
            Invalid::EntryHeader(1)
        );
    }

    #[test]
    fn a_crlf_keeps_its_carriage_return_and_an_empty_line_is_a_bad_entry() {
        // CMP-R31: blocks split on LF alone, so a CR stays in its entry (LOG-R101)
        let blocks = vec!["C1|1|1791735050|0|a\r\nC1|1|1791735051|0|b".to_owned()];
        let log = valid(&blocks_file("256", "2", &blocks));
        assert_eq!(log.entry(1), Some(&b"C1|1|1791735050|0|a\r"[..]));
        assert_eq!(log.entry(2), Some(&b"C1|1|1791735051|0|b"[..]));
        // an empty line inside a block is an entry with no header
        let blocks = vec!["C1|1|1791735050|0\n\nC1|1|1791735051|0".to_owned()];
        assert_eq!(
            invalid(&blocks_file("256", "3", &blocks)).reason,
            Invalid::EntryHeader(2)
        );
        // a trailing LF is an empty last line, which `count` or the header refuses
        let blocks = vec!["C1|1|1791735050|0\nC1|1|1791735051|0\n".to_owned()];
        assert_eq!(
            invalid(&blocks_file("256", "2", &blocks)).reason,
            Invalid::Count
        );
        assert_eq!(
            invalid(&blocks_file("256", "3", &blocks)).reason,
            Invalid::EntryHeader(3)
        );
    }

    #[test]
    fn entry_checks_number_their_seq_across_blocks() {
        // CMP-R31: header and character checks run on the expanded entries (issue #57)
        let mut entries = entries_of(6);
        entries[4] = "C1|9|1791735060|0".to_owned();
        assert_eq!(invalid(&format4(&entries, 3)).reason, Invalid::EntryChar(5));
        entries[4] = "?".to_owned();
        assert_eq!(
            invalid(&format4(&entries, 3)).reason,
            Invalid::EntryHeader(5)
        );
    }

    #[test]
    fn recovery_reads_format_4_entries_without_header_checks() {
        // Recovery reads malformed entries as data (parse_recovery), blocks included
        let blocks = vec!["junk\nC1|1|1791735051|0".to_owned()];
        let Found::Valid(log) =
            parse_recovery(blocks_file("256", "2", &blocks).as_bytes()).expect("parses")
        else {
            panic!("not valid");
        };
        assert_eq!(log.entry(1), Some(&b"junk"[..]));
        // the reserved byte and the counts still apply
        let reserved = vec!["\u{1}x".to_owned()];
        assert!(matches!(
            parse_recovery(blocks_file("256", "1", &reserved).as_bytes()).expect("parses"),
            Found::Invalid(InvalidLog {
                reason: Invalid::ReservedBlock(1),
                ..
            })
        ));
    }

    #[test]
    fn a_missing_or_malformed_block_size_count_or_blocks_is_invalid() {
        // CMP-R31: blockSize, count and blocks read as integer literals and a list of strings
        let blocks = blocks_of(&entries_of(2), 256);
        for size in [
            "0",
            "-1",
            "1.5",
            "\"256\"",
            "nil",
            "true",
            "9007199254740992",
        ] {
            assert_eq!(
                invalid(&blocks_file(size, "2", &blocks)).reason,
                Invalid::BlockSize,
                "blockSize {size}"
            );
        }
        for count in ["-1", "1.5", "\"2\"", "nil", "0x2", "02"] {
            assert_eq!(
                invalid(&blocks_file("256", count, &blocks)).reason,
                Invalid::Count,
                "count {count}"
            );
        }
        let source = blocks_file("256", "2", &blocks);
        let no_blocks = source.replace("[\"blocks\"]", "[\"blocks2\"]");
        assert_eq!(invalid(&no_blocks).reason, Invalid::Blocks);
        for blocks in [
            "\"abc\"",
            "{ 5 }",
            "{ [1] = \"C1|1|1|0\", [3] = \"C1|1|2|0\" }",
            "{ \"C1|1|1|0\", x = \"y\" }",
        ] {
            let source = format!(
                "PagleLog = {{ format = 4, logId = \"3f9a2c1e7b0d4a65\", characters = {{ {} }},\n \
                 blockSize = 256, count = 1, blocks = {blocks} }}",
                ada()
            );
            assert_eq!(invalid(&source).reason, Invalid::Blocks, "{blocks}");
        }
    }

    #[test]
    fn a_log_keeps_to_its_formats_storage() {
        // CMP-R31: format 4 reads blocks and formats 1 to 3 read entries (issue #57)
        let four = format4(&entries_of(2), 256);
        let with_entries_only = four
            .replace("[\"blocks\"]", "[\"entries\"]")
            .replace("[\"blockSize\"] = 256,", "")
            .replace("[\"count\"] = 2,", "");
        assert_eq!(invalid(&with_entries_only).reason, Invalid::Blocks);
        let three_with_blocks = format!(
            "PagleLog = {{ format = 3, logId = \"3f9a2c1e7b0d4a65\", characters = {{ {} }},\n \
             blockSize = 256, count = 1, blocks = {{ \"C1|1|1791735050|0\" }} }}",
            ada()
        );
        assert_eq!(invalid(&three_with_blocks).reason, Invalid::Entries);
        for format in ["1", "2", "3"] {
            valid(&file(
                format,
                "\"3f9a2c1e7b0d4a65\"",
                &format!("{{ {} }}", ada()),
                &format!("{{ {ENTRY_1} }}"),
            ));
        }
    }

    #[test]
    fn a_format_above_4_reads_as_blocks_and_one_stored_as_entries_is_invalid_blocks() {
        // CMP-R31: format is any integer literal of at least 1, so the backend answers
        // `unsupported_format` for one it can't read; the block layout is frozen (LOG-R17)
        let entries = entries_of(300);
        for format in ["5", "12", "9007199254740991"] {
            let source = format4(&entries, 256)
                .replace("[\"format\"] = 4,", &format!("[\"format\"] = {format},"));
            let log = valid(&source);
            assert_eq!(log.format, format.parse::<u64>().unwrap());
            assert_eq!(log.len(), 300);
        }
        // entries alone, as formats 1 to 3 store them, is not readable at 12
        let source = format!(
            "PagleLog = {{ format = 12, logId = \"3f9a2c1e7b0d4a65\", characters = {{ {} }},\n \
             entries = {{ {ENTRY_1} }} }}",
            ada()
        );
        let log = invalid(&source);
        assert_eq!(log.reason, Invalid::Blocks);
        assert_eq!(log.log_id.as_deref(), Some("3f9a2c1e7b0d4a65"));
    }

    #[test]
    fn format_4_reasons_never_name_entry_text() {
        // CMP-R112: reasons name indexes and counts only
        assert_eq!(
            Invalid::Block(3).to_string(),
            "block 3 doesn't hold a valid number of entries"
        );
        assert_eq!(
            Invalid::ReservedBlock(2).to_string(),
            "block 2 starts with a reserved byte"
        );
    }
}

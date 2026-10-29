//! Building a page of an upload run: the upload document of UPL Contracts
//! for a slice of a valid local log, and its raw DEFLATE body (CMP
//! Contracts, Building an upload document; CMP-R47).

use std::io::Write;

use flate2::Compression;
use flate2::write::DeflateEncoder;

use crate::pagle_log::{Character, LocalLog};

/// The body's `Content-Type` (UPL-R2).
pub const CONTENT_TYPE: &str = "application/vnd.pagle.upload+deflate";

/// The upload document version this Companion writes (UPL Tunables).
pub const DOCUMENT_VERSION: u32 = 2;

/// The compressed cap, per Upload (UPL Tunables).
pub const COMPRESSED_CAP: usize = 2_097_152;

/// The decompressed cap, per Upload (UPL Tunables).
pub const DECOMPRESSED_CAP: usize = 16_777_216;

/// The default DEFLATE compression level the body is written at.
const LEVEL: u32 = 6;

/// One page: `count` entries from `seq from`, as a document and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub from: u64,
    pub count: u64,
    /// The upload document's bytes.
    pub document: Vec<u8>,
    /// The request body: raw DEFLATE (RFC 1951) of `document`.
    pub body: Vec<u8>,
}

impl Page {
    /// Whether the page is within both caps; a page that isn't is rebuilt
    /// with half the `count` (CMP-R47 step 4).
    pub fn fits_caps(&self) -> bool {
        self.body.len() <= COMPRESSED_CAP && self.document.len() <= DECOMPRESSED_CAP
    }
}

/// The page of `log` holding `count` entries from `seq from`, as of `as_of`
/// (CMP-R47 step 3). `None` when `from` is 0 or the page would run past the
/// last entry (`from + count − 1 > L`, UPL-R10 step 8).
pub fn page(log: &LocalLog, from: u64, count: u64, as_of: u64) -> Option<Page> {
    if from == 0 || from.checked_add(count)? - 1 > log.len() {
        return None;
    }
    let anchor = log.checkpoint(from - 1)?;

    let mut body = Vec::new();
    for character in &log.characters {
        write_character(&mut body, character);
    }
    for seq in from..from + count {
        body.extend_from_slice(log.entry(seq)?);
        body.push(b'\n');
    }

    let header = format!(
        "PGL|{DOCUMENT_VERSION}|{}|{}|{as_of}|{}|{}|{from}|{anchor}|{count}|{}|{}\n",
        log.log_id,
        log.format,
        log.len(),
        log.characters.len(),
        body.len(),
        crc32(&body),
    );
    let mut document = header.into_bytes();
    document.extend_from_slice(&body);
    let body = deflate(&document);
    Some(Page {
        from,
        count,
        document,
        body,
    })
}

/// A character line: `guid|name|surname|classID|raceID|faction|realmID|
/// hardcore|firstSeen|lastSeen`, nil written as empty and `selfFound` left
/// out (CMP Contracts, Building an upload document).
fn write_character(out: &mut Vec<u8>, c: &Character) {
    let hardcore = match c.hardcore {
        Some(true) => "1",
        Some(false) => "0",
        None => "",
    };
    let line = format!(
        "{}|{}|{}|{}|{}|{}|{}|{hardcore}|{}|{}\n",
        c.guid,
        c.name,
        c.surname.as_deref().unwrap_or(""),
        c.class_id,
        c.race_id,
        c.faction,
        c.realm_id,
        c.first_seen,
        c.last_seen,
    );
    out.extend_from_slice(line.as_bytes());
}

/// Raw DEFLATE (RFC 1951, no zlib or gzip wrapper) at the default level, 6.
fn deflate(document: &[u8]) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(LEVEL));
    encoder
        .write_all(document)
        .expect("writing to a Vec doesn't fail");
    encoder.finish().expect("writing to a Vec doesn't fail")
}

/// The CRC-32 of `bytes` (zlib / ISO-HDLC), as 8 lowercase hex digits (UPL
/// Contracts).
pub fn crc32(bytes: &[u8]) -> String {
    format!("{:08x}", crc32fast::hash(bytes))
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use flate2::read::DeflateDecoder;

    use super::*;
    use crate::pagle_log::{self, Found};

    const ADA: &str = r#"{ guid = "Player-4618-0A1B2C3D", name = "Ada", surname = "Brook",
        classID = 1, raceID = 1, faction = "Alliance", realmID = 4618, hardcore = false,
        selfFound = false, firstSeen = 1791731680, lastSeen = 1791735090 }"#;
    const BO: &str = r#"{ guid = "Player-4618-0A1B2C3E", name = "Bo", classID = 8, raceID = 4,
        faction = "Horde", realmID = 4618, hardcore = true, firstSeen = 1, lastSeen = 2 }"#;
    const CY: &str = r#"{ guid = "Player-1-F", name = "Cy", surname = "Dale", classID = 0,
        raceID = 0, faction = "Alliance", realmID = 1, firstSeen = 0, lastSeen = 0 }"#;

    /// The example's lines (UPL Contracts, The upload document).
    const EXAMPLE_CHARACTER: &str =
        "Player-4618-0A1B2C3D|Ada|Brook|1|1|Alliance|4618|0|1791731680|1791735090";
    const EXAMPLE_ENTRY_11: &str = "C1|1|1791735050|0|c|494|1439|1|148|453|86|25|263|||6358x1";
    const EXAMPLE_ENTRY_12: &str = "S1|1|1791735090|0|87|150|25";
    /// `body-bytes` and `body-crc` of the example's three lines, computed
    /// apart from this code (Python's `zlib.crc32`).
    const EXAMPLE_BODY_BYTES: usize = 159;
    const EXAMPLE_BODY_CRC: &str = "d28fda68";

    fn log(characters: &[&str], entries: &[String]) -> LocalLog {
        let entries: Vec<String> = entries.iter().map(|e| format!("{e:?}")).collect();
        let source = format!(
            "PagleLog = {{ format = 1, logId = \"3f9a2c1e7b0d4a65\", characters = {{ {} }}, entries = {{ {} }} }}",
            characters.join(", "),
            entries.join(", ")
        );
        match pagle_log::parse(source.as_bytes()).expect("parses") {
            Found::Valid(log) => log,
            other => panic!("not valid: {other:?}"),
        }
    }

    /// The example's log: twelve entries, of which 11 and 12 are the
    /// example's and 10 was caught at the anchor's time.
    fn example_log() -> LocalLog {
        let mut entries: Vec<String> = (1..=9)
            .map(|i| format!("S1|1|17917300{i:02}|0|80|150|25"))
            .collect();
        entries.push("S1|1|1791735000|0|86|150|25".into());
        entries.push(EXAMPLE_ENTRY_11.into());
        entries.push(EXAMPLE_ENTRY_12.into());
        log(&[ADA], &entries)
    }

    fn lines(document: &[u8]) -> Vec<&str> {
        let text = std::str::from_utf8(document).expect("UTF-8");
        let body = text.strip_suffix('\n').expect("ends in LF");
        body.split('\n').collect()
    }

    fn inflate(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        DeflateDecoder::new(body)
            .read_to_end(&mut out)
            .expect("raw DEFLATE");
        out
    }

    #[test]
    fn the_crc_is_zlibs_with_its_check_value() {
        // UPL Contracts: CRC-32 check value for `123456789` is cbf43926
        assert_eq!(crc32(b"123456789"), "cbf43926");
        assert_eq!(crc32(b""), "00000000");
    }

    #[test]
    fn a_first_page_of_the_example_matches_its_lines() {
        // CMP Contracts, Building an upload document: f = 1, count = 2 against UPL's example
        let log = log(&[ADA], &[EXAMPLE_ENTRY_11.into(), EXAMPLE_ENTRY_12.into()]);
        let page = page(&log, 1, 2, 1791735100).expect("in range");
        assert_eq!(
            lines(&page.document),
            [
                format!(
                    "PGL|2|3f9a2c1e7b0d4a65|1|1791735100|2|1|1|aaca407c7c0a15ab37059a90380e117eb02b79f9f367ee6373b6fef6532a0bd4|2|{EXAMPLE_BODY_BYTES}|{EXAMPLE_BODY_CRC}"
                )
                .as_str(),
                EXAMPLE_CHARACTER,
                EXAMPLE_ENTRY_11,
                EXAMPLE_ENTRY_12,
            ]
        );
        assert_eq!((page.from, page.count), (1, 2));
    }

    #[test]
    fn a_format_4_log_gives_the_same_lines_as_the_format_3_log_and_its_own_format() {
        // CMP Contracts, Building an upload document: one entry per line, the header
        // carrying the log's own format (issue #57)
        let entries: Vec<String> = (0..600)
            .map(|i| format!("S1|1|{}|{}|87|150|25", 1791730000 + i, i % 2))
            .collect();
        let three = log(&[ADA], &entries);
        let blocks: Vec<String> = entries
            .chunks(256)
            .map(|chunk| format!("{:?}", chunk.join("\n")))
            .collect();
        let source = format!(
            "PagleLog = {{ format = 4, logId = \"3f9a2c1e7b0d4a65\", characters = {{ {ADA} }}, \
             blockSize = 256, count = 600, blocks = {{ {} }} }}",
            blocks.join(", ")
        );
        let Found::Valid(four) = pagle_log::parse(source.as_bytes()).expect("parses") else {
            panic!("not valid");
        };
        for (from, count) in [(1, 600), (250, 10), (257, 256), (600, 1)] {
            let (a, b) = (
                page(&three, from, count, 7).unwrap(),
                page(&four, from, count, 7).unwrap(),
            );
            let (a, b) = (lines(&a.document), lines(&b.document));
            assert_eq!(a[1..], b[1..], "from {from}");
            assert_eq!(a[0].split('|').nth(3), Some("1"));
            assert_eq!(b[0].split('|').nth(3), Some("4"));
        }
    }

    #[test]
    fn the_examples_page_matches_its_header_and_lines_exactly() {
        // UPL Contracts, the example: header, character line and entry lines
        let page = page(&example_log(), 11, 2, 1791735100).expect("in range");
        let expected = format!(
            "PGL|2|3f9a2c1e7b0d4a65|1|1791735100|12|1|11|9b5c1bd975d4f8decca456dcd05b5291f00d16941d4616f8db1ed8b1b965646c|2|\
             {EXAMPLE_BODY_BYTES}|{EXAMPLE_BODY_CRC}\n\
             {EXAMPLE_CHARACTER}\n{EXAMPLE_ENTRY_11}\n{EXAMPLE_ENTRY_12}\n"
        );
        assert_eq!(String::from_utf8(page.document).unwrap(), expected);
    }

    #[test]
    fn body_bytes_and_crc_cover_every_byte_after_the_headers_line_feed() {
        // CMP Contracts, Building an upload document: body-bytes and body-crc
        let page = page(&example_log(), 3, 9, 7).expect("in range");
        let lf = page.document.iter().position(|&b| b == b'\n').unwrap();
        let header = std::str::from_utf8(&page.document[..lf]).unwrap();
        let rest = &page.document[lf + 1..];
        let fields: Vec<&str> = header.split('|').collect();
        assert_eq!(fields.len(), 12);
        assert_eq!(fields[10], rest.len().to_string());
        assert_eq!(fields[11], crc32(rest));
    }

    #[test]
    fn character_lines_write_nil_as_empty_and_hardcore_as_one_or_zero() {
        // CMP Contracts, Building an upload document: nil surname empty;
        // hardcore 1, 0 or empty; selfFound left out
        let log = log(&[ADA, BO, CY], &["S1|3|5|0".into()]);
        let page = page(&log, 1, 1, 9).expect("in range");
        assert_eq!(
            lines(&page.document)[1..],
            [
                EXAMPLE_CHARACTER,
                "Player-4618-0A1B2C3E|Bo||8|4|Horde|4618|1|1|2",
                "Player-1-F|Cy|Dale|0|0|Alliance|1||0|0",
                "S1|3|5|0",
            ]
        );
        assert!(lines(&page.document)[0].starts_with("PGL|2|3f9a2c1e7b0d4a65|1|9|1|3|1|aaca407c7c0a15ab37059a90380e117eb02b79f9f367ee6373b6fef6532a0bd4|1|"));
    }

    #[test]
    fn entry_lines_are_their_bytes_after_escapes() {
        // UPL Contracts: an entry line is its Lua string value, never its literal (UPL-R12)
        let source = "PagleLog = { format = 1, logId = \"3f9a2c1e7b0d4a65\", characters = { \
                      { guid = \"Player-1-F\", name = \"Cy\", classID = 0, raceID = 0, \
                      faction = \"A\", realmID = 1, firstSeen = 0, lastSeen = 0 } }, \
                      entries = { [[A1|1|0|0|caf\u{e9}]], \"A1|1|0|1|\\\"q\\\"\\r\\255\" } }";
        let Found::Valid(log) = pagle_log::parse(source.as_bytes()).unwrap() else {
            panic!("not valid");
        };
        let page = page(&log, 1, 2, 0).unwrap();
        assert!(
            page.document
                .ends_with(b"\nA1|1|0|0|caf\xc3\xa9\nA1|1|0|1|\"q\"\r\xff\n")
        );
    }

    #[test]
    fn the_body_inflates_as_raw_deflate_back_to_the_document() {
        // CMP Contracts, Building an upload document: raw DEFLATE (RFC 1951) at level 6
        let page = page(&example_log(), 1, 12, 1791735100).expect("in range");
        assert_eq!(inflate(&page.body), page.document);
        // A zlib or gzip wrapper would start with 0x78 or 0x1f 0x8b.
        assert_ne!(page.body[0], 0x78);
        assert_ne!(page.body[..2], [0x1f, 0x8b]);
        assert!(page.fits_caps());
    }

    #[test]
    fn a_page_past_the_last_entry_or_from_zero_is_not_built() {
        // UPL-R10 step 8: from + count − 1 is at most total; from is at least 1
        let log = example_log();
        assert!(page(&log, 0, 1, 0).is_none());
        assert!(page(&log, 12, 2, 0).is_none());
        assert!(page(&log, 1, 13, 0).is_none());
        assert!(page(&log, 1, 12, 0).is_some());
    }

    #[test]
    fn a_page_over_either_cap_does_not_fit() {
        // CMP-R47 step 4: over the compressed or the decompressed cap
        let over_document = Page {
            from: 1,
            count: 1,
            document: vec![b'x'; DECOMPRESSED_CAP + 1],
            body: Vec::new(),
        };
        assert!(!over_document.fits_caps());
        let over_body = Page {
            from: 1,
            count: 1,
            document: Vec::new(),
            body: vec![0; COMPRESSED_CAP + 1],
        };
        assert!(!over_body.fits_caps());
        let at_caps = Page {
            from: 1,
            count: 1,
            document: vec![b'x'; DECOMPRESSED_CAP],
            body: vec![0; COMPRESSED_CAP],
        };
        assert!(at_caps.fits_caps());
    }
}

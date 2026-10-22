//! The shared pushed-content test vector (HEV Contracts, Test vector), so the
//! Companion's check of `Pagle_Content` (CMP-R82, HEV-R51) is tested against
//! the bytes wow-fish's Core reads and the backend signs and serves, not only
//! against the grammar as read here.
//!
//! The files beside this one are copied byte for byte from pagle-backend at
//! `4e5c26d` (#238), `src/test/resources/pushed-content/`, which copied all
//! but `bad/` from wow-fish at `c856ccd0`, `spec/Pagle/fixtures/`:
//!
//! - `Content.lua`: wow-fish's signed `PagleContentData` tables `good`,
//!   `functionCall`, `singleQuote` and `big` (65,536 bytes of payload, the
//!   cap), each for `generatedAt` 1791728100, `seq` 42 and `expires`
//!   1792336500. It is a fixture holding four tables, not a data file:
//!   [`content_file`] lays each out as the backend serves it.
//! - `content/`: the decoded payloads of `good`, `functionCall` (a function
//!   call) and `singleQuote` (a single-quoted string); the last two are
//!   signed but outside the payload grammar.
//! - `bad/`: the backend's data files the checker refuses, each the `good`
//!   file with one defect: `over-cap.lua` (a payload of 65,537 bytes),
//!   `exponent.lua` (`seq = 42e0`), `leading-zero.lua` (`seq = 042`),
//!   `unknown-field.lua` (a tenth line, `extra = 1,`),
//!   `base64-line-break.lua` (a LF after the payload's 76th Base64
//!   character) and `crlf.lua` (CRLF line ends).
//!
//! `.gitattributes` here keeps every fixture as it is. When the backend copies
//! wow-fish's fixtures again, copy them again from it and name its commit
//! above.

/// `Content.lua`, wow-fish's four signed tables.
const CONTENT_LUA: &str = include_str!("Content.lua");

/// The header the vector's tables share.
pub const GENERATED_AT: u64 = 1_791_728_100;
pub const SEQ: u64 = 42;
pub const EXPIRES: u64 = 1_792_336_500;
pub const KEY_ID: &str = "testKey";

/// `content/good.txt`: `good`'s decoded payload.
pub const GOOD_PAYLOAD: &[u8] = include_bytes!("content/good.txt");
/// `content/function-call.txt`: `functionCall`'s decoded payload.
pub const FUNCTION_CALL_PAYLOAD: &[u8] = include_bytes!("content/function-call.txt");
/// `content/single-quote.txt`: `singleQuote`'s decoded payload.
pub const SINGLE_QUOTE_PAYLOAD: &[u8] = include_bytes!("content/single-quote.txt");

/// `bad/`, by file name.
pub const BAD: [(&str, &[u8]); 6] = [
    ("over-cap.lua", include_bytes!("bad/over-cap.lua")),
    ("exponent.lua", include_bytes!("bad/exponent.lua")),
    ("leading-zero.lua", include_bytes!("bad/leading-zero.lua")),
    ("unknown-field.lua", include_bytes!("bad/unknown-field.lua")),
    (
        "base64-line-break.lua",
        include_bytes!("bad/base64-line-break.lua"),
    ),
    ("crlf.lua", include_bytes!("bad/crlf.lua")),
];

/// One of `Content.lua`'s tables: its Base64 `payload` and `sig`.
#[derive(Debug, Clone)]
pub struct Signed {
    pub payload: String,
    pub sig: String,
}

/// The table `name` of `Content.lua` (`good`, `functionCall`, `singleQuote`
/// or `big`).
pub fn signed(name: &str) -> Signed {
    let start = CONTENT_LUA
        .find(&format!("\n\t{name} = {{\n"))
        .unwrap_or_else(|| panic!("no table {name} in Content.lua"));
    let table = &CONTENT_LUA[start..];
    let table = &table[..table.find("\n\t},").expect("the table ends")];
    let field = |key: &str| {
        let at = table.find(&format!("\t\t{key} = \"")).expect(key) + key.len() + 6;
        table[at..at + table[at..].find('"').unwrap()].to_owned()
    };
    // Every table shares the header the vector names.
    assert!(table.contains(&format!("\t\tgeneratedAt = {GENERATED_AT},\n")));
    assert!(table.contains(&format!("\t\tseq = {SEQ},\n")));
    assert!(table.contains(&format!("\t\texpires = {EXPIRES},\n")));
    assert!(table.contains(&format!("\t\tkeyId = \"{KEY_ID}\",\n")));
    Signed {
        payload: field("payload"),
        sig: field("sig"),
    }
}

/// A data file as HEV Contracts, `Pagle_Content`, lays it out, with the
/// vector's header and the given fields.
pub fn content_file_with(
    generated_at: &str,
    key_id: &str,
    seq: &str,
    expires: &str,
    payload: &str,
    sig: &str,
) -> String {
    format!(
        "PagleContentData = {{\n\
         \tv = 1,\n\
         \tgeneratedAt = {generated_at},\n\
         \tkeyId = \"{key_id}\",\n\
         \tseq = {seq},\n\
         \texpires = {expires},\n\
         \tpayload = \"{payload}\",\n\
         \tsig = \"{sig}\",\n\
         }}\n"
    )
}

/// The data file of `Content.lua`'s table `name`, as the backend serves it.
pub fn content_file(name: &str) -> String {
    let signed = signed(name);
    content_file_with(
        &GENERATED_AT.to_string(),
        KEY_ID,
        &SEQ.to_string(),
        &EXPIRES.to_string(),
        &signed.payload,
        &signed.sig,
    )
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    use super::*;

    fn decoded(name: &str) -> Vec<u8> {
        STANDARD.decode(signed(name).payload).unwrap()
    }

    #[test]
    fn the_tables_carry_the_payloads_of_content() {
        // HEV Contracts, Test vector: Content.lua signs content/'s payloads
        assert_eq!(decoded("good"), GOOD_PAYLOAD);
        assert_eq!(decoded("functionCall"), FUNCTION_CALL_PAYLOAD);
        assert_eq!(decoded("singleQuote"), SINGLE_QUOTE_PAYLOAD);
        // The big payload is exactly the payload cap (PUSH Tunables).
        assert_eq!(decoded("big").len(), 65_536);
        for name in ["good", "functionCall", "singleQuote", "big"] {
            assert_eq!(STANDARD.decode(signed(name).sig).unwrap().len(), 256);
        }
    }

    #[test]
    fn each_bad_file_is_the_good_file_with_its_one_defect() {
        // HEV Contracts, Test vector: bad/ is the good file with one defect,
        // so the good file built here is the backend's byte for byte
        let good = content_file("good");
        let bad = |name: &str| {
            let (_, bytes) = BAD.iter().find(|(n, _)| *n == name).unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        };
        assert_eq!(bad("crlf.lua"), good.replace('\n', "\r\n"));
        assert_eq!(
            bad("exponent.lua"),
            good.replace("\tseq = 42,", "\tseq = 42e0,")
        );
        assert_eq!(
            bad("leading-zero.lua"),
            good.replace("\tseq = 42,", "\tseq = 042,")
        );
        let last_line = good.len() - "}\n".len();
        assert_eq!(
            bad("unknown-field.lua"),
            format!("{}\textra = 1,\n}}\n", &good[..last_line])
        );
        let payload = signed("good").payload;
        assert_eq!(
            bad("base64-line-break.lua"),
            good.replace(&payload, &format!("{}\n{}", &payload[..76], &payload[76..]))
        );
        assert!(bad("over-cap.lua").starts_with("PagleContentData = {\n\tv = 1,\n"));
    }
}

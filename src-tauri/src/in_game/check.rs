//! Checking a downloaded data file body before it is written (CMP-R82,
//! IGV-R41, HEV-R51): [`check_following`], [`check_anglers`] and
//! [`check_content`] take the decoded body of a `200` and say whether it may
//! be written.
//!
//! A body passes only when it matches its file's grammar in
//! `in-game-views.md` (Contracts) byte for byte, with the constraints its
//! comments add and the Companion's two caps; a `Pagle_Content` body, the
//! grammar of `hunt-events.md` (Contracts, `Pagle_Content`) byte for byte,
//! its constraints and both content caps, and its decoded payload the payload
//! grammar as Core's reader takes it ([`payload`]). The signature is never
//! checked: only the addon holds keys (HEV-R51, PUSH-R12). Everything the addon would
//! treat as absent (IGV-R44) is a rejection, so a written file is always one
//! the addon reads; and since the grammars admit nothing but data, a server
//! can never put anything else into the game. Orderings the file alone can't
//! prove, such as `Pagle_Following`'s order by name and `publicId`, aren't
//! checked (CMP-R82).
//!
//! The check is one pass over the body with no I/O. Besides the body it
//! holds only the previous title of a block, so a full 16 MiB `Pagle_Anglers`
//! body costs about its own size in memory.
//!
//! A [`Rejection`] says where and by which rule, never what the body holds,
//! so its `Display` is safe for the local log (CMP-R112).

mod payload;

use std::cmp::Ordering;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// The followed cap (CMP Tunables, IGV-R41): at most this many Anglers in
/// `Pagle_Following`.
pub const FOLLOWED_CAP: usize = 200;

/// The Anglers cap (CMP Tunables, IGV-R41): at most this many bytes of
/// `Pagle_Anglers`.
pub const ANGLERS_CAP: usize = 16 * 1024 * 1024;

/// The content file cap (HEV Tunables, CMP Tunables "Content caps"): at most
/// this many bytes of `Pagle_Content`'s data file.
pub const CONTENT_CAP: usize = 90_112;

/// The payload cap (HEV Tunables, PUSH Tunables): at most this many bytes of
/// `Pagle_Content`'s decoded payload.
pub const PAYLOAD_CAP: usize = 65_536;

/// The bytes `sig` decodes to: one RSA-2048 signature (HEV Contracts,
/// `Pagle_Content`; HEV-R35).
pub const SIG_LEN: usize = 256;

/// The most digits of a header `int` (HEV Contracts, `Pagle_Content`), so
/// every one is below 2^53 (exact in the addon's doubles).
const INT_DIGITS: usize = 15;

/// The longest `keyId` (HEV Contracts, `Pagle_Content`; HEV-R40).
const KEY_ID_LEN: usize = 32;

/// Characters of one card in a block's `cards` (IGV Tunables, card width).
const CARD_WIDTH: usize = 16;

/// Characters of one key in a block's `keys` (IGV Tunables, key width).
const KEY_WIDTH: usize = 19;

/// Characters of a key's search key: its realm and UID (IGV Contracts).
const SEARCH_KEY_WIDTH: usize = 13;

/// One of the three data files (IGV-R40, HEV-R50).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataFile {
    /// `Pagle_Following`, the followed tier (IGV-R42).
    Following,
    /// `Pagle_Anglers`, the region tier (IGV-R43).
    Anglers,
    /// `Pagle_Content`, the pushed content (HEV-R50).
    Content,
}

impl DataFile {
    /// The addon folder's name, which is also how the spec names the file.
    pub fn name(self) -> &'static str {
        match self {
            DataFile::Following => "Pagle_Following",
            DataFile::Anglers => "Pagle_Anglers",
            DataFile::Content => "Pagle_Content",
        }
    }
}

/// Why a body was rejected. No variant carries anything from the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The body is longer than the Anglers cap.
    AnglersCap,
    /// `Pagle_Following` holds more angler lines than the followed cap.
    FollowedCap,
    /// The grammar's literal text (a header, footer, field name or
    /// separator) isn't here; the text it wanted is given.
    ExpectedText(&'static str),
    /// The grammar's production isn't here, for example a `nat` or an LF.
    Expected(&'static str),
    /// A `nat` starts with a `0` that isn't the whole number.
    LeadingZero,
    /// A Lua string runs to the end of the body.
    UnterminatedString,
    /// A Lua string holds a control byte or DEL unescaped.
    ControlByte,
    /// A Lua string holds an escape other than `\"`, `\\` and `\ddd`.
    BadEscape,
    /// A `\ddd` escape above 255, which Lua refuses to load.
    EscapeTooLarge,
    /// Something follows the closing `}` and its LF.
    TrailingBytes,
    /// `count` isn't the number of angler lines or blocks.
    CountMismatch,
    /// A block's `cards` isn't 16 × `cardCount` characters.
    CardsLength,
    /// A block's `keys` isn't 19 × `keyCount` characters.
    KeysLength,
    /// A card isn't 15 digits and a hardcore of `0` or `1`.
    BadCard,
    /// A key isn't 5 digits, 8 upper-case hex digits and 6 digits.
    BadKey,
    /// A card's title index is above the block's number of titles.
    TitleIndex,
    /// A key's card number is 0 or above the block's `cardCount`.
    CardNumber,
    /// A block's keys aren't in ascending byte order.
    KeysOrder,
    /// Two keys of a block share a search key.
    DuplicateSearchKey,
    /// A block's titles aren't in ascending byte order.
    TitlesOrder,
    /// A block names one title twice.
    DuplicateTitle,
    /// The body is longer than the content file cap.
    ContentCap,
    /// A header `int` has more than 15 digits.
    IntTooLong,
    /// `keyId` isn't 1 to 32 of `[A-Za-z0-9_-]`.
    BadKeyId,
    /// Base64 that isn't the standard alphabet, padded, on one line.
    BadBase64,
    /// Base64 whose padding bits aren't zero.
    Base64PaddingBits,
    /// `expires` isn't after `generatedAt`.
    ExpiresNotAfterGenerated,
    /// `sig` doesn't decode to exactly 256 bytes.
    SigLength,
    /// The decoded payload is longer than the payload cap.
    PayloadCap,
    /// The decoded payload isn't valid UTF-8 from its byte `at`.
    PayloadUtf8 { at: usize },
    /// The decoded payload breaks the payload grammar at its byte `at`, for
    /// the reason Core's reader gives (in words).
    PayloadGrammar { at: usize, what: &'static str },
}

impl Reason {
    /// The rule the body broke, for the log line.
    pub fn rule(&self) -> &'static str {
        match self {
            Reason::AnglersCap | Reason::FollowedCap => "IGV-R41, CMP-R82",
            Reason::ExpectedText(_) | Reason::Expected(_) | Reason::TrailingBytes => {
                "IGV Contracts"
            }
            Reason::LeadingZero
            | Reason::UnterminatedString
            | Reason::ControlByte
            | Reason::BadEscape
            | Reason::EscapeTooLarge => "IGV Contracts, Lua strings",
            Reason::CountMismatch | Reason::CardsLength | Reason::KeysLength => "IGV-R41, IGV-R44",
            Reason::BadCard | Reason::BadKey | Reason::TitleIndex | Reason::CardNumber => {
                "IGV Contracts, Pagle_Anglers"
            }
            Reason::KeysOrder
            | Reason::DuplicateSearchKey
            | Reason::TitlesOrder
            | Reason::DuplicateTitle => "IGV-R41",
            Reason::ContentCap | Reason::PayloadCap => "HEV-R51, CMP-R82",
            Reason::IntTooLong
            | Reason::BadKeyId
            | Reason::BadBase64
            | Reason::Base64PaddingBits
            | Reason::ExpiresNotAfterGenerated
            | Reason::SigLength
            | Reason::PayloadUtf8 { .. } => "HEV Contracts, Pagle_Content",
            Reason::PayloadGrammar { .. } => "HEV-R51, PUSH-R20",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::AnglersCap => {
                write!(f, "the body is over the Anglers cap of {ANGLERS_CAP} bytes")
            }
            Reason::FollowedCap => {
                write!(f, "more than the followed cap of {FOLLOWED_CAP} Anglers")
            }
            Reason::ExpectedText(text) => write!(f, "expected the text `{text}`"),
            Reason::Expected(what) => write!(f, "expected {what}"),
            Reason::LeadingZero => write!(f, "a nat has a leading zero"),
            Reason::UnterminatedString => write!(f, "a string isn't closed"),
            Reason::ControlByte => write!(f, "a string holds an unescaped control byte or DEL"),
            Reason::BadEscape => {
                write!(f, r#"a string has an escape other than \", \\ or \ddd"#)
            }
            Reason::EscapeTooLarge => write!(f, r"a \ddd escape is above 255"),
            Reason::TrailingBytes => write!(f, "bytes follow the closing `}}` and its LF"),
            Reason::CountMismatch => {
                write!(f, "count isn't the number of entries that follow it")
            }
            Reason::CardsLength => write!(f, "cards isn't 16 × cardCount characters"),
            Reason::KeysLength => write!(f, "keys isn't 19 × keyCount characters"),
            Reason::BadCard => write!(f, "a card isn't 15 digits and a hardcore of 0 or 1"),
            Reason::BadKey => {
                write!(
                    f,
                    "a key isn't a 5-digit realm, 8-digit upper-case hex UID and 6-digit card number"
                )
            }
            Reason::TitleIndex => write!(f, "a card's title index is past the block's titles"),
            Reason::CardNumber => write!(f, "a key's card number is 0 or past cardCount"),
            Reason::KeysOrder => write!(f, "keys aren't in ascending byte order"),
            Reason::DuplicateSearchKey => write!(f, "two keys share a search key"),
            Reason::TitlesOrder => write!(f, "titles aren't in ascending byte order"),
            Reason::DuplicateTitle => write!(f, "a title is given twice"),
            Reason::ContentCap => {
                write!(
                    f,
                    "the body is over the content file cap of {CONTENT_CAP} bytes"
                )
            }
            Reason::IntTooLong => write!(f, "an int has more than {INT_DIGITS} digits"),
            Reason::BadKeyId => {
                write!(f, "keyId isn't 1 to {KEY_ID_LEN} of A-Z, a-z, 0-9, _ and -")
            }
            Reason::BadBase64 => write!(
                f,
                "not padded standard Base64 on one line (RFC 4648 section 4)"
            ),
            Reason::Base64PaddingBits => write!(f, "Base64 whose padding bits aren't zero"),
            Reason::ExpiresNotAfterGenerated => write!(f, "expires isn't after generatedAt"),
            Reason::SigLength => write!(f, "sig doesn't decode to {SIG_LEN} bytes"),
            Reason::PayloadCap => {
                write!(
                    f,
                    "the decoded payload is over the payload cap of {PAYLOAD_CAP} bytes"
                )
            }
            Reason::PayloadUtf8 { at } => {
                write!(f, "the decoded payload isn't UTF-8 at its byte {at}")
            }
            Reason::PayloadGrammar { at, what } => {
                write!(
                    f,
                    "the decoded payload breaks its grammar at its byte {at}: {what}"
                )
            }
        }
    }
}

/// A body that may not be written (CMP-R82): which file, where, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// The data file the body was for.
    pub file: DataFile,
    /// The 0-based byte offset in the body where the check failed.
    pub offset: usize,
    /// The 1-based line of `offset`.
    pub line: usize,
    /// Why.
    pub reason: Reason,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} body rejected at line {}, byte {}: {} ({})",
            self.file.name(),
            self.line,
            self.offset,
            self.reason,
            self.reason.rule()
        )
    }
}

impl std::error::Error for Rejection {}

/// Checks `body` as the data file `file` (CMP-R82).
pub fn check(file: DataFile, body: &[u8]) -> Result<(), Rejection> {
    match file {
        DataFile::Following => check_following(body),
        DataFile::Anglers => check_anglers(body),
        DataFile::Content => check_content(body),
    }
}

/// Checks a decoded `Pagle_Following` body against the `following-file`
/// grammar, its count and the followed cap (CMP-R82, IGV-R41).
pub fn check_following(body: &[u8]) -> Result<(), Rejection> {
    following(&mut Cursor::new(body)).map_err(|fail| fail.rejection(DataFile::Following, body))
}

/// Checks a decoded `Pagle_Anglers` body against the `anglers-file` grammar,
/// its counts, lengths and orders, and the Anglers cap (CMP-R82, IGV-R41).
pub fn check_anglers(body: &[u8]) -> Result<(), Rejection> {
    let checked = if body.len() > ANGLERS_CAP {
        reject_at(ANGLERS_CAP, Reason::AnglersCap)
    } else {
        anglers(&mut Cursor::new(body))
    };
    checked.map_err(|fail| fail.rejection(DataFile::Anglers, body))
}

/// Checks a decoded `Pagle_Content` body (CMP-R82, HEV-R51): the `file`
/// grammar of HEV Contracts, `Pagle_Content`, byte for byte, with its
/// constraints (`expires > generatedAt`, a 256-byte `sig`), the content file
/// cap and the payload cap, and the decoded payload against the payload
/// grammar as Core's reader takes it. Never the signature.
pub fn check_content(body: &[u8]) -> Result<(), Rejection> {
    let checked = if body.len() > CONTENT_CAP {
        reject_at(CONTENT_CAP, Reason::ContentCap)
    } else {
        content(&mut Cursor::new(body))
    };
    checked.map_err(|fail| fail.rejection(DataFile::Content, body))
}

/// Where and why a check failed, before it is told which file it was.
struct Fail {
    at: usize,
    reason: Reason,
}

impl Fail {
    fn rejection(self, file: DataFile, body: &[u8]) -> Rejection {
        let before = &body[..self.at.min(body.len())];
        Rejection {
            file,
            offset: self.at,
            line: 1 + before.iter().filter(|&&b| b == b'\n').count(),
            reason: self.reason,
        }
    }
}

type Step<T = ()> = Result<T, Fail>;

/// A failure at byte `at`.
fn reject_at<T>(at: usize, reason: Reason) -> Step<T> {
    Err(Fail { at, reason })
}

/// `following-file` (IGV Contracts, `Pagle_Following`).
fn following(c: &mut Cursor) -> Step {
    c.line("PagleFollowingData = {")?;
    c.line("v = 1,")?;
    c.text("generatedAt = ")?;
    c.nat()?;
    c.line(",")?;
    c.text("count = ")?;
    let count_at = c.pos;
    let count = c.nat()?;
    c.line(",")?;
    c.line("anglers = {")?;
    let mut lines: u64 = 0;
    while c.peek() == Some(b'{') {
        if lines == FOLLOWED_CAP as u64 {
            return c.fail(Reason::FollowedCap);
        }
        angler_line(c)?;
        lines += 1;
    }
    c.line("},")?;
    c.line("ok = true,")?;
    c.line("}")?;
    c.end()?;
    if count != lines {
        return reject_at(count_at, Reason::CountMismatch);
    }
    Ok(())
}

/// `angler-line` (IGV Contracts, `Pagle_Following`).
fn angler_line(c: &mut Cursor) -> Step {
    c.text("{ name = ")?;
    c.lua_string()?;
    c.text(", more = ")?;
    c.nat()?;
    c.text(", hardcore = ")?;
    c.bool()?;
    c.text(", title = ")?;
    c.lua_string()?;
    for field in [
        ", level = ",
        ", prestige = ",
        ", ledger = ",
        ", legends = ",
        ", legendsTotal = ",
        ", waters = ",
        ", catches = ",
    ] {
        c.text(field)?;
        c.nat()?;
    }
    c.text(", asOf = ")?;
    c.digits(8, "8 digits")?;
    c.text(", guids = { ")?;
    if c.peek() == Some(b'"') {
        c.lua_string()?;
        while c.rest().starts_with(b", ") {
            c.text(", ")?;
            c.lua_string()?;
        }
    }
    c.line(" } },")
}

/// `anglers-file` (IGV Contracts, `Pagle_Anglers`).
fn anglers(c: &mut Cursor) -> Step {
    c.line("PagleAnglersData = {")?;
    c.line("v = 1,")?;
    c.text("generatedAt = ")?;
    c.nat()?;
    c.line(",")?;
    c.text("count = ")?;
    let count_at = c.pos;
    let count = c.nat()?;
    c.line(",")?;
    c.line("blocks = {")?;
    let mut blocks: u64 = 0;
    while c.peek() == Some(b'{') {
        block(c)?;
        blocks += 1;
    }
    c.line("},")?;
    c.line("ok = true,")?;
    c.line("}")?;
    c.end()?;
    if count != blocks {
        return reject_at(count_at, Reason::CountMismatch);
    }
    Ok(())
}

/// `block` (IGV Contracts, `Pagle_Anglers`), with the constraints of its
/// comments (IGV-R41, IGV-R44).
fn block(c: &mut Cursor) -> Step {
    c.text("{ region = ")?;
    c.lua_string()?;
    c.text(", builtAt = ")?;
    c.nat()?;
    c.text(", cardCount = ")?;
    let card_count = c.nat()?;
    c.text(", keyCount = ")?;
    let key_count = c.nat()?;

    c.text(", titles = { ")?;
    let mut titles: u64 = 0;
    if c.peek() == Some(b'"') {
        let mut previous = Vec::new();
        let mut title = Vec::new();
        loop {
            let at = c.pos;
            title.clear();
            c.lua_string_into(&mut title)?;
            if titles > 0 {
                match previous.cmp(&title) {
                    Ordering::Less => {}
                    Ordering::Equal => {
                        return reject_at(at, Reason::DuplicateTitle);
                    }
                    Ordering::Greater => {
                        return reject_at(at, Reason::TitlesOrder);
                    }
                }
            }
            std::mem::swap(&mut previous, &mut title);
            titles += 1;
            if !c.rest().starts_with(b", ") {
                break;
            }
            c.text(", ")?;
        }
    }
    c.text(" }, cards = ")?;

    let (cards_at, cards) = c.packed(|b| b.is_ascii_digit(), Reason::BadCard)?;
    if Some(cards.len() as u64) != card_count.checked_mul(CARD_WIDTH as u64) {
        return reject_at(cards_at, Reason::CardsLength);
    }
    for (n, card) in cards.as_chunks::<CARD_WIDTH>().0.iter().enumerate() {
        let at = cards_at + n * CARD_WIDTH;
        // 15 digits, already checked, then the hardcore flag.
        if !matches!(card[CARD_WIDTH - 1], b'0' | b'1') {
            return reject_at(at + CARD_WIDTH - 1, Reason::BadCard);
        }
        if decimal(&card[..3]) > titles {
            return reject_at(at, Reason::TitleIndex);
        }
    }

    c.text(", keys = ")?;
    let (keys_at, keys) = c.packed(
        |b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b),
        Reason::BadKey,
    )?;
    if Some(keys.len() as u64) != key_count.checked_mul(KEY_WIDTH as u64) {
        return reject_at(keys_at, Reason::KeysLength);
    }
    let mut previous: Option<&[u8]> = None;
    for (i, key) in keys.as_chunks::<KEY_WIDTH>().0.iter().enumerate() {
        let at = keys_at + i * KEY_WIDTH;
        // The UID is HEXUP, already checked; the realm and card number are
        // digits only.
        let (realm, rest) = key.split_at(5);
        let card_number = &rest[8..];
        if let Some(bad) = realm.iter().position(|b| !b.is_ascii_digit()) {
            return reject_at(at + bad, Reason::BadKey);
        }
        if let Some(bad) = card_number.iter().position(|b| !b.is_ascii_digit()) {
            return reject_at(at + 13 + bad, Reason::BadKey);
        }
        let card_number = decimal(card_number);
        if card_number == 0 || card_number > card_count {
            return reject_at(at + 13, Reason::CardNumber);
        }
        let search_key = &key[..SEARCH_KEY_WIDTH];
        if let Some(previous) = previous {
            // Keys ascending with distinct search keys means search keys
            // strictly ascending, since a search key is a key's prefix.
            match previous.cmp(search_key) {
                Ordering::Less => {}
                Ordering::Equal => {
                    return reject_at(at, Reason::DuplicateSearchKey);
                }
                Ordering::Greater => {
                    return reject_at(at, Reason::KeysOrder);
                }
            }
        }
        previous = Some(search_key);
    }
    c.line(" },")
}

/// `file` (HEV Contracts, `Pagle_Content`), then its constraints.
fn content(c: &mut Cursor) -> Step {
    c.line("PagleContentData = {")?;
    c.tab()?;
    c.line("v = 1,")?;
    c.tab()?;
    c.text("generatedAt = ")?;
    let generated_at = c.int()?;
    c.line(",")?;
    c.tab()?;
    c.text("keyId = \"")?;
    c.key_id()?;
    c.line("\",")?;
    c.tab()?;
    c.text("seq = ")?;
    c.int()?;
    c.line(",")?;
    c.tab()?;
    c.text("expires = ")?;
    let expires_at = c.pos;
    let expires = c.int()?;
    c.line(",")?;
    c.tab()?;
    c.text("payload = \"")?;
    let (payload_at, payload) = c.base64()?;
    c.line("\",")?;
    c.tab()?;
    c.text("sig = \"")?;
    let (sig_at, sig) = c.base64()?;
    c.line("\",")?;
    c.line("}")?;
    c.end()?;

    if expires <= generated_at {
        return reject_at(expires_at, Reason::ExpiresNotAfterGenerated);
    }
    let sig = decode(sig_at, sig)?;
    if sig.len() != SIG_LEN {
        return reject_at(sig_at, Reason::SigLength);
    }
    let payload = decode(payload_at, payload)?;
    if payload.len() > PAYLOAD_CAP {
        return reject_at(payload_at, Reason::PayloadCap);
    }
    // HEV Contracts, `Pagle_Content`: the payload is valid UTF-8, which
    // Core's reader doesn't itself require.
    if let Err(e) = std::str::from_utf8(&payload) {
        return reject_at(
            payload_at,
            Reason::PayloadUtf8 {
                at: e.valid_up_to(),
            },
        );
    }
    payload::check(&payload).map_err(|e| Fail {
        at: payload_at,
        reason: Reason::PayloadGrammar {
            at: e.at,
            what: e.what,
        },
    })
}

/// Decodes Base64 the grammar already admitted, refusing padding bits that
/// aren't zero (RFC 4648 section 3.5, as HEV Contracts' `b64` has it).
fn decode(at: usize, base64: &[u8]) -> Step<Vec<u8>> {
    STANDARD
        .decode(base64)
        .or_else(|_| reject_at(at, Reason::Base64PaddingBits))
}

/// The value of a run of ASCII digits, already checked to be digits.
fn decimal(digits: &[u8]) -> u64 {
    digits.iter().fold(0, |n, &d| n * 10 + u64::from(d - b'0'))
}

/// A read position in the body.
struct Cursor<'a> {
    body: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(body: &'a [u8]) -> Self {
        Cursor { body, pos: 0 }
    }

    fn fail<T>(&self, reason: Reason) -> Step<T> {
        reject_at(self.pos, reason)
    }

    fn peek(&self) -> Option<u8> {
        self.body.get(self.pos).copied()
    }

    fn rest(&self) -> &'a [u8] {
        &self.body[self.pos..]
    }

    /// The grammar's literal `text`, failing at the first byte that differs.
    fn text(&mut self, text: &'static str) -> Step {
        let rest = self.rest();
        let same = text
            .bytes()
            .zip(rest)
            .take_while(|(want, got)| want == *got)
            .count();
        if same < text.len() {
            return reject_at(self.pos + same, Reason::ExpectedText(text));
        }
        self.pos += text.len();
        Ok(())
    }

    /// `text` then LF.
    fn line(&mut self, text: &'static str) -> Step {
        self.text(text)?;
        if self.peek() != Some(b'\n') {
            return self.fail(Reason::Expected("LF"));
        }
        self.pos += 1;
        Ok(())
    }

    /// Nothing more.
    fn end(&self) -> Step {
        if self.pos != self.body.len() {
            return self.fail(Reason::TrailingBytes);
        }
        Ok(())
    }

    /// `nat`, whose value saturates at `u64::MAX`: only counts are compared,
    /// and no count that large can match what follows it.
    fn nat(&mut self) -> Step<u64> {
        match self.peek() {
            Some(b'0') => {
                if self.body.get(self.pos + 1).is_some_and(u8::is_ascii_digit) {
                    return self.fail(Reason::LeadingZero);
                }
                self.pos += 1;
                Ok(0)
            }
            Some(b'1'..=b'9') => {
                let mut n: u64 = 0;
                while let Some(d @ b'0'..=b'9') = self.peek() {
                    n = n.saturating_mul(10).saturating_add(u64::from(d - b'0'));
                    self.pos += 1;
                }
                Ok(n)
            }
            _ => self.fail(Reason::Expected("a nat")),
        }
    }

    /// One HTAB.
    fn tab(&mut self) -> Step {
        if self.peek() != Some(b'\t') {
            return self.fail(Reason::Expected("a tab"));
        }
        self.pos += 1;
        Ok(())
    }

    /// `int` of HEV Contracts, `Pagle_Content`: `"0" / (%x31-39 0*14DIGIT)`.
    /// A digit past the 15th fails here; anything else after it is the next
    /// production's to refuse.
    fn int(&mut self) -> Step<u64> {
        match self.peek() {
            Some(b'0') => {
                if self.body.get(self.pos + 1).is_some_and(u8::is_ascii_digit) {
                    return self.fail(Reason::LeadingZero);
                }
                self.pos += 1;
                Ok(0)
            }
            Some(b'1'..=b'9') => {
                let run = self
                    .rest()
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
                if run > INT_DIGITS {
                    return reject_at(self.pos + INT_DIGITS, Reason::IntTooLong);
                }
                let value = decimal(&self.rest()[..run]);
                self.pos += run;
                Ok(value)
            }
            _ => self.fail(Reason::Expected("an int")),
        }
    }

    /// `keyid` of HEV Contracts, `Pagle_Content`: `1*32(ALPHA / DIGIT / "_"
    /// / "-")`, up to its closing quote.
    fn key_id(&mut self) -> Step {
        let run = self
            .rest()
            .iter()
            .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            .count();
        if run == 0 || run > KEY_ID_LEN {
            return reject_at(self.pos + run.min(KEY_ID_LEN), Reason::BadKeyId);
        }
        if self.body.get(self.pos + run) != Some(&b'"') {
            return reject_at(self.pos + run, Reason::BadKeyId);
        }
        self.pos += run;
        Ok(())
    }

    /// `b64` of HEV Contracts, `Pagle_Content`, up to its closing quote:
    /// `*(4b64c) [2b64c "==" / 3b64c "="]`, the standard alphabet, padded,
    /// with no line breaks. Its offset and text.
    fn base64(&mut self) -> Step<(usize, &'a [u8])> {
        let start = self.pos;
        let rest = self.rest();
        let chars = rest
            .iter()
            .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
            .count();
        let pads = rest[chars..]
            .iter()
            .take(2)
            .take_while(|&&b| b == b'=')
            .count();
        let end = start + chars + pads;
        if self.body.get(end) != Some(&b'"') {
            return reject_at(end, Reason::BadBase64);
        }
        let whole = match pads {
            0 => chars % 4 == 0,
            1 => chars % 4 == 3,
            _ => chars % 4 == 2,
        };
        if !whole {
            return reject_at(start, Reason::BadBase64);
        }
        self.pos = end;
        Ok((start, &self.body[start..end]))
    }

    /// `bool`.
    fn bool(&mut self) -> Step {
        for word in ["true", "false"] {
            if self.rest().starts_with(word.as_bytes()) {
                self.pos += word.len();
                return Ok(());
            }
        }
        self.fail(Reason::Expected("a bool"))
    }

    /// Exactly `count` ASCII digits.
    fn digits(&mut self, count: usize, what: &'static str) -> Step {
        let run = self
            .rest()
            .iter()
            .take(count)
            .take_while(|b| b.is_ascii_digit())
            .count();
        if run < count {
            return reject_at(self.pos + run, Reason::Expected(what));
        }
        self.pos += count;
        Ok(())
    }

    /// `lua-string`, its value thrown away.
    fn lua_string(&mut self) -> Step {
        self.lua_string_into(&mut Vec::new())
    }

    /// `lua-string` (IGV Contracts, Lua strings), its value appended to
    /// `value`.
    fn lua_string_into(&mut self, value: &mut Vec<u8>) -> Step {
        if self.peek() != Some(b'"') {
            return self.fail(Reason::Expected("a Lua string"));
        }
        self.pos += 1;
        loop {
            match self.peek() {
                None => return self.fail(Reason::UnterminatedString),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(());
                }
                Some(b'\\') => match self.body.get(self.pos + 1) {
                    Some(&b @ (b'"' | b'\\')) => {
                        value.push(b);
                        self.pos += 2;
                    }
                    _ => {
                        let ddd = self.body.get(self.pos + 1..self.pos + 4);
                        let Some(ddd) = ddd.filter(|d| d.iter().all(u8::is_ascii_digit)) else {
                            return self.fail(Reason::BadEscape);
                        };
                        let Ok(b) = u8::try_from(decimal(ddd)) else {
                            return self.fail(Reason::EscapeTooLarge);
                        };
                        value.push(b);
                        self.pos += 4;
                    }
                },
                Some(b) if b < 0x20 || b == 0x7f => return self.fail(Reason::ControlByte),
                Some(b) => {
                    value.push(b);
                    self.pos += 1;
                }
            }
        }
    }

    /// A packed string, `DQUOTE *(allowed byte) DQUOTE`: its offset and
    /// contents. The first byte that isn't allowed must be the closing quote.
    fn packed(&mut self, allowed: fn(u8) -> bool, bad: Reason) -> Step<(usize, &'a [u8])> {
        self.text("\"")?;
        let start = self.pos;
        let rest = self.rest();
        let len = rest.iter().position(|&b| !allowed(b)).unwrap_or(rest.len());
        let end = start + len;
        if self.body.get(end) != Some(&b'"') {
            return reject_at(end, bad);
        }
        self.pos = end + 1;
        Ok((start, &self.body[start..end]))
    }
}

#[cfg(test)]
mod content_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use std::time::{Duration, Instant};

    /// IGV Contracts, `Pagle_Following`: the example, byte for byte.
    const FOLLOWING: &str = r#"PagleFollowingData = {
v = 1,
generatedAt = 1791735100,
count = 1,
anglers = {
{ name = "Ada", more = 1, hardcore = false, title = "Warden of Feralas Waters", level = 23, prestige = 1, ledger = 37, legends = 3, legendsTotal = 12, waters = 4, catches = 5120, asOf = 20261012, guids = { "Player-4618-0A1B2C3D", "Player-4618-0A1B2C3E" } },
},
ok = true,
}
"#;

    /// IGV Contracts, `Pagle_Anglers`: the example, byte for byte.
    const ANGLERS: &str = r#"PagleAnglersData = {
v = 1,
generatedAt = 1791734400,
count = 1,
blocks = {
{ region = "eu", builtAt = 1791734400, cardCount = 2, keyCount = 3, titles = { "Seeker of the Deep", "Warden of Feralas Waters" }, cards = "00223001037031200016001210007121", keys = "0439501234567000002046180A1B2C3D000001046180A1B2C3E000001" },
},
ok = true,
}
"#;

    /// The example's one angler line, LF included.
    const ADA: &str = "{ name = \"Ada\", more = 1, hardcore = false, title = \"Warden of Feralas Waters\", level = 23, prestige = 1, ledger = 37, legends = 3, legendsTotal = 12, waters = 4, catches = 5120, asOf = 20261012, guids = { \"Player-4618-0A1B2C3D\", \"Player-4618-0A1B2C3E\" } },\n";

    /// The example's one block, LF included.
    const EU: &str = "{ region = \"eu\", builtAt = 1791734400, cardCount = 2, keyCount = 3, titles = { \"Seeker of the Deep\", \"Warden of Feralas Waters\" }, cards = \"00223001037031200016001210007121\", keys = \"0439501234567000002046180A1B2C3D000001046180A1B2C3E000001\" },\n";

    /// A `Pagle_Following` body holding `lines` with `count` as given.
    fn following(count: usize, lines: &str) -> String {
        format!(
            "PagleFollowingData = {{\nv = 1,\ngeneratedAt = 1791735100,\ncount = {count},\nanglers = {{\n{lines}}},\nok = true,\n}}\n"
        )
    }

    /// A `Pagle_Anglers` body holding `blocks` with `count` as given.
    fn anglers(count: usize, blocks: &str) -> String {
        format!(
            "PagleAnglersData = {{\nv = 1,\ngeneratedAt = 1791734400,\ncount = {count},\nblocks = {{\n{blocks}}},\nok = true,\n}}\n"
        )
    }

    /// One block with the given counts, titles list, cards and keys.
    fn block(card_count: usize, key_count: usize, titles: &str, cards: &str, keys: &str) -> String {
        format!(
            "{{ region = \"eu\", builtAt = 1791734400, cardCount = {card_count}, keyCount = {key_count}, titles = {{ {titles} }}, cards = \"{cards}\", keys = \"{keys}\" }},\n"
        )
    }

    /// The example's angler line with one field's text replaced.
    fn ada_with(from: &str, to: &str) -> String {
        assert!(ADA.contains(from), "{from:?} not in the example line");
        following(1, &ADA.replacen(from, to, 1))
    }

    /// The example's block with one piece of text replaced.
    fn eu_with(from: &str, to: &str) -> String {
        assert!(EU.contains(from), "{from:?} not in the example block");
        anglers(1, &EU.replacen(from, to, 1))
    }

    fn following_fails(body: impl AsRef<[u8]>, reason: Reason) -> Rejection {
        let rejection = check_following(body.as_ref()).expect_err("should be rejected");
        assert_eq!(rejection.reason, reason, "{rejection}");
        assert_eq!(rejection.file, DataFile::Following);
        rejection
    }

    fn anglers_fails(body: impl AsRef<[u8]>, reason: Reason) -> Rejection {
        let rejection = check_anglers(body.as_ref()).expect_err("should be rejected");
        assert_eq!(rejection.reason, reason, "{rejection}");
        assert_eq!(rejection.file, DataFile::Anglers);
        rejection
    }

    /// A valid `Pagle_Anglers` body of exactly `size` bytes: one block of
    /// many cards and keys, padded to size by the length of its one title.
    fn anglers_of_size(size: usize) -> Vec<u8> {
        const KEYS: usize = 400_000;
        let fixed = anglers(1, &block(0, KEYS, "\"\"", "", "")).len();
        let cards = (size - fixed - 19 * KEYS - 64) / 16;
        let unpadded = anglers(1, &block(cards, KEYS, "\"\"", "", "")).len();
        let title_len = size - unpadded - 16 * cards - 19 * KEYS;
        let mut keys = String::with_capacity(19 * KEYS);
        for i in 0..KEYS {
            write!(keys, "04618{i:08X}{:06}", i % cards + 1).unwrap();
        }
        let body = anglers(
            1,
            &block(
                cards,
                KEYS,
                &format!("\"{}\"", "a".repeat(title_len)),
                &"0012300103703120".repeat(cards),
                &keys,
            ),
        );
        assert_eq!(body.len(), size);
        body.into_bytes()
    }

    // --- Pagle_Following ---

    #[test]
    fn the_following_example_passes() {
        // IGV Contracts, `Pagle_Following`: the example is a valid file
        assert_eq!(check_following(FOLLOWING.as_bytes()), Ok(()));
        assert_eq!(following(1, ADA), FOLLOWING);
    }

    #[test]
    fn a_following_file_with_no_anglers_passes() {
        // IGV-R34: the follower of no one gets a file with no Anglers
        assert_eq!(check_following(following(0, "").as_bytes()), Ok(()));
        assert_eq!(
            check(DataFile::Following, following(0, "").as_bytes()),
            Ok(())
        );
    }

    #[test]
    fn a_following_count_that_isnt_the_number_of_lines_fails() {
        // IGV-R41, IGV-R44: `count` is the number of angler lines
        following_fails(following(2, ADA), Reason::CountMismatch);
        following_fails(following(0, ADA), Reason::CountMismatch);
        following_fails(following(1, ""), Reason::CountMismatch);
        assert_eq!(
            check_following(following(2, &ADA.repeat(2)).as_bytes()),
            Ok(())
        );
    }

    #[test]
    fn a_following_version_other_than_1_fails() {
        // IGV-R44: an unknown `v` makes the file absent
        let rejection = following_fails(
            FOLLOWING.replace("v = 1,", "v = 2,"),
            Reason::ExpectedText("v = 1,"),
        );
        assert_eq!((rejection.line, rejection.offset), (2, 27));
        following_fails(
            FOLLOWING.replace("v = 1,", "v = 10,"),
            Reason::ExpectedText("v = 1,"),
        );
    }

    #[test]
    fn a_following_file_without_ok_true_fails() {
        // IGV-R44: `ok` isn't `true`
        following_fails(
            FOLLOWING.replace("ok = true,", "ok = false,"),
            Reason::ExpectedText("ok = true,"),
        );
        following_fails(
            FOLLOWING.replace("ok = true,\n", ""),
            Reason::ExpectedText("ok = true,"),
        );
    }

    #[test]
    fn a_crlf_line_end_fails() {
        // IGV Contracts, `Pagle_Following`: LF line ends
        let rejection = following_fails(FOLLOWING.replace('\n', "\r\n"), Reason::Expected("LF"));
        assert_eq!((rejection.line, rejection.offset), (1, 22));
        following_fails(
            FOLLOWING.replacen("count = 1,\n", "count = 1,\r\n", 1),
            Reason::Expected("LF"),
        );
        following_fails(ada_with(" },\n", " },\r\n"), Reason::Expected("LF"));
    }

    #[test]
    fn a_leading_bom_fails() {
        // IGV Contracts, `Pagle_Following`: the file starts with its header
        let rejection = following_fails(
            format!("\u{feff}{FOLLOWING}"),
            Reason::ExpectedText("PagleFollowingData = {"),
        );
        assert_eq!((rejection.line, rejection.offset), (1, 0));
    }

    #[test]
    fn trailing_bytes_or_a_missing_last_lf_fail() {
        // IGV Contracts, `Pagle_Following`: nothing after the closing `}` and its LF
        following_fails(format!("{FOLLOWING}\n"), Reason::TrailingBytes);
        following_fails(format!("{FOLLOWING} "), Reason::TrailingBytes);
        following_fails(format!("{FOLLOWING}x = 1\n"), Reason::TrailingBytes);
        following_fails(FOLLOWING.trim_end_matches('\n'), Reason::Expected("LF"));
        following_fails("", Reason::ExpectedText("PagleFollowingData = {"));
    }

    #[test]
    fn as_of_must_be_8_digits() {
        // IGV Contracts, `Pagle_Following`: `asOf` is 8DIGIT
        following_fails(
            ada_with("asOf = 20261012", "asOf = 2026101"),
            Reason::Expected("8 digits"),
        );
        following_fails(
            ada_with("asOf = 20261012", "asOf = 202610121"),
            Reason::ExpectedText(", guids = { "),
        );
        following_fails(
            ada_with("asOf = 20261012", "asOf = 2026-10-"),
            Reason::Expected("8 digits"),
        );
        // 8DIGIT, unlike nat, may start with a zero
        assert_eq!(
            check_following(ada_with("asOf = 20261012", "asOf = 00000000").as_bytes()),
            Ok(())
        );
    }

    #[test]
    fn a_nat_has_no_leading_zero_sign_or_fraction() {
        // IGV Contracts, Lua strings: nat = "0" / %x31-39 *DIGIT
        let rejection = following_fails(ada_with("level = 23", "level = 07"), Reason::LeadingZero);
        assert_eq!(rejection.line, 6);
        following_fails(ada_with("level = 23", "level = 00"), Reason::LeadingZero);
        following_fails(
            ada_with("level = 23", "level = -1"),
            Reason::Expected("a nat"),
        );
        following_fails(
            ada_with("level = 23", "level = 2.5"),
            Reason::ExpectedText(", prestige = "),
        );
        following_fails(
            ada_with("level = 23", "level = 0x1"),
            Reason::ExpectedText(", prestige = "),
        );
        following_fails(
            ada_with("level = 23", "level = "),
            Reason::Expected("a nat"),
        );
        following_fails(
            FOLLOWING.replace("generatedAt = 1791735100", "generatedAt = 01791735100"),
            Reason::LeadingZero,
        );
        following_fails(
            FOLLOWING.replace("count = 1", "count = 01"),
            Reason::LeadingZero,
        );
        assert_eq!(
            check_following(ada_with("level = 23", "level = 0").as_bytes()),
            Ok(())
        );
        // a nat has no upper bound in the grammar
        assert_eq!(
            check_following(
                ada_with("catches = 5120", "catches = 123456789012345678901234567890").as_bytes()
            ),
            Ok(())
        );
    }

    #[test]
    fn a_bool_is_true_or_false() {
        // IGV Contracts, Lua strings: bool = "true" / "false"
        assert_eq!(
            check_following(ada_with("hardcore = false", "hardcore = true").as_bytes()),
            Ok(())
        );
        following_fails(
            ada_with("hardcore = false", "hardcore = nil"),
            Reason::Expected("a bool"),
        );
        following_fails(
            ada_with("hardcore = false", "hardcore = 1"),
            Reason::Expected("a bool"),
        );
        following_fails(
            ada_with("hardcore = false", "hardcore = False"),
            Reason::Expected("a bool"),
        );
    }

    #[test]
    fn an_angler_lines_fields_come_in_the_grammars_order_and_spacing() {
        // IGV Contracts, `Pagle_Following`: angler-line, byte for byte
        following_fails(
            ada_with("more = 1, hardcore = false", "hardcore = false, more = 1"),
            Reason::ExpectedText(", more = "),
        );
        following_fails(
            ada_with("{ name", "{name"),
            Reason::ExpectedText("{ name = "),
        );
        following_fails(
            ada_with(", level = 23", ""),
            Reason::ExpectedText(", level = "),
        );
        following_fails(
            ada_with(" } },\n", " }, },\n"),
            Reason::ExpectedText(" } },"),
        );
        following_fails(ada_with(" } },\n", " } }\n"), Reason::ExpectedText(" } },"));
        following_fails(
            ada_with("name = \"Ada\"", "name = 'Ada'"),
            Reason::Expected("a Lua string"),
        );
        following_fails(
            ada_with("name = \"Ada\"", "name = [[Ada]]"),
            Reason::Expected("a Lua string"),
        );
        following_fails(
            ada_with("\", \"Player", "\",\"Player"),
            Reason::ExpectedText(" } },"),
        );
        following_fails(
            FOLLOWING.replace("anglers = {", "anglers = { "),
            Reason::Expected("LF"),
        );
    }

    #[test]
    fn guids_may_be_empty_or_many() {
        // IGV Contracts, `Pagle_Following`: guids = { [ lua-string *( ", " lua-string ) ] }
        let guids = "guids = { \"Player-4618-0A1B2C3D\", \"Player-4618-0A1B2C3E\" }";
        assert_eq!(
            check_following(ada_with(guids, "guids = {  }").as_bytes()),
            Ok(())
        );
        assert_eq!(
            check_following(ada_with(guids, "guids = { \"Player-4618-0A1B2C3D\" }").as_bytes()),
            Ok(())
        );
        following_fails(
            ada_with(guids, "guids = { }"),
            Reason::ExpectedText(" } },"),
        );
        following_fails(
            ada_with(guids, "guids = {}"),
            Reason::ExpectedText(", guids = { "),
        );
        following_fails(
            ada_with(guids, "guids = { \"a\", }"),
            Reason::Expected("a Lua string"),
        );
        following_fails(
            ada_with(guids, "guids = { 1 }"),
            Reason::ExpectedText(" } },"),
        );
    }

    #[test]
    fn an_unescaped_control_byte_or_del_in_a_string_fails() {
        // IGV Contracts, Lua strings: plain is every byte but controls, DEL, `"` and `\`
        following_fails(ada_with("\"Ada\"", "\"A\nda\""), Reason::ControlByte);
        following_fails(ada_with("\"Ada\"", "\"A\tda\""), Reason::ControlByte);
        following_fails(ada_with("\"Ada\"", "\"A\0da\""), Reason::ControlByte);
        following_fails(ada_with("\"Ada\"", "\"A\x1fda\""), Reason::ControlByte);
        following_fails(ada_with("\"Ada\"", "\"A\x7fda\""), Reason::ControlByte);
        // ...and a string runs to its closing quote
        following_fails(
            FOLLOWING.replace("\"Ada\", more", "\"Ada, more"),
            Reason::ExpectedText(", more = "),
        );
        let cut = FOLLOWING.find("Ada").unwrap() + 2;
        following_fails(&FOLLOWING.as_bytes()[..cut], Reason::UnterminatedString);
    }

    #[test]
    fn only_the_three_escapes_pass() {
        // IGV Contracts, Lua strings: escape = "\" ( DQUOTE / "\" / 3DIGIT )
        for escaped in [r#"\"Ada\""#, r"\\", r"\010", r"\000", r"\255", r"\0659"] {
            let body = ada_with("\"Ada\"", &format!("\"A{escaped}da\""));
            assert_eq!(check_following(body.as_bytes()), Ok(()), "{escaped}");
        }
        following_fails(ada_with("\"Ada\"", r#""A\x41da""#), Reason::BadEscape);
        following_fails(ada_with("\"Ada\"", r#""A\nda""#), Reason::BadEscape);
        following_fails(ada_with("\"Ada\"", r#""A\'da""#), Reason::BadEscape);
        following_fails(ada_with("\"Ada\"", "\"A\\\nda\""), Reason::BadEscape);
        following_fails(ada_with("\"Ada\"", r#""A\65da""#), Reason::BadEscape);
        following_fails(ada_with("\"Ada\"", r#""A\6""#), Reason::BadEscape);
        // Lua 5.1 refuses a \ddd escape above 255, so the file wouldn't load
        following_fails(ada_with("\"Ada\"", r#""A\256da""#), Reason::EscapeTooLarge);
        following_fails(ada_with("\"Ada\"", r#""A\999da""#), Reason::EscapeTooLarge);
    }

    #[test]
    fn bytes_from_0x80_up_pass_unescaped() {
        // IGV Contracts, Lua strings: plain includes %x80-FF
        let mut body = ada_with("\"Ada\"", "\"A__da\"").into_bytes();
        let at = body.windows(2).position(|w| w == b"__").unwrap();
        body[at] = 0xc3;
        body[at + 1] = 0xa9;
        assert_eq!(check_following(&body), Ok(()));
        body[at] = 0xff;
        body[at + 1] = 0x80;
        assert_eq!(check_following(&body), Ok(()));
    }

    #[test]
    fn at_most_the_followed_cap_of_anglers_passes() {
        // IGV-R41, CMP-R82: at most the followed cap (200) of Anglers
        assert_eq!(FOLLOWED_CAP, 200);
        let full = following(200, &ADA.repeat(200));
        assert_eq!(check_following(full.as_bytes()), Ok(()));
        let rejection = following_fails(following(201, &ADA.repeat(201)), Reason::FollowedCap);
        assert_eq!(rejection.line, 6 + 200);
    }

    #[test]
    fn the_following_order_by_name_isnt_checked() {
        // CMP-R82: orderings the file alone can't prove aren't checked
        let zed = ADA.replace("\"Ada\"", "\"Zed\"");
        let lines = format!("{zed}{ADA}{ADA}");
        assert_eq!(check_following(following(3, &lines).as_bytes()), Ok(()));
    }

    #[test]
    fn an_anglers_body_isnt_a_following_body() {
        // IGV Contracts: each file has its own header
        following_fails(ANGLERS, Reason::ExpectedText("PagleFollowingData = {"));
        anglers_fails(FOLLOWING, Reason::ExpectedText("PagleAnglersData = {"));
    }

    // --- Pagle_Anglers ---

    #[test]
    fn the_anglers_example_passes() {
        // IGV Contracts, `Pagle_Anglers`: the example is a valid file
        assert_eq!(check_anglers(ANGLERS.as_bytes()), Ok(()));
        assert_eq!(check(DataFile::Anglers, ANGLERS.as_bytes()), Ok(()));
        assert_eq!(anglers(1, EU), ANGLERS);
    }

    #[test]
    fn an_anglers_file_with_no_blocks_or_empty_blocks_passes() {
        // IGV-R4: a Player with no region gets a file with no blocks
        assert_eq!(check_anglers(anglers(0, "").as_bytes()), Ok(()));
        // ...and a region with no Angler shown in game is an empty block
        let empty = block(0, 0, "", "", "");
        assert!(empty.contains("titles = {  }, cards = \"\", keys = \"\" },"));
        assert_eq!(check_anglers(anglers(1, &empty).as_bytes()), Ok(()));
    }

    #[test]
    fn several_blocks_pass_and_count_is_their_number() {
        // IGV-R41, IGV-R44: `count` is the number of blocks
        let us = EU.replace("\"eu\"", "\"us\"");
        assert_eq!(
            check_anglers(anglers(2, &format!("{EU}{us}")).as_bytes()),
            Ok(())
        );
        anglers_fails(anglers(1, &format!("{EU}{us}")), Reason::CountMismatch);
        anglers_fails(anglers(2, EU), Reason::CountMismatch);
        anglers_fails(anglers(1, ""), Reason::CountMismatch);
        anglers_fails(
            ANGLERS.replace("v = 1,", "v = 2,"),
            Reason::ExpectedText("v = 1,"),
        );
        anglers_fails(
            ANGLERS.replace("ok = true,", "ok = false,"),
            Reason::ExpectedText("ok = true,"),
        );
    }

    #[test]
    fn cards_must_be_16_times_card_count_characters() {
        // IGV-R41, IGV-R44: cards is 16 × cardCount characters
        anglers_fails(
            eu_with("0016001210007121\"", "001600121000712\""),
            Reason::CardsLength,
        );
        anglers_fails(
            eu_with("0016001210007121\"", "00160012100071210\""),
            Reason::CardsLength,
        );
        anglers_fails(
            eu_with("cardCount = 2", "cardCount = 3"),
            Reason::CardsLength,
        );
        anglers_fails(
            eu_with("cardCount = 2", "cardCount = 99999999999999999999999"),
            Reason::CardsLength,
        );
    }

    #[test]
    fn keys_must_be_19_times_key_count_characters() {
        // IGV-R41, IGV-R44: keys is 19 × keyCount characters
        anglers_fails(eu_with("keyCount = 3", "keyCount = 2"), Reason::KeysLength);
        anglers_fails(eu_with("3E000001\"", "3E00001\""), Reason::KeysLength);
    }

    #[test]
    fn a_card_is_digits_with_a_hardcore_of_0_or_1() {
        // IGV Contracts, `Pagle_Anglers`: card = 15 digits then "0" / "1"
        anglers_fails(
            eu_with("0016001210007121\"", "0016001210007122\""),
            Reason::BadCard,
        );
        anglers_fails(
            eu_with("0016001210007121\"", "001600121000712A\""),
            Reason::BadCard,
        );
        anglers_fails(
            eu_with("0016001210007121\"", "00160012100 7121\""),
            Reason::BadCard,
        );
        anglers_fails(
            eu_with("0016001210007121\"", "001600121000712\\\""),
            Reason::BadCard,
        );
    }

    #[test]
    fn a_title_index_names_one_of_the_titles_or_none() {
        // IGV Contracts, `Pagle_Anglers`: title-index 000 = none, else 1-based into titles
        assert_eq!(
            check_anglers(eu_with("\"0022300103703120", "\"0002300103703120").as_bytes()),
            Ok(())
        );
        anglers_fails(
            eu_with("\"0022300103703120", "\"0032300103703120"),
            Reason::TitleIndex,
        );
    }

    #[test]
    fn keys_out_of_ascending_order_fail() {
        // IGV-R41: keys ascending in byte order
        anglers_fails(
            eu_with(
                "0439501234567000002046180A1B2C3D000001",
                "046180A1B2C3D0000010439501234567000002",
            ),
            Reason::KeysOrder,
        );
        anglers_fails(
            eu_with(
                "046180A1B2C3D000001046180A1B2C3E000001",
                "046180A1B2C3E000001046180A1B2C3D000001",
            ),
            Reason::KeysOrder,
        );
    }

    #[test]
    fn two_keys_with_one_search_key_fail() {
        // IGV-R41: no two keys with the same search key (first 13 characters)
        let rejection = anglers_fails(
            eu_with("046180A1B2C3E000001", "046180A1B2C3D000002"),
            Reason::DuplicateSearchKey,
        );
        assert_eq!(rejection.line, 6);
        anglers_fails(
            eu_with("046180A1B2C3E000001", "046180A1B2C3D000001"),
            Reason::DuplicateSearchKey,
        );
    }

    #[test]
    fn a_key_is_a_realm_an_upper_case_hex_uid_and_a_card_number() {
        // IGV Contracts, `Pagle_Anglers`: key = 5DIGIT 8HEXUP 6DIGIT
        anglers_fails(eu_with("046180A1B2C3E", "046180A1B2C3e"), Reason::BadKey);
        anglers_fails(eu_with("046180A1B2C3E", "046180A1B2C3G"), Reason::BadKey);
        anglers_fails(eu_with("046180A1B2C3E", "0461A0A1B2C3E"), Reason::BadKey);
        anglers_fails(eu_with("3E000001\"", "3E00000A\""), Reason::BadKey);
        anglers_fails(eu_with("3E000001\"", "3E00000\\\""), Reason::BadKey);
    }

    #[test]
    fn a_keys_card_number_names_one_of_the_cards() {
        // IGV Contracts, `Pagle_Anglers`: card-number is 1-based, card 1 first
        anglers_fails(eu_with("3E000001\"", "3E000000\""), Reason::CardNumber);
        anglers_fails(eu_with("3E000001\"", "3E000003\""), Reason::CardNumber);
    }

    #[test]
    fn titles_out_of_order_or_repeated_fail() {
        // IGV Contracts, `Pagle_Anglers`: titles are distinct and in ascending byte order
        anglers_fails(
            eu_with(
                "\"Seeker of the Deep\", \"Warden of Feralas Waters\"",
                "\"Warden of Feralas Waters\", \"Seeker of the Deep\"",
            ),
            Reason::TitlesOrder,
        );
        anglers_fails(
            eu_with(
                "\"Seeker of the Deep\", \"Warden of Feralas Waters\"",
                "\"Seeker of the Deep\", \"Seeker of the Deep\"",
            ),
            Reason::DuplicateTitle,
        );
        // byte order is of the text, not its escaped form: "\010" (LF) sorts first
        assert_eq!(
            check_anglers(eu_with("\"Seeker of the Deep\"", "\"\\010 Seeker\"").as_bytes()),
            Ok(())
        );
        assert_eq!(
            check_anglers(eu_with("\"Seeker of the Deep\"", "\"\\065\"").as_bytes()),
            Ok(())
        );
        anglers_fails(
            eu_with("\"Warden of Feralas Waters\"", "\"\\065\""),
            Reason::TitlesOrder,
        );
        anglers_fails(
            eu_with(
                "\"Seeker of the Deep\", \"Warden of Feralas Waters\"",
                "\"A\", \"\\065\"",
            ),
            Reason::DuplicateTitle,
        );
    }

    #[test]
    fn a_blocks_fields_come_in_the_grammars_order_and_spacing() {
        // IGV Contracts, `Pagle_Anglers`: block, byte for byte
        anglers_fails(
            eu_with("titles = { ", "titles = {"),
            Reason::ExpectedText(", titles = { "),
        );
        anglers_fails(
            eu_with("\" }, cards", "\"}, cards"),
            Reason::ExpectedText(" }, cards = "),
        );
        anglers_fails(
            eu_with("cards = \"", "cards = '"),
            Reason::ExpectedText("\""),
        );
        anglers_fails(
            eu_with("keyCount = 3", "keyCount = 03"),
            Reason::LeadingZero,
        );
        anglers_fails(
            eu_with("region = \"eu\"", "region = eu"),
            Reason::Expected("a Lua string"),
        );
        anglers_fails(eu_with("\" },\n", "\"},\n"), Reason::ExpectedText(" },"));
        anglers_fails(eu_with(" },\n", " },\r\n"), Reason::Expected("LF"));
        anglers_fails(
            format!("\u{feff}{ANGLERS}"),
            Reason::ExpectedText("PagleAnglersData = {"),
        );
        anglers_fails(format!("{ANGLERS}\n"), Reason::TrailingBytes);
    }

    #[test]
    fn the_anglers_cap_is_16_mib_inclusive_and_checked_quickly() {
        // IGV-R41, CMP-R82: at most the Anglers cap (16,777,216 bytes), and a
        // full file is checked in under a second, holding nothing but the
        // body and the previous title
        assert_eq!(ANGLERS_CAP, 16_777_216);
        let full = anglers_of_size(ANGLERS_CAP);
        let started = Instant::now();
        assert_eq!(check_anglers(&full), Ok(()));
        let took = started.elapsed();
        assert!(took < Duration::from_secs(1), "took {took:?}");

        let over = anglers_of_size(ANGLERS_CAP + 1);
        let rejection = anglers_fails(&over, Reason::AnglersCap);
        assert_eq!(rejection.offset, ANGLERS_CAP);
    }

    // --- Rejections ---

    #[test]
    fn a_rejection_names_its_position_and_rule_and_no_content() {
        // CMP-R112, CMP-R82: the failure goes to the local log, which never
        // holds names or GUIDs
        let rejection = following_fails(ada_with("\"Ada\"", r#""Ada\x41""#), Reason::BadEscape);
        let shown = rejection.to_string();
        assert_eq!(
            shown,
            "Pagle_Following body rejected at line 6, byte 92: \
             a string has an escape other than \\\", \\\\ or \\ddd (IGV Contracts, Lua strings)"
        );
        assert!(!shown.contains("Ada"));
        assert!(!shown.contains("Player-"));

        let rejection = anglers_fails(
            eu_with("046180A1B2C3E000001", "046180A1B2C3D000002"),
            Reason::DuplicateSearchKey,
        );
        let shown = rejection.to_string();
        assert!(
            shown.starts_with("Pagle_Anglers body rejected at line 6, byte "),
            "{shown}"
        );
        assert!(shown.ends_with("(IGV-R41)"), "{shown}");
        assert!(!shown.contains("0A1B2C3D"), "{shown}");
    }

    #[test]
    fn every_reason_names_a_rule() {
        // CMP-R82: the log line says why, by rule
        let reasons = [
            Reason::AnglersCap,
            Reason::FollowedCap,
            Reason::ExpectedText("v = 1,"),
            Reason::Expected("LF"),
            Reason::LeadingZero,
            Reason::UnterminatedString,
            Reason::ControlByte,
            Reason::BadEscape,
            Reason::EscapeTooLarge,
            Reason::TrailingBytes,
            Reason::CountMismatch,
            Reason::CardsLength,
            Reason::KeysLength,
            Reason::BadCard,
            Reason::BadKey,
            Reason::TitleIndex,
            Reason::CardNumber,
            Reason::KeysOrder,
            Reason::DuplicateSearchKey,
            Reason::TitlesOrder,
            Reason::DuplicateTitle,
        ];
        for reason in reasons {
            assert!(!reason.to_string().is_empty());
            assert!(reason.rule().starts_with("IGV"), "{reason:?}");
        }
    }
}

//! The check of `Pagle_Content` (CMP-R82, HEV-R51): the shared test vector,
//! then one change per rule of HEV Contracts, `Pagle_Content`, to a good
//! file built here as the contract lays it out.

use super::*;
use crate::in_game::content_vector::{
    self, BAD, EXPIRES, GENERATED_AT, GOOD_PAYLOAD, KEY_ID, SEQ, content_file, content_file_with,
    signed,
};

/// The vector's `good` file, as the backend serves it.
fn good() -> String {
    content_file("good")
}

/// The good file with one piece of text replaced.
fn good_with(from: &str, to: &str) -> String {
    let good = good();
    assert!(good.contains(from), "{from:?} not in the good file");
    good.replacen(from, to, 1)
}

/// A file with the vector's header and `good`'s signature, carrying
/// `payload` (whose signature this isn't: the Companion never checks it).
fn with_payload(payload: &[u8]) -> String {
    content_file_with(
        &GENERATED_AT.to_string(),
        KEY_ID,
        &SEQ.to_string(),
        &EXPIRES.to_string(),
        &STANDARD.encode(payload),
        &signed("good").sig,
    )
}

/// A file with the given header fields and `good`'s payload and signature.
fn with_header(generated_at: &str, key_id: &str, seq: &str, expires: &str) -> String {
    let good = signed("good");
    content_file_with(generated_at, key_id, seq, expires, &good.payload, &good.sig)
}

fn passes(body: impl AsRef<[u8]>) {
    let body = body.as_ref();
    assert_eq!(check_content(body), Ok(()));
    assert_eq!(check(DataFile::Content, body), Ok(()));
}

fn fails(body: impl AsRef<[u8]>, reason: Reason) -> Rejection {
    let rejection = check_content(body.as_ref()).expect_err("should be rejected");
    assert_eq!(rejection.reason, reason, "{rejection}");
    assert_eq!(rejection.file, DataFile::Content);
    rejection
}

/// Fails on the payload grammar, whatever Core's reason.
fn fails_on_payload(body: impl AsRef<[u8]>) -> Rejection {
    let rejection = check_content(body.as_ref()).expect_err("should be rejected");
    assert!(
        matches!(rejection.reason, Reason::PayloadGrammar { .. }),
        "{rejection}"
    );
    rejection
}

// --- the shared test vector (HEV Contracts, Test vector)

#[test]
fn the_vectors_good_and_big_files_pass() {
    // HEV Contracts, Test vector: the Companion's checker accepts good and
    // big (exactly the payload cap)
    passes(good());
    passes(content_file("big"));
    assert_eq!(
        STANDARD.decode(signed("big").payload).unwrap().len(),
        PAYLOAD_CAP
    );
}

#[test]
fn the_vectors_payloads_outside_the_grammar_fail() {
    // HEV Contracts, Test vector: it refuses the two payloads outside the
    // grammar, though they are signed
    for name in ["functionCall", "singleQuote"] {
        let rejection = fails_on_payload(content_file(name));
        assert_eq!(rejection.line, 7, "{name}: {rejection}");
    }
}

#[test]
fn every_bad_file_of_the_vector_fails() {
    // HEV Contracts, Test vector: and the bodies under bad/
    for (name, body) in BAD {
        let rejection = check_content(body).expect_err(name);
        let want = match name {
            "over-cap.lua" => Reason::PayloadCap,
            "exponent.lua" => Reason::ExpectedText(","),
            "leading-zero.lua" => Reason::LeadingZero,
            "unknown-field.lua" => Reason::ExpectedText("}"),
            "base64-line-break.lua" => Reason::BadBase64,
            "crlf.lua" => Reason::Expected("LF"),
            _ => unreachable!("{name}"),
        };
        assert_eq!(rejection.reason, want, "{name}: {rejection}");
    }
}

// --- the file's lines, byte for byte (HEV Contracts, `Pagle_Content`)

#[test]
fn the_good_file_is_the_nine_lines_of_the_contract() {
    // HEV Contracts, `Pagle_Content`: nine LF-ended lines, fields indented
    // with one tab
    let good = good();
    let lines: Vec<&str> = good.split_terminator('\n').collect();
    assert_eq!(lines.len(), 9);
    assert_eq!(lines[0], "PagleContentData = {");
    assert_eq!(lines[1], "\tv = 1,");
    assert_eq!(lines[2], format!("\tgeneratedAt = {GENERATED_AT},"));
    assert_eq!(lines[3], format!("\tkeyId = \"{KEY_ID}\","));
    assert_eq!(lines[8], "}");
}

#[test]
fn a_version_other_than_1_fails() {
    // HEV Contracts, `Pagle_Content`: v = 1 (PUSH-R13: an unknown v is
    // absent)
    let rejection = fails(
        good_with("v = 1,", "v = 2,"),
        Reason::ExpectedText("v = 1,"),
    );
    assert_eq!(rejection.line, 2);
    fails(
        good_with("v = 1,", "v = 10,"),
        Reason::ExpectedText("v = 1,"),
    );
    fails(good_with("\tv = 1,\n", ""), Reason::ExpectedText("v = 1,"));
}

#[test]
fn field_lines_are_indented_with_one_tab() {
    // HEV Contracts, `Pagle_Content`: HTAB, never spaces, nothing more
    fails(
        good_with("\tv = 1,", "    v = 1,"),
        Reason::Expected("a tab"),
    );
    fails(
        good_with("\tseq = ", "\t\tseq = "),
        Reason::ExpectedText("seq = "),
    );
    fails(good_with("\tsig = ", "sig = "), Reason::Expected("a tab"));
    fails(good_with("\n}\n", "\n\t}\n"), Reason::ExpectedText("}"));
}

#[test]
fn fields_come_in_the_contracts_order_and_spacing() {
    // HEV Contracts, `Pagle_Content`: the eight fixed lines in order, with
    // no other whitespace
    let good = good();
    let lines: Vec<&str> = good.split_inclusive('\n').collect();
    let mut swapped = lines.clone();
    swapped.swap(4, 5);
    fails(swapped.concat(), Reason::ExpectedText("seq = "));
    fails(
        good_with("generatedAt = ", "generatedAt= "),
        Reason::ExpectedText("generatedAt = "),
    );
    fails(
        good_with("seq = 42,", "seq = 42 ,"),
        Reason::ExpectedText(","),
    );
    fails(
        good_with("seq = 42,", "seq = 42"),
        Reason::ExpectedText(","),
    );
    fails(
        good_with("seq = 42,", "seq = 42,  "),
        Reason::Expected("LF"),
    );
    fails(
        good_with("keyId = \"", "keyId = '"),
        Reason::ExpectedText("keyId = \""),
    );
    fails(
        good_with("PagleContentData = {", "PagleContentData={"),
        Reason::ExpectedText("PagleContentData = {"),
    );
    fails(
        good_with("PagleContentData", "PagleFollowingData"),
        Reason::ExpectedText("PagleContentData = {"),
    );
}

#[test]
fn an_unknown_or_missing_field_fails() {
    // HEV Contracts, `Pagle_Content`: exactly these fields (the backend's
    // bad/unknown-field.lua puts one last)
    fails(
        good_with("\tseq = ", "\textra = 1,\n\tseq = "),
        Reason::ExpectedText("seq = "),
    );
    let good = good();
    let without_seq: String = good
        .split_inclusive('\n')
        .filter(|l| !l.starts_with("\tseq"))
        .collect();
    fails(without_seq, Reason::ExpectedText("seq = "));
}

#[test]
fn crlf_line_ends_a_bom_trailing_bytes_or_no_last_lf_fail() {
    // HEV Contracts, `Pagle_Content`: UTF-8 without a BOM, LF line ends,
    // nothing after the last line
    let rejection = fails(good().replace('\n', "\r\n"), Reason::Expected("LF"));
    assert_eq!((rejection.line, rejection.offset), (1, 20));
    fails(
        good_with("seq = 42,\n", "seq = 42,\r\n"),
        Reason::Expected("LF"),
    );
    fails(
        format!("\u{feff}{}", good()),
        Reason::ExpectedText("PagleContentData = {"),
    );
    fails(format!("{}\n", good()), Reason::TrailingBytes);
    fails(format!("{} ", good()), Reason::TrailingBytes);
    fails(good().trim_end_matches('\n'), Reason::Expected("LF"));
    fails("", Reason::ExpectedText("PagleContentData = {"));
}

// --- int (HEV Contracts, `Pagle_Content`)

#[test]
fn an_int_is_decimal_with_no_sign_or_leading_zero() {
    // int = "0" / (%x31-39 0*14DIGIT)
    fails(with_header("01", KEY_ID, "42", "2"), Reason::LeadingZero);
    fails(good_with("seq = 42,", "seq = 042,"), Reason::LeadingZero);
    fails(good_with("seq = 42,", "seq = 00,"), Reason::LeadingZero);
    fails(
        good_with("seq = 42,", "seq = -1,"),
        Reason::Expected("an int"),
    );
    fails(
        good_with("seq = 42,", "seq = +1,"),
        Reason::Expected("an int"),
    );
    fails(
        good_with("seq = 42,", "seq = ,"),
        Reason::Expected("an int"),
    );
    fails(
        good_with("seq = 42,", "seq = 42e0,"),
        Reason::ExpectedText(","),
    );
    fails(
        good_with("seq = 42,", "seq = 4.2,"),
        Reason::ExpectedText(","),
    );
    fails(
        good_with("seq = 42,", "seq = 0x2A,"),
        Reason::ExpectedText(","),
    );
    fails(
        good_with("seq = 42,", "seq = 42.0,"),
        Reason::ExpectedText(","),
    );
    passes(good_with("seq = 42,", "seq = 0,"));
    passes(with_header("0", KEY_ID, "0", "1"));
}

#[test]
fn an_int_has_at_most_15_digits() {
    // int: at most 15 digits, so every int is at most 2^53
    let largest = "999999999999999";
    assert!(largest.parse::<u64>().unwrap() < 1 << 53);
    passes(good_with("seq = 42,", &format!("seq = {largest},")));
    passes(with_header("1", KEY_ID, "1", largest));
    let rejection = fails(
        good_with("seq = 42,", "seq = 1000000000000000,"),
        Reason::IntTooLong,
    );
    assert_eq!(rejection.line, 5);
    fails(
        with_header("1", KEY_ID, "1", "1000000000000000"),
        Reason::IntTooLong,
    );
}

#[test]
fn expires_must_be_after_generated_at() {
    // HEV Contracts, `Pagle_Content`: expires > generatedAt (HEV-R33)
    let rejection = fails(
        with_header("100", KEY_ID, "42", "100"),
        Reason::ExpiresNotAfterGenerated,
    );
    assert_eq!(rejection.line, 6);
    fails(
        with_header("100", KEY_ID, "42", "99"),
        Reason::ExpiresNotAfterGenerated,
    );
    fails(
        with_header("1", KEY_ID, "42", "0"),
        Reason::ExpiresNotAfterGenerated,
    );
    passes(with_header("100", KEY_ID, "42", "101"));
    // Compared as numbers, not text.
    passes(with_header("9", KEY_ID, "42", "10"));
}

// --- keyid (HEV Contracts, `Pagle_Content`; HEV-R40)

#[test]
fn a_key_id_is_1_to_32_of_letters_digits_underscore_and_hyphen() {
    // keyid = 1*32(ALPHA / DIGIT / "_" / "-"), never "|" (PUSH-R11)
    for key_id in ["k", "testKey", "Key_2026-10", &"a".repeat(32), "0-_Z"] {
        passes(with_header("1", key_id, "42", "2"));
    }
    for key_id in [
        "",
        "test|Key",
        "test Key",
        "test.Key",
        "t\u{e9}st",
        "test\\Key",
    ] {
        let rejection = fails(with_header("1", key_id, "42", "2"), Reason::BadKeyId);
        assert_eq!(rejection.line, 4, "{key_id:?}");
    }
    let long = with_header("1", &"a".repeat(33), "42", "2");
    let rejection = fails(&long, Reason::BadKeyId);
    assert_eq!(rejection.offset, long.find("\"aaa").unwrap() + 1 + 32);
    // A quote inside it can't be escaped.
    fails(with_header("1", "test\\\"Key", "42", "2"), Reason::BadKeyId);
}

// --- b64 (HEV Contracts, `Pagle_Content`; RFC 4648 section 4)

#[test]
fn base64_is_the_standard_alphabet_padded_on_one_line() {
    // b64 = *(4b64c) [2b64c "==" / 3b64c "="], b64c = ALPHA / DIGIT / "+" / "/"
    // "ab" is "YWI=", "a" is "YQ==": padding is required, and exact.
    assert_eq!(STANDARD.encode(b"ab"), "YWI=");
    assert_eq!(
        check_content(with_payload_text("YWI=").as_bytes()).map_err(|r| r.reason),
        Err(Reason::PayloadGrammar {
            at: 0,
            what: "expected {"
        })
    );
    for bad in [
        "YWI", "YWI==", "YWI=====", "YQ", "YQ=", "YQ===", "Y", "Y===", "YWJj=",
    ] {
        fails(with_payload_text(bad), Reason::BadBase64);
    }
    let payload = signed("good").payload;
    let rejection = fails(
        good_with(&payload, &format!("{}\n{}", &payload[..76], &payload[76..])),
        Reason::BadBase64,
    );
    assert_eq!(rejection.line, 7);
    fails(
        good_with(
            &payload,
            &format!("{}\r\n{}", &payload[..76], &payload[76..]),
        ),
        Reason::BadBase64,
    );
    fails(
        good_with(&payload, &format!("{} {}", &payload[..76], &payload[76..])),
        Reason::BadBase64,
    );
    fails(
        good_with(&payload, &format!("{}={}", &payload[..76], &payload[76..])),
        Reason::BadBase64,
    );
    // The URL-safe alphabet isn't the standard one.
    let sig = signed("good").sig;
    assert!(sig.contains('+') || sig.contains('/'));
    fails(
        good_with(&sig, &sig.replace('+', "-").replace('/', "_")),
        Reason::BadBase64,
    );
    // Padding with the wrong number of characters before it.
    fails(with_payload_text("QUJD="), Reason::BadBase64);
    fails(with_payload_text("QUJDRA="), Reason::BadBase64);
    fails(with_payload_text("Q==="), Reason::BadBase64);
}

/// A file whose payload is the Base64 text `text`, as given.
fn with_payload_text(text: &str) -> String {
    good_with(&signed("good").payload, text)
}

#[test]
fn base64_padding_bits_must_be_zero() {
    // b64: zero padding bits (RFC 4648 section 3.5)
    // "QQ==" is "A"; "QR==" has a padding bit set.
    fails(with_payload_text("QR=="), Reason::Base64PaddingBits);
    // "QUI=" is "AB"; "QUJ=" has padding bits set.
    fails(with_payload_text("QUJ="), Reason::Base64PaddingBits);
    // 256 bytes end in one byte: two characters and "==", the second
    // carrying four padding bits; "B" sets one.
    let sig = signed("good").sig;
    assert!(sig.ends_with("=="));
    let flipped = format!("{}B==", &sig[..sig.len() - 3]);
    let rejection = fails(good_with(&sig, &flipped), Reason::Base64PaddingBits);
    assert_eq!(rejection.line, 8);
}

#[test]
fn sig_decodes_to_exactly_256_bytes() {
    // HEV Contracts, `Pagle_Content`: sig decodes to exactly 256 bytes
    // (HEV-R35)
    let sig = signed("good").sig;
    for len in [0, 1, 255, 257, 512] {
        let rejection = fails(
            good_with(&sig, &STANDARD.encode(vec![7u8; len])),
            Reason::SigLength,
        );
        assert_eq!(rejection.line, 8, "{len}");
    }
    passes(good_with(&sig, &STANDARD.encode([7u8; 256])));
}

// --- the payload (HEV-R51; PUSH Contracts, The payload)

#[test]
fn the_payload_is_at_most_the_payload_cap() {
    // HEV-R51, PUSH Tunables: at most 64 KiB (65,536 bytes) decoded
    assert_eq!(PAYLOAD_CAP, 65_536);
    let big = STANDARD.decode(signed("big").payload).unwrap();
    passes(with_payload(&big));
    // One byte more, still in the grammar: the last run's ID one longer.
    let over = String::from_utf8(big)
        .unwrap()
        .replacen("p_test_event_", "p_test_event__", 1);
    assert_eq!(over.len(), PAYLOAD_CAP + 1);
    assert_eq!(super::payload::check(over.as_bytes()), Ok(()));
    let rejection = fails(with_payload(over.as_bytes()), Reason::PayloadCap);
    assert_eq!(rejection.line, 7);
}

#[test]
fn the_whole_file_is_at_most_the_content_file_cap() {
    // HEV Tunables: the content file cap, 88 KiB (90,112 bytes), checked
    // before anything else
    assert_eq!(CONTENT_CAP, 90_112);
    let big = content_file("big");
    assert!(big.len() <= CONTENT_CAP);
    let mut over = big.into_bytes();
    over.resize(CONTENT_CAP + 1, b'\n');
    let rejection = fails(&over, Reason::ContentCap);
    assert_eq!(rejection.offset, CONTENT_CAP);
    over.truncate(CONTENT_CAP);
    fails(&over, Reason::TrailingBytes);
}

#[test]
fn the_payload_is_valid_utf8() {
    // HEV Contracts, `Pagle_Content`: the payload decodes to valid UTF-8
    let mut payload = GOOD_PAYLOAD.to_vec();
    let at = payload.windows(4).position(|w| w == b"Tide").unwrap();
    payload[at] = 0xff;
    let rejection = fails(with_payload(&payload), Reason::PayloadUtf8 { at });
    assert_eq!(rejection.line, 7);
    // UTF-8 itself passes.
    let mut payload = GOOD_PAYLOAD.to_vec();
    payload.splice(at..at + 1, "\u{c9}".bytes());
    passes(with_payload(&payload));
}

#[test]
fn the_payload_must_match_the_payload_grammar() {
    // HEV-R51: the decoded payload matches PUSH's payload grammar as Core's
    // reader takes it, its two extra forms included
    passes(with_payload(b"{items={\n},}\n"));
    passes(with_payload(
        br#"{items={{kind="huntRun",v=1,event="x",start=1,["end"]=2}},}"#,
    ));
    let rejection = fails_on_payload(with_payload(b"return {items={},}"));
    assert_eq!(rejection.line, 7);
    assert_eq!(
        rejection.reason,
        Reason::PayloadGrammar {
            at: 0,
            what: "expected {"
        }
    );
    fails_on_payload(with_payload(b""));
    fails_on_payload(with_payload(b"{items={},}--"));
    fails_on_payload(with_payload(
        br#"{items={{kind="huntRun",v=1,x=os.time()},},}"#,
    ));
}

#[test]
fn the_signature_isnt_checked() {
    // HEV-R51: the Companion doesn't check the signature; only the addon
    // holds keys (PUSH-R12)
    passes(with_payload(br#"{items={{kind="notice",v=1,},},}"#));
    passes(with_header("1", "unknownKey", "1", "2"));
}

// --- rejections

#[test]
fn a_rejection_names_its_position_and_rule_and_no_content() {
    // CMP-R112, CMP-R82: the failure goes to the local log file, never what
    // the body holds
    let payload = br#"{items={{kind="huntRun",v=1,event="secret\q"},},}"#;
    let rejection = fails_on_payload(with_payload(payload));
    let shown = rejection.to_string();
    let at = payload.windows(2).position(|w| w == br"\q").unwrap();
    assert_eq!(
        shown,
        format!(
            "Pagle_Content body rejected at line 7, byte {}: the decoded payload breaks its \
             grammar at its byte {at}: a string has an escape other than \\\\, \\\" or \\n \
             (HEV-R51, PUSH-R20)",
            good().find("payload = \"").unwrap() + "payload = \"".len()
        )
    );
    assert!(!shown.contains("secret"));

    let shown = fails(with_header("1", "secret|key", "1", "2"), Reason::BadKeyId).to_string();
    assert!(shown.ends_with("(HEV Contracts, Pagle_Content)"), "{shown}");
    assert!(!shown.contains("secret"), "{shown}");
}

#[test]
fn every_content_reason_names_a_rule() {
    // CMP-R82: the log line says why, by rule
    let reasons = [
        Reason::ContentCap,
        Reason::IntTooLong,
        Reason::BadKeyId,
        Reason::BadBase64,
        Reason::Base64PaddingBits,
        Reason::ExpiresNotAfterGenerated,
        Reason::SigLength,
        Reason::PayloadCap,
        Reason::PayloadUtf8 { at: 3 },
        Reason::PayloadGrammar {
            at: 3,
            what: "expected {",
        },
    ];
    for reason in reasons {
        assert!(!reason.to_string().is_empty());
        assert!(reason.rule().starts_with("HEV"), "{reason:?}");
    }
}

#[test]
fn the_vector_module_is_the_one_tested_here() {
    // The good file here is the vector's, laid out as the backend lays it out.
    assert_eq!(good(), content_vector::content_file("good"));
}

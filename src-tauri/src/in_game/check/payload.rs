//! The payload grammar of `Pagle_Content` (HEV-R51; wow-fish
//! `core/pushed-content.md` › The payload, PUSH-R20, PUSH-R21): [`check`]
//! accepts exactly the decoded payloads Core's reader takes.
//!
//! It is a port of Core's reader, `addons/Pagle/logic/PushedPayload.lua`
//! (`ns.ParsePayload`, wow-fish #293), step for step, so the Companion never
//! writes a file Core would read as absent for its payload (PUSH-R13), and
//! never refuses one Core reads. Like Core it takes, besides the grammar's
//! letter, a field name written `["name"]` (`huntRun`'s `["end"]`) and a
//! table's last element without its `,`; whitespace (space, tab, CR, LF)
//! between tokens; integers with a sign or leading zeros, of at most 15
//! digits; and tables nested at most 16 deep, the payload's own table
//! counting 1. It keeps nothing of what it reads but the first two fields of
//! each item and the field names of the table it is in, to refuse a name
//! given twice, as Core does.
//!
//! An [`Error`] says where (a byte offset in the decoded payload) and why,
//! never what the payload holds.

use std::collections::HashSet;

/// How deep tables may nest, the payload's own table counting 1 (PUSH
/// Tunables, `ns.PAYLOAD_DEPTH_LIMIT`).
pub const DEPTH_LIMIT: usize = 16;

/// The most digits an integer has (PUSH Contracts, The payload: exact in a
/// double).
pub const DIGITS: usize = 15;

/// Where and why a payload breaks the grammar: `at` is the 0-based byte in
/// the decoded payload, `what` Core's reason in words (its `PAYLOAD_*`
/// key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    pub at: usize,
    pub what: &'static str,
}

/// Checks a decoded payload (PUSH-R20): `{ items = { item, … }, }`, each
/// item `{ kind = "…", v = n, fields… }` with `kind` `[%w_]+` and `v` an
/// integer, those two fields first and in that order.
pub fn check(text: &[u8]) -> Result<(), Error> {
    let mut p = Parser { text, at: 0 };
    p.skip();
    p.expect(b'{', "expected {")?;
    p.skip();
    let key_at = p.at;
    if p.key()? != b"items" {
        return p.fail_at(key_at, "expected items");
    }
    p.skip();
    p.expect(b'=', "expected =")?;
    p.skip();
    p.expect(b'{', "expected {")?;
    p.skip();
    let mut closed = p.peek() == Some(b'}');
    if closed {
        p.at += 1;
    }
    while !closed {
        let item_at = p.at;
        if p.peek() != Some(b'{') {
            return p.fail_at(item_at, "expected an item");
        }
        let (first, second) = p.table(3)?;
        let kind_ok = first.is_some_and(|(key, value)| {
            key == b"kind"
                && matches!(value, Value::Str(kind) if !kind.is_empty()
                    && kind.iter().all(|&b| is_word(b)))
        });
        let v_ok = second.is_some_and(|(key, value)| key == b"v" && value == Value::Int);
        if !kind_ok || !v_ok {
            return p.fail_at(item_at, "an item doesn't start with its kind and v");
        }
        closed = p.separator()?;
    }
    // The payload's own table: its one field is done; a "," may follow it,
    // then "}".
    if !p.separator()? {
        return p.fail("expected }");
    }
    p.skip();
    if p.at < text.len() {
        return p.fail("bytes follow the payload's table");
    }
    Ok(())
}

/// What a value was, as far as an item's first two fields need it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Str(Vec<u8>),
    Int,
    Bool,
    Table,
}

/// A record's field: its name and value.
type Field = (Vec<u8>, Value);

type Step<T = ()> = Result<T, Error>;

/// `%a` or `_` (Lua's C locale: ASCII only).
fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

/// `%w` or `_`.
fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

struct Parser<'a> {
    text: &'a [u8],
    /// The next byte to read.
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn fail_at<T>(&self, at: usize, what: &'static str) -> Step<T> {
        Err(Error { at, what })
    }

    fn fail<T>(&self, what: &'static str) -> Step<T> {
        self.fail_at(self.at, what)
    }

    /// The byte `want`, or a failure at what is there instead.
    fn expect(&mut self, want: u8, what: &'static str) -> Step {
        if self.peek() != Some(want) {
            return self.fail(what);
        }
        self.at += 1;
        Ok(())
    }

    /// Skips whitespace: spaces, tabs and line ends.
    fn skip(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.at += 1;
        }
    }

    /// The length of the name `[%a_][%w_]*` at `at`, 0 when none.
    fn name_len(&self, at: usize) -> usize {
        match self.text.get(at) {
            Some(&b) if is_name_start(b) => {
                1 + self.text[at + 1..]
                    .iter()
                    .take_while(|&&b| is_word(b))
                    .count()
            }
            _ => 0,
        }
    }

    /// A double-quoted string at `at`, with only the `\\`, `\"` and `\n`
    /// escapes and no line end: its value.
    fn string(&mut self) -> Step<Vec<u8>> {
        let start = self.at;
        let mut value = Vec::new();
        let mut at = start + 1;
        loop {
            let Some(stop) = self.text[at.min(self.text.len())..]
                .iter()
                .position(|b| matches!(b, b'"' | b'\\' | b'\n' | b'\r'))
                .map(|n| at + n)
            else {
                return self.fail_at(start, "a string isn't closed");
            };
            value.extend_from_slice(&self.text[at..stop]);
            match self.text[stop] {
                b'"' => {
                    self.at = stop + 1;
                    return Ok(value);
                }
                b'\\' => {}
                _ => return self.fail_at(stop, "a line end inside a string"),
            }
            match self.text.get(stop + 1) {
                Some(b'\\') => value.push(b'\\'),
                Some(b'"') => value.push(b'"'),
                Some(b'n') => value.push(b'\n'),
                _ => {
                    return self.fail_at(stop, r#"a string has an escape other than \\, \" or \n"#);
                }
            }
            at = stop + 2;
        }
    }

    /// A decimal integer at `at`: `-?%d+`, not followed by `[%w_.]`, of at
    /// most [`DIGITS`] digits.
    fn integer(&mut self) -> Step {
        let start = self.at;
        let first = start + usize::from(self.peek() == Some(b'-'));
        let digits = self.text[first.min(self.text.len())..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits == 0 {
            return self.fail("an unexpected byte");
        }
        let last = first + digits;
        if self
            .text
            .get(last)
            .is_some_and(|&b| is_word(b) || b == b'.')
        {
            return self.fail_at(start, "a number that isn't a decimal integer");
        }
        if digits > DIGITS {
            return self.fail_at(start, "an integer of more than 15 digits");
        }
        self.at = last;
        Ok(())
    }

    /// Whether a field starts at `at`: `[`, or a name followed by `=` (not
    /// `==`).
    fn field_ahead(&self) -> bool {
        if self.peek() == Some(b'[') {
            return true;
        }
        let len = self.name_len(self.at);
        if len == 0 {
            return false;
        }
        let mut at = self.at + len;
        while matches!(self.text.get(at), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            at += 1;
        }
        self.text.get(at) == Some(&b'=') && self.text.get(at + 1) != Some(&b'=')
    }

    /// A field's name at `at`, bare or as `["name"]`.
    fn key(&mut self) -> Step<Vec<u8>> {
        let start = self.at;
        let len = self.name_len(start);
        if len > 0 {
            self.at += len;
            return Ok(self.text[start..start + len].to_vec());
        }
        if self.peek() != Some(b'[') {
            return self.fail("expected a field");
        }
        self.at += 1;
        self.skip();
        if self.peek() != Some(b'"') {
            return self.fail("expected \"");
        }
        let name = self.string()?;
        let is_name =
            name.first().is_some_and(|&b| is_name_start(b)) && name.iter().all(|&b| is_word(b));
        if !is_name {
            return self.fail_at(start, "a field name that isn't a name");
        }
        self.skip();
        self.expect(b']', "expected ]")?;
        Ok(name)
    }

    /// A value at `at`, in a table at `depth`.
    fn value(&mut self, depth: usize) -> Step<Value> {
        match self.peek() {
            Some(b'{') => {
                self.table(depth + 1)?;
                Ok(Value::Table)
            }
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b) if b == b'-' || b.is_ascii_digit() => {
                self.integer()?;
                Ok(Value::Int)
            }
            None => self.fail("the payload ends early"),
            Some(_) => {
                let len = self.name_len(self.at);
                if len == 0 {
                    return self.fail("an unexpected byte");
                }
                let name = &self.text[self.at..self.at + len];
                if name == b"true" || name == b"false" {
                    self.at += len;
                    return Ok(Value::Bool);
                }
                self.fail("a name where a value goes")
            }
        }
    }

    /// After an element: true at the table's `}` (read), false after its
    /// `,` with more to come.
    fn separator(&mut self) -> Step<bool> {
        self.skip();
        match self.peek() {
            Some(b',') => {
                self.at += 1;
                self.skip();
                if self.peek() == Some(b'}') {
                    self.at += 1;
                    return Ok(true);
                }
                Ok(false)
            }
            Some(b'}') => {
                self.at += 1;
                Ok(true)
            }
            None => self.fail("the payload ends early"),
            Some(_) => self.fail("expected , or }"),
        }
    }

    /// A table at `at` (`{`), at `depth`: a list of values, or a record of
    /// fields, never both. Returns a record's first two fields.
    fn table(&mut self, depth: usize) -> Step<(Option<Field>, Option<Field>)> {
        let open = self.at;
        if depth > DEPTH_LIMIT {
            return self.fail_at(open, "tables nest more than 16 deep");
        }
        self.at = open + 1;
        self.skip();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok((None, None));
        }
        let record = self.field_ahead();
        let mut names: HashSet<Vec<u8>> = HashSet::new();
        let (mut first, mut second) = (None, None);
        loop {
            if record {
                let key_at = self.at;
                let key = self.key()?;
                self.skip();
                self.expect(b'=', "expected =")?;
                self.skip();
                if names.contains(&key) {
                    return self.fail_at(key_at, "a field is given twice");
                }
                let value = self.value(depth)?;
                match names.len() {
                    0 => first = Some((key.clone(), value)),
                    1 => second = Some((key.clone(), value)),
                    _ => {}
                }
                names.insert(key);
            } else {
                if self.field_ahead() {
                    return self.fail("a table holds both values and fields");
                }
                self.value(depth)?;
            }
            if self.separator()? {
                return Ok((first, second));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::in_game::content_vector::{
        FUNCTION_CALL_PAYLOAD, GOOD_PAYLOAD, SINGLE_QUOTE_PAYLOAD, signed,
    };
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    /// A payload holding `items`, in the backend's canonical layout (HEV
    /// Contracts, The payload text).
    fn payload(items: &[&str]) -> String {
        let mut text = String::from("{items={\n");
        for item in items {
            text.push_str(item);
            text.push_str(",\n");
        }
        text.push_str("},}\n");
        text
    }

    const RUN: &str =
        r#"{kind="huntRun",v=1,event="p_test_tide",start=1791720000,["end"]=1791979200,}"#;

    fn passes(text: impl AsRef<[u8]>) {
        let text = text.as_ref();
        assert_eq!(check(text), Ok(()), "{}", String::from_utf8_lossy(text));
    }

    fn fails(text: impl AsRef<[u8]>, what: &str) -> usize {
        let text = text.as_ref();
        let error = check(text).expect_err(&String::from_utf8_lossy(text));
        assert_eq!(error.what, what, "{}", String::from_utf8_lossy(text));
        error.at
    }

    fn run_with(from: &str, to: &str) -> String {
        assert!(RUN.contains(from), "{from:?}");
        payload(&[&RUN.replacen(from, to, 1)])
    }

    // --- the shared test vector (HEV Contracts, Test vector)

    #[test]
    fn the_vectors_good_and_big_payloads_pass() {
        // HEV Contracts, Test vector: the checker accepts good and big
        passes(GOOD_PAYLOAD);
        let big = STANDARD.decode(signed("big").payload).unwrap();
        assert_eq!(big.len(), 65_536);
        passes(&big);
    }

    #[test]
    fn the_vectors_payloads_outside_the_grammar_fail() {
        // HEV Contracts, Test vector: function-call and single-quote are
        // refused; PUSH-R20: tables, strings, integers and booleans only
        let at = fails(FUNCTION_CALL_PAYLOAD, "a name where a value goes");
        assert_eq!(&FUNCTION_CALL_PAYLOAD[at..at + 6], b"print(");
        let at = fails(SINGLE_QUOTE_PAYLOAD, "an unexpected byte");
        assert_eq!(SINGLE_QUOTE_PAYLOAD[at], b'\'');
    }

    // --- the grammar's letter (PUSH Contracts, The payload)

    #[test]
    fn an_empty_payload_and_canonical_items_pass() {
        // HEV-R31: an empty schedule still gives a payload, with no items
        passes(payload(&[]));
        passes("{items={},}");
        passes(payload(&[RUN, RUN]));
    }

    #[test]
    fn values_are_strings_integers_booleans_and_tables() {
        // PUSH Contracts, The payload: value = str | int | bool | list | record
        passes(run_with(
            "start=1791720000,",
            "start=true,a=false,b={1,2,},c={x=1,},d={},",
        ));
        passes(run_with("start=1791720000,", "start=-12,"));
        fails(
            run_with("start=1791720000,", "start=nil,"),
            "a name where a value goes",
        );
        fails(
            run_with("start=1791720000,", "start=1.5,"),
            "a number that isn't a decimal integer",
        );
        fails(
            run_with("start=1791720000,", "start=42e0,"),
            "a number that isn't a decimal integer",
        );
        fails(
            run_with("start=1791720000,", "start=0x1F,"),
            "a number that isn't a decimal integer",
        );
        fails(
            run_with("start=1791720000,", "start=1_0,"),
            "a number that isn't a decimal integer",
        );
        fails(
            run_with("start=1791720000,", "start=-,"),
            "an unexpected byte",
        );
        fails(
            run_with("start=1791720000,", "start=[[x]],"),
            "an unexpected byte",
        );
        fails(
            run_with("start=1791720000,", "start=f(),"),
            "a name where a value goes",
        );
    }

    #[test]
    fn integers_have_at_most_15_digits() {
        // PUSH Contracts, The payload: an integer has at most 15 digits
        passes(run_with("1791720000", "999999999999999"));
        passes(run_with("1791720000", "-999999999999999"));
        fails(
            run_with("1791720000", "1000000000000000"),
            "an integer of more than 15 digits",
        );
        // Core reads leading zeros as decimal, so they pass as there.
        passes(run_with("1791720000", "000000000000001"));
        fails(
            run_with("1791720000", "0000000000000001"),
            "an integer of more than 15 digits",
        );
    }

    #[test]
    fn strings_take_only_three_escapes_and_no_line_end() {
        // PUSH Contracts, The payload: str has only \\, \" and \n escapes
        passes(run_with("p_test_tide", r#"a\"b\\c\nd"#));
        fails(
            run_with("p_test_tide", r"a\tb"),
            r#"a string has an escape other than \\, \" or \n"#,
        );
        fails(
            run_with("p_test_tide", r"a\065b"),
            r#"a string has an escape other than \\, \" or \n"#,
        );
        fails(
            run_with("p_test_tide", "a\\\nb"),
            r#"a string has an escape other than \\, \" or \n"#,
        );
        fails(
            run_with("p_test_tide", "a\nb"),
            "a line end inside a string",
        );
        fails(
            run_with("p_test_tide", "a\rb"),
            "a line end inside a string",
        );
        fails(r#"{items={{kind="huntRun"#, "a string isn't closed");
        fails(
            r#"{items={{kind="huntRun\"#,
            r#"a string has an escape other than \\, \" or \n"#,
        );
        // Other bytes, tabs and UTF-8 among them, are plain.
        passes(run_with("p_test_tide", "a\tb\u{e9}"));
    }

    #[test]
    fn a_field_name_may_be_bracketed_and_a_last_comma_left_out() {
        // PUSH Contracts, The payload: Core's reader also takes ["name"] and
        // a table's last element without its ","
        passes(run_with(r#"["end"]"#, "endTime"));
        passes(run_with("event=", r#"["event"]="#));
        passes(run_with(
            r#",["end"]=1791979200,}"#,
            r#",["end"]=1791979200}"#,
        ));
        passes(r#"{items={{kind="huntRun",v=1}},}"#);
        passes(r#"{items={{kind="huntRun",v=1,z={1,2}},}}"#);
        passes(r#"{["items"]={},}"#);
        fails(
            run_with(r#"["end"]"#, r#"["1end"]"#),
            "a field name that isn't a name",
        );
        fails(
            run_with(r#"["end"]"#, r#"["en d"]"#),
            "a field name that isn't a name",
        );
        fails(run_with(r#"["end"]"#, r#"[1]"#), "expected \"");
        fails(run_with(r#"["end"]"#, r#"["end"}"#), "expected ]");
        // Keywords are names too, as Core reads them.
        passes(run_with(r#"["end"]"#, "end"));
    }

    #[test]
    fn whitespace_between_tokens_passes() {
        // Core's reader skips spaces, tabs and line ends between tokens
        passes(
            " \t\r\n{ items = {\r\n { kind = \"huntRun\" , v = 1 , [ \"end\" ] = 2 , } ,\n} , }\r\n ",
        );
        fails(
            r#"{items={{kind="huntRun",v=1,s=- 1,},},}"#,
            "an unexpected byte",
        );
    }

    #[test]
    fn the_payload_is_one_items_table_and_nothing_after() {
        // PUSH Contracts, The payload: payload = "{" "items" "=" "{" *(item ",") "}" "," "}"
        assert_eq!(fails("", "expected {"), 0);
        fails("items={}", "expected {");
        fails("{things={},}", "expected items");
        fails("{items={}", "the payload ends early");
        fails("{items={},", "expected }");
        fails("{items={},x=1,}", "expected }");
        fails("{items={},};", "bytes follow the payload's table");
        fails("{items={},}{}", "bytes follow the payload's table");
        fails("{items:{},}", "expected =");
        fails("{items=1,}", "expected {");
        fails("{items={1,},}", "expected an item");
        fails(
            &payload(&[RUN])[..payload(&[RUN]).len() - 3],
            "the payload ends early",
        );
    }

    #[test]
    fn an_item_starts_with_its_kind_then_v() {
        // PUSH-R21: each item has a kind ([%w_]+) and a v (an integer), first
        // and in that order
        passes(payload(&[r#"{kind="notice",v=7,text="x",}"#]));
        passes(payload(&[r#"{kind="a_1",v=-1,}"#]));
        let bad = [
            r#"{v=1,kind="huntRun",}"#,
            r#"{kind="huntRun",}"#,
            r#"{kind="huntRun",x=1,v=1,}"#,
            r#"{kind="hunt-run",v=1,}"#,
            r#"{kind="",v=1,}"#,
            r#"{kind="a\nb",v=1,}"#,
            r#"{kind=1,v=1,}"#,
            r#"{kind="huntRun",v="1",}"#,
            r#"{kind="huntRun",v=true,}"#,
            r#"{kind="huntRun",v={},}"#,
            r#"{}"#,
            r#"{"huntRun",1,}"#,
        ];
        for item in bad {
            let at = fails(
                payload(&[item]),
                "an item doesn't start with its kind and v",
            );
            assert_eq!(at, "{items={\n".len(), "{item}");
        }
    }

    #[test]
    fn a_field_given_twice_or_a_mixed_table_fails() {
        // Core's reader: a record's names are distinct, and a table is a list
        // or a record, never both
        fails(
            run_with("start=", "event=\"x\",start="),
            "a field is given twice",
        );
        fails(run_with(r#"["end"]"#, "start"), "a field is given twice");
        fails(
            run_with("start=1791720000,", "start={1,x=2,},"),
            "a table holds both values and fields",
        );
        fails(
            run_with("start=1791720000,", "start={x=1,2,},"),
            "expected a field",
        );
        // `a==b` isn't a field.
        fails(
            run_with("start=1791720000,", "start={a==1,},"),
            "a name where a value goes",
        );
    }

    #[test]
    fn tables_nest_at_most_16_deep() {
        // PUSH Tunables: 16 tables deep, the payload's own table counting 1
        // (an item is at 3)
        let nested = |depth: usize| {
            let inner = 3..depth;
            let open = "{".repeat(inner.len());
            let close = "},".repeat(inner.len());
            payload(&[&format!(r#"{{kind="huntRun",v=1,x={open}{close}}}"#)])
        };
        passes(nested(16));
        let at = fails(nested(17), "tables nest more than 16 deep");
        assert_eq!(&nested(17).as_bytes()[at..at + 2], b"{}");
    }

    #[test]
    fn an_error_names_a_byte_and_never_the_content() {
        // CMP-R112: the reason is Core's, in words; nothing from the payload
        let text = run_with("p_test_tide", r"secret\q");
        let error = check(text.as_bytes()).unwrap_err();
        assert_eq!(&text.as_bytes()[error.at..error.at + 2], br"\q");
        assert!(!error.what.contains("secret"));
    }
}

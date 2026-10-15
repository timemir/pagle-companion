//! The Companion's own reader for the Lua 5.1 subset WoW writes to
//! SavedVariables (CMP Contracts, Parsing `Pagle.lua`; CMP-R31).
//!
//! It only reads: nothing in the file is ever executed, loaded or evaluated,
//! and no reference to another file is followed. Anything outside the subset,
//! anywhere in the file, is a parse failure.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

/// Every top-level `name = value` assignment, the last one of a name winning.
/// A name assigned `nil` is absent, as in Lua.
pub type Assignments = BTreeMap<String, Value>;

/// A value of the subset.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Nil,
    Bool(bool),
    Number(Number),
    /// The string's bytes after escapes are resolved, never its literal.
    String(Vec<u8>),
    Table(Table),
}

/// A number with the literal it was written as, so callers can tell an
/// integer literal (CMP-R31) from `1.0`, `0x1` or `-1`.
#[derive(Debug, Clone, PartialEq)]
pub struct Number {
    /// Whether a unary `-` preceded the literal.
    pub negative: bool,
    /// The literal as written, without the `-`.
    pub literal: String,
    /// Its value, as Lua 5.1 reads it.
    pub value: f64,
}

/// A table key: Lua compares numbers by value, so `[1]`, `[1.0]` and
/// `[0x1]` are one key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Bool(bool),
    Number(u64),
    String(Vec<u8>),
}

impl Key {
    fn number(value: f64) -> Self {
        // -0.0 and 0.0 are one key.
        Key::Number(if value == 0.0 { 0.0f64 } else { value }.to_bits())
    }
}

/// A table built by a constructor. A key given twice keeps its last value,
/// and a key set to `nil` is absent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    items: HashMap<Key, Value>,
}

impl Table {
    /// The value at a string key (`name = …` or `["name"] = …`).
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.items.get(&Key::String(name.as_bytes().to_vec()))
    }

    /// The value at a numeric key.
    pub fn get_index(&self, index: u64) -> Option<&Value> {
        self.items.get(&Key::number(index as f64))
    }

    /// The number of keys.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The values at `1 … n`, when those are the table's only keys: a list
    /// with no hole and no other key. Items may have been positional or
    /// `[n] =` keyed.
    pub fn as_list(&self) -> Option<Vec<&Value>> {
        (1..=self.items.len() as u64)
            .map(|i| self.get_index(i))
            .collect()
    }

    /// Like [`Table::as_list`], taking the values out of the table.
    pub fn into_list(mut self) -> Option<Vec<Value>> {
        (1..=self.items.len() as u64)
            .map(|i| self.items.remove(&Key::number(i as f64)))
            .collect()
    }

    /// Takes the value at a string key out of the table.
    pub fn remove(&mut self, name: &str) -> Option<Value> {
        self.items.remove(&Key::String(name.as_bytes().to_vec()))
    }

    fn set(&mut self, key: Key, value: Value) {
        if value == Value::Nil {
            self.items.remove(&key);
        } else {
            self.items.insert(key, value);
        }
    }
}

/// Why a file isn't in the subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line of the failure.
    pub line: usize,
    pub message: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parses a whole file of top-level assignments (CMP Contracts, Parsing
/// `Pagle.lua`).
pub fn parse(source: &[u8]) -> Result<Assignments, ParseError> {
    let mut parser = Parser {
        lexer: Lexer {
            src: source,
            pos: 0,
            line: 1,
        },
        peeked: None,
        depth: 0,
    };
    let mut all = Assignments::new();
    loop {
        match parser.next()? {
            Token::Eof => return Ok(all),
            Token::Name(name) => {
                parser.expect(Token::Sym(b'='))?;
                match parser.value()? {
                    Value::Nil => all.remove(&name),
                    value => all.insert(name, value),
                };
            }
            _ => return Err(parser.error("expected an assignment")),
        }
    }
}

/// Lua 5.1's reserved words: none is a name.
const KEYWORDS: [&str; 21] = [
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in", "local",
    "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// Lua 5.1's limit on nested syntax levels (LUAI_MAXCCALLS), so a deeply
/// nested file fails instead of exhausting the stack.
const MAX_DEPTH: usize = 200;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Eof,
    Name(String),
    Keyword(&'static str),
    String(Vec<u8>),
    Number { literal: String, value: f64 },
    Sym(u8),
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
}

impl Lexer<'_> {
    fn error(&self, message: &'static str) -> ParseError {
        ParseError {
            line: self.line,
            message,
        }
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek_at(0)?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
        }
        Some(b)
    }

    /// Consumes a line break: `\n`, `\r`, `\n\r` or `\r\n`, as Lua 5.1 does.
    fn newline(&mut self) {
        let first = self.bump();
        if let (Some(a), Some(b)) = (first, self.peek_at(0))
            && matches!(b, b'\n' | b'\r')
            && b != a
        {
            self.bump();
        }
    }

    fn next(&mut self) -> Result<Token, ParseError> {
        loop {
            let Some(b) = self.peek_at(0) else {
                return Ok(Token::Eof);
            };
            match b {
                b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c => {
                    self.bump();
                }
                b'-' if self.peek_at(1) == Some(b'-') => self.comment()?,
                b'-' | b'=' | b'{' | b'}' | b']' | b',' | b';' => {
                    self.bump();
                    return Ok(Token::Sym(b));
                }
                b'[' => {
                    return match self.long_bracket_level() {
                        Some(level) => Ok(Token::String(self.long_string(level)?)),
                        None if self.peek_at(1) == Some(b'=') => {
                            Err(self.error("invalid long string delimiter"))
                        }
                        None => {
                            self.bump();
                            Ok(Token::Sym(b'['))
                        }
                    };
                }
                b'"' | b'\'' => return self.short_string(b),
                b'0'..=b'9' => return self.number(),
                b'.' if self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) => {
                    return self.number();
                }
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => return Ok(self.name()),
                _ => return Err(self.error("unexpected symbol")),
            }
        }
    }

    /// `--…` to the end of the line, or a long comment `--[[…]]` of any level.
    fn comment(&mut self) -> Result<(), ParseError> {
        self.pos += 2;
        if self.peek_at(0) == Some(b'[')
            && let Some(level) = self.long_bracket_level()
        {
            self.long_string(level)?;
            return Ok(());
        }
        while let Some(b) = self.peek_at(0) {
            if b == b'\n' || b == b'\r' {
                break;
            }
            self.bump();
        }
        Ok(())
    }

    /// At a `[`: the level of the long bracket opening here, if one does.
    fn long_bracket_level(&self) -> Option<usize> {
        let mut level = 0;
        while self.peek_at(1 + level) == Some(b'=') {
            level += 1;
        }
        (self.peek_at(1 + level) == Some(b'[')).then_some(level)
    }

    /// A long string or comment body of `level`, past its opening bracket. A
    /// newline right after the opening bracket is dropped, and every line
    /// break inside reads as `\n`, as in Lua 5.1.
    fn long_string(&mut self, level: usize) -> Result<Vec<u8>, ParseError> {
        self.pos += level + 2;
        if matches!(self.peek_at(0), Some(b'\n' | b'\r')) {
            self.newline();
        }
        let mut bytes = Vec::new();
        loop {
            match self.peek_at(0) {
                None => return Err(self.error("unfinished long string")),
                Some(b']')
                    if (1..=level).all(|i| self.peek_at(i) == Some(b'='))
                        && self.peek_at(level + 1) == Some(b']') =>
                {
                    self.pos += level + 2;
                    return Ok(bytes);
                }
                Some(b'\n' | b'\r') => {
                    self.newline();
                    bytes.push(b'\n');
                }
                Some(b) => {
                    self.bump();
                    bytes.push(b);
                }
            }
        }
    }

    fn short_string(&mut self, quote: u8) -> Result<Token, ParseError> {
        self.bump();
        let mut bytes = Vec::new();
        loop {
            match self.peek_at(0) {
                None | Some(b'\n' | b'\r') => return Err(self.error("unfinished string")),
                Some(b) if b == quote => {
                    self.bump();
                    return Ok(Token::String(bytes));
                }
                Some(b'\\') => {
                    self.bump();
                    let escaped = match self.peek_at(0) {
                        Some(b'a') => 0x07,
                        Some(b'b') => 0x08,
                        Some(b'f') => 0x0c,
                        Some(b'n') => b'\n',
                        Some(b'r') => b'\r',
                        Some(b't') => b'\t',
                        Some(b'v') => 0x0b,
                        Some(b @ (b'\\' | b'"' | b'\'')) => b,
                        Some(b'\n' | b'\r') => {
                            self.newline();
                            bytes.push(b'\n');
                            continue;
                        }
                        Some(b'0'..=b'9') => {
                            let mut code: u32 = 0;
                            for _ in 0..3 {
                                match self.peek_at(0) {
                                    Some(d @ b'0'..=b'9') => {
                                        code = code * 10 + u32::from(d - b'0');
                                        self.bump();
                                    }
                                    _ => break,
                                }
                            }
                            let byte = u8::try_from(code)
                                .map_err(|_| self.error("escape sequence too large"))?;
                            bytes.push(byte);
                            continue;
                        }
                        _ => return Err(self.error("invalid escape sequence")),
                    };
                    self.bump();
                    bytes.push(escaped);
                }
                Some(b) => {
                    self.bump();
                    bytes.push(b);
                }
            }
        }
    }

    /// A decimal, exponent or hex numeral; it may not run into a letter,
    /// digit, `_` or `.`.
    fn number(&mut self) -> Result<Token, ParseError> {
        let start = self.pos;
        let value = if self.peek_at(0) == Some(b'0') && matches!(self.peek_at(1), Some(b'x' | b'X'))
        {
            self.pos += 2;
            let mut value = 0f64;
            let mut digits = 0;
            while let Some(d) = self.peek_at(0).and_then(|c| (c as char).to_digit(16)) {
                value = value * 16.0 + f64::from(d);
                digits += 1;
                self.pos += 1;
            }
            if digits == 0 {
                return Err(self.error("malformed number"));
            }
            value
        } else {
            let mut digits = self.digits();
            if self.peek_at(0) == Some(b'.') {
                self.pos += 1;
                digits += self.digits();
            }
            if digits == 0 {
                return Err(self.error("malformed number"));
            }
            if matches!(self.peek_at(0), Some(b'e' | b'E')) {
                self.pos += 1;
                if matches!(self.peek_at(0), Some(b'+' | b'-')) {
                    self.pos += 1;
                }
                if self.digits() == 0 {
                    return Err(self.error("malformed number"));
                }
            }
            let text = std::str::from_utf8(&self.src[start..self.pos]).expect("ASCII");
            text.parse::<f64>()
                .map_err(|_| self.error("malformed number"))?
        };
        if self
            .peek_at(0)
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.')
        {
            return Err(self.error("malformed number"));
        }
        let literal = String::from_utf8(self.src[start..self.pos].to_vec()).expect("ASCII");
        Ok(Token::Number { literal, value })
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while self.peek_at(0).is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn name(&mut self) -> Token {
        let start = self.pos;
        while self
            .peek_at(0)
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.pos += 1;
        }
        let name = std::str::from_utf8(&self.src[start..self.pos]).expect("ASCII");
        match KEYWORDS.iter().find(|k| **k == name) {
            Some(keyword) => Token::Keyword(keyword),
            None => Token::Name(name.to_owned()),
        }
    }
}

struct Parser<'a> {
    lexer: Lexer<'a>,
    peeked: Option<Token>,
    depth: usize,
}

impl Parser<'_> {
    fn error(&self, message: &'static str) -> ParseError {
        self.lexer.error(message)
    }

    fn next(&mut self) -> Result<Token, ParseError> {
        match self.peeked.take() {
            Some(token) => Ok(token),
            None => self.lexer.next(),
        }
    }

    fn peek(&mut self) -> Result<&Token, ParseError> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lexer.next()?);
        }
        Ok(self.peeked.as_ref().expect("just peeked"))
    }

    fn expect(&mut self, token: Token) -> Result<(), ParseError> {
        if self.next()? == token {
            Ok(())
        } else {
            Err(self.error("unexpected symbol"))
        }
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        match self.next()? {
            Token::Keyword("nil") => Ok(Value::Nil),
            Token::Keyword("true") => Ok(Value::Bool(true)),
            Token::Keyword("false") => Ok(Value::Bool(false)),
            Token::String(bytes) => Ok(Value::String(bytes)),
            Token::Number { literal, value } => Ok(Value::Number(Number {
                negative: false,
                literal,
                value,
            })),
            Token::Sym(b'-') => match self.next()? {
                Token::Number { literal, value } => Ok(Value::Number(Number {
                    negative: true,
                    literal,
                    value: -value,
                })),
                _ => Err(self.error("unary minus takes a number only")),
            },
            Token::Sym(b'{') => self.table(),
            _ => Err(self.error("unexpected symbol")),
        }
    }

    /// The rest of a table constructor, past its `{`.
    fn table(&mut self) -> Result<Value, ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("tables nested too deeply"));
        }
        let mut table = Table::default();
        let mut position = 0u64;
        loop {
            if *self.peek()? == Token::Sym(b'}') {
                self.next()?;
                break;
            }
            match self.peek()? {
                Token::Sym(b'[') => {
                    self.next()?;
                    let key = match self.value()? {
                        Value::Bool(b) => Key::Bool(b),
                        Value::Number(n) => Key::number(n.value),
                        Value::String(s) => Key::String(s),
                        Value::Nil | Value::Table(_) => {
                            return Err(self.error("table key is nil or a table"));
                        }
                    };
                    self.expect(Token::Sym(b']'))?;
                    self.expect(Token::Sym(b'='))?;
                    let value = self.value()?;
                    table.set(key, value);
                }
                Token::Name(_) => {
                    let Token::Name(name) = self.next()? else {
                        unreachable!("just peeked a name")
                    };
                    self.expect(Token::Sym(b'='))?;
                    let value = self.value()?;
                    table.set(Key::String(name.into_bytes()), value);
                }
                _ => {
                    let value = self.value()?;
                    position += 1;
                    table.set(Key::number(position as f64), value);
                }
            }
            match self.next()? {
                Token::Sym(b',' | b';') => {}
                Token::Sym(b'}') => break,
                _ => return Err(self.error("expected `,`, `;` or `}`")),
            }
        }
        self.depth -= 1;
        Ok(Value::Table(table))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(source: &str) -> Result<Assignments, ParseError> {
        parse(source.as_bytes())
    }

    fn value(source: &str) -> Value {
        let mut all = parse_str(&format!("x = {source}")).expect("parses");
        all.remove("x").unwrap_or(Value::Nil)
    }

    fn string(source: &str) -> Vec<u8> {
        match value(source) {
            Value::String(bytes) => bytes,
            other => panic!("not a string: {other:?}"),
        }
    }

    fn number(source: &str) -> Number {
        match value(source) {
            Value::Number(n) => n,
            other => panic!("not a number: {other:?}"),
        }
    }

    fn table(source: &str) -> Table {
        match value(source) {
            Value::Table(t) => t,
            other => panic!("not a table: {other:?}"),
        }
    }

    fn fails(source: &str) {
        assert!(parse_str(source).is_err(), "should fail: {source:?}");
    }

    const PAGLE_LUA: &str = r#"
-- written by the game
PagleDB = {
	["profileKeys"] = {
		["Ada Brook - Living Flame"] = "Default",
	},
	["global"] = {
		["decodeErrors"] = 0,
		["note"] = [==[
a long ]] string]==],
		["mask"] = 0xFF,
		["ratio"] = -1.5e-3,
	},
}
--[[ a long
comment ]]
PagleLog = {
	["format"] = 1,
	["logId"] = "3f9a2c1e7b0d4a65",
	["characters"] = {
		{
			["guid"] = "Player-4618-0A1B2C3D",
			["name"] = "Ada",
			["surname"] = "Brook",
			["classID"] = 1,
			["raceID"] = 1,
			["faction"] = "Alliance",
			["realmID"] = 4618,
			["hardcore"] = false,
			["firstSeen"] = 1791731680,
			["lastSeen"] = 1791735090,
		}, -- [1]
	},
	["entries"] = {
		[1] = "C1|1|1791735050|0|c|494|1439|1|148|453|86|25|263|||6358x1",
		[2] = "S1|1|1791735090|0|87|150|25",
		[3] = "A1|1|1791735095|0|Mod|note|caf\195\169 \"x\"\9\124",
	},
}
"#;

    #[test]
    fn a_saved_variables_file_parses_and_strings_are_their_bytes_after_escapes() {
        // CMP Contracts, Parsing `Pagle.lua`: PagleDB, comments, long strings,
        // \ddd escapes, hex numbers and [n] = keyed entries all parse
        let all = parse_str(PAGLE_LUA).expect("parses");
        let Some(Value::Table(log)) = all.get("PagleLog") else {
            panic!("no PagleLog table");
        };
        let Some(Value::Table(entries)) = log.get("entries") else {
            panic!("no entries table");
        };
        assert_eq!(
            entries.get_index(3),
            Some(&Value::String(
                b"A1|1|1791735095|0|Mod|note|caf\xc3\xa9 \"x\"\t|".to_vec()
            ))
        );
        assert!(matches!(all.get("PagleDB"), Some(Value::Table(_))));
    }

    #[test]
    fn functions_names_as_values_and_unterminated_long_strings_fail() {
        // CMP Contracts, Parsing `Pagle.lua`: anything else is a parse failure
        fails("x = function() end");
        fails("PagleLog = { a = b }");
        fails("x = [==[ never closed ]=]");
        fails("x = [[ never closed");
    }

    #[test]
    fn nothing_outside_the_subset_parses() {
        // CMP Contracts, Parsing `Pagle.lua`: anything else anywhere in the file
        fails("x = 1 + 2");
        fails("x = 'a' .. 'b'");
        fails("x = {} y");
        fails("x");
        fails("x.y = 1");
        fails("x, y = 1, 2");
        fails("local x = 1");
        fails("x = f()");
        fails("x = { [nil] = 1 }");
        fails("x = { end = 1 }");
        fails("end = 1");
        fails("x = - -1");
        fails("x = -'a'");
        fails("x = #t");
        fails("x = {,}");
        fails("x = { 1,, 2 }");
        fails("x = { 1 2 }");
        fails("x = (1)");
        fails("x = 1;");
        fails("\u{feff}x = 1");
    }

    #[test]
    fn nothing_is_evaluated() {
        // CMP Contracts, Parsing `Pagle.lua`: never executes, loads or evaluates
        fails("x = os.execute('rm -rf /')");
        fails("x = dofile('other.lua')");
        fails("x = loadstring('return 1')()");
        fails("x = require 'socket'");
    }

    #[test]
    fn an_empty_file_or_one_of_comments_has_no_assignment() {
        // CMP Contracts, Parsing `Pagle.lua`: comments of any kind
        assert_eq!(parse_str(""), Ok(Assignments::new()));
        assert_eq!(
            parse_str("-- a\n--[[ b ]] --[=[ c ]=]"),
            Ok(Assignments::new())
        );
    }

    #[test]
    fn a_name_assigned_twice_keeps_its_last_value_and_nil_removes_it() {
        // CMP Contracts, Parsing `Pagle.lua`: a key given twice keeps its last value
        let all = parse_str("a = 1 a = 'two' b = 1 b = nil").unwrap();
        assert_eq!(all.get("a"), Some(&Value::String(b"two".to_vec())));
        assert_eq!(all.get("b"), None);
    }

    #[test]
    fn every_short_string_escape_resolves_to_its_byte() {
        // CMP Contracts, Parsing `Pagle.lua`: every Lua 5.1 escape
        assert_eq!(
            string(r#""\a\b\f\n\r\t\v\\\"\'""#),
            b"\x07\x08\x0c\n\r\t\x0b\\\"'".to_vec()
        );
        assert_eq!(string(r#"'\0\65\066\255x\1234'"#), b"\0AB\xffx{4".to_vec());
        assert_eq!(string("\"a\\\nb\""), b"a\nb".to_vec());
        assert_eq!(string("\"a\\\r\nb\""), b"a\nb".to_vec());
        assert_eq!(string("'single \"quotes\"'"), b"single \"quotes\"".to_vec());
    }

    #[test]
    fn bad_short_strings_fail() {
        // CMP Contracts, Parsing `Pagle.lua`: only Lua 5.1's escapes
        fails(r#"x = "\256""#);
        fails(r#"x = "\q""#);
        fails("x = \"line\nbreak\"");
        fails("x = \"never closed");
        fails("x = 'mixed\"");
    }

    #[test]
    fn long_strings_of_any_level_drop_a_newline_after_the_opening_bracket() {
        // CMP Contracts, Parsing `Pagle.lua`: long strings [[…]], [=[…]=] of any level
        assert_eq!(string("[[\nab]]"), b"ab".to_vec());
        assert_eq!(string("[[\r\nab]]"), b"ab".to_vec());
        assert_eq!(string("[[\n\nab]]"), b"\nab".to_vec());
        assert_eq!(string("[[a\\n]]"), b"a\\n".to_vec());
        assert_eq!(string("[=[a]]b]=]"), b"a]]b".to_vec());
        assert_eq!(string("[===[x]==]y]===]"), b"x]==]y".to_vec());
        assert_eq!(string("[[]]"), Vec::<u8>::new());
    }

    #[test]
    fn numbers_in_decimal_exponent_and_hex_forms_with_unary_minus() {
        // CMP Contracts, Parsing `Pagle.lua`: decimal, exponent and hex numbers
        assert_eq!(number("42").value, 42.0);
        assert_eq!(number("3.").value, 3.0);
        assert_eq!(number(".5").value, 0.5);
        assert_eq!(number("1.5e3").value, 1500.0);
        assert_eq!(number("2E-2").value, 0.02);
        assert_eq!(number("1e+2").value, 100.0);
        assert_eq!(number("0x1F").value, 31.0);
        assert_eq!(number("0XaB").value, 171.0);
        let minus = number("- 7");
        assert!(minus.negative);
        assert_eq!(minus.literal, "7");
        assert_eq!(minus.value, -7.0);
        assert_eq!(number("-0x10").value, -16.0);
        assert_eq!(number("1.0").literal, "1.0");
    }

    #[test]
    fn malformed_numbers_fail() {
        // CMP Contracts, Parsing `Pagle.lua`: anything else is a parse failure
        fails("x = 1e");
        fails("x = 0x");
        fails("x = 12abc");
        fails("x = 1.2.3");
        fails("x = 0x1.8");
        fails("x = .");
    }

    #[test]
    fn true_false_and_nil() {
        // CMP Contracts, Parsing `Pagle.lua`: true, false, nil
        assert_eq!(value("true"), Value::Bool(true));
        assert_eq!(value("false"), Value::Bool(false));
        assert_eq!(value("nil"), Value::Nil);
    }

    #[test]
    fn table_constructors_take_keyed_named_and_positional_items() {
        // CMP Contracts, Parsing `Pagle.lua`: [key] =, name = and positional items
        let t = table("{ 'a'; [5] = 'e', b = true, ['c d'] = 1, 'z', }");
        assert_eq!(t.get_index(1), Some(&Value::String(b"a".to_vec())));
        assert_eq!(t.get_index(2), Some(&Value::String(b"z".to_vec())));
        assert_eq!(t.get_index(5), Some(&Value::String(b"e".to_vec())));
        assert_eq!(t.get("b"), Some(&Value::Bool(true)));
        assert!(matches!(t.get("c d"), Some(Value::Number(_))));
        assert_eq!(t.len(), 5);
        assert!(table("{}").is_empty());
        assert!(table("{ { {} }; }").get_index(1).is_some());
    }

    #[test]
    fn a_key_given_twice_keeps_its_last_value() {
        // CMP Contracts, Parsing `Pagle.lua`: a key given twice keeps its last value
        let t = table("{ a = 1, ['a'] = 2, [1] = 'x', [1.0] = 'y', [0x2] = 'p', 'q', 'r' }");
        assert_eq!(
            t.get("a")
                .map(|v| matches!(v, Value::Number(n) if n.value == 2.0)),
            Some(true)
        );
        assert_eq!(t.get_index(1), Some(&Value::String(b"q".to_vec())));
        assert_eq!(t.get_index(2), Some(&Value::String(b"r".to_vec())));
        let t = table("{ a = 1, a = nil }");
        assert!(t.is_empty());
    }

    #[test]
    fn a_list_is_keys_one_to_n_with_no_hole_and_no_other_key() {
        // CMP Contracts, Parsing `Pagle.lua`: positional or [n] = keyed, the key is the index
        assert_eq!(table("{ 'a', 'b' }").as_list().map(|l| l.len()), Some(2));
        assert_eq!(
            table("{ [2] = 'b', [1] = 'a' }").as_list().map(|l| l.len()),
            Some(2)
        );
        assert_eq!(table("{}").as_list().map(|l| l.len()), Some(0));
        assert!(table("{ [1] = 'a', [3] = 'c' }").as_list().is_none());
        assert!(table("{ 'a', nil, 'c' }").as_list().is_none());
        assert!(table("{ 'a', x = 1 }").as_list().is_none());
        assert!(table("{ [0] = 'z', 'a' }").as_list().is_none());
    }

    #[test]
    fn comments_may_sit_anywhere_between_tokens() {
        // CMP Contracts, Parsing `Pagle.lua`: comments --… and long comments of any level
        let all =
            parse_str("x --[==[ ]] still ]==] = -- c\n { --[[x]] 1 --\n , - --[[m]] 2 } -- end")
                .unwrap();
        let Some(Value::Table(t)) = all.get("x") else {
            panic!()
        };
        assert!(matches!(t.get_index(2), Some(Value::Number(n)) if n.value == -2.0));
        fails("x = 1 --[[ never closed");
        // `--[` without a second bracket is a short comment.
        assert!(parse_str("--[= not long\nx = 1").unwrap().contains_key("x"));
    }

    #[test]
    fn deep_nesting_is_a_failure_not_a_crash() {
        // CMP Contracts, Parsing `Pagle.lua`: anything else is a parse failure
        let deep = format!("x = {}{}", "{".repeat(10_000), "}".repeat(10_000));
        fails(&deep);
        let ok = format!("x = {}{}", "{".repeat(100), "}".repeat(100));
        assert!(parse_str(&ok).is_ok());
    }

    #[test]
    fn a_failure_names_its_line() {
        // CMP-R30: a parse failure is reported, never evaluated around
        assert_eq!(parse_str("a = 1\nb = 2\nc = d").unwrap_err().line, 3);
    }
}

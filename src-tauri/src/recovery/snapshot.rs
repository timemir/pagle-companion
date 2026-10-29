//! The complete semantic snapshot shared with the manual addon exporter.
use crate::pagle_log::{Character, LocalLog, MAX_NUMBER};

pub const CAP: usize = 16 * 1024 * 1024;
pub const HEADER: &[u8] = b"PGL-SNAPSHOT|1\n";

pub fn field(out: &mut Vec<u8>, value: impl AsRef<[u8]>) {
    let value = value.as_ref();
    out.extend_from_slice(value.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(value);
    out.push(b',');
}
fn number(out: &mut Vec<u8>, value: u64) {
    field(out, value.to_string());
}
fn boolean(out: &mut Vec<u8>, value: Option<bool>) {
    field(
        out,
        match value {
            None => "n",
            Some(false) => "f",
            Some(true) => "t",
        },
    );
}

pub fn encode(log: &LocalLog) -> Vec<u8> {
    let mut out = HEADER.to_vec();
    field(&mut out, &log.log_id);
    number(&mut out, log.format);
    boolean(&mut out, log.test);
    number(&mut out, log.characters.len() as u64);
    for c in &log.characters {
        field(&mut out, &c.guid);
        field(&mut out, &c.name);
        field(
            &mut out,
            c.surname
                .as_ref()
                .map_or_else(|| "n".to_owned(), |s| format!("s{s}")),
        );
        number(&mut out, c.class_id);
        number(&mut out, c.race_id);
        field(&mut out, &c.faction);
        number(&mut out, c.realm_id);
        boolean(&mut out, c.hardcore);
        number(&mut out, c.first_seen);
        number(&mut out, c.last_seen);
        boolean(&mut out, c.self_found);
    }
    number(&mut out, log.len());
    for seq in 1..=log.len() {
        field(&mut out, log.entry(seq).expect("snapshot entry"));
    }
    out
}

pub struct Reader<'a> {
    pub rest: &'a [u8],
}
impl<'a> Reader<'a> {
    pub fn field(&mut self) -> Result<&'a [u8], String> {
        let colon = self
            .rest
            .iter()
            .position(|b| *b == b':')
            .ok_or("Missing snapshot length")?;
        let size = canonical_number(&self.rest[..colon])?;
        let size = usize::try_from(size).map_err(|_| "Snapshot length out of range")?;
        let after = self.rest.get(colon + 1..).ok_or("Missing snapshot bytes")?;
        if after.get(size) != Some(&b',') {
            return Err("Truncated snapshot field".into());
        }
        let value = &after[..size];
        self.rest = &after[size + 1..];
        Ok(value)
    }
    pub fn text(&mut self) -> Result<String, String> {
        String::from_utf8(self.field()?.to_vec()).map_err(|_| "Invalid snapshot UTF-8".into())
    }
    pub fn number(&mut self) -> Result<u64, String> {
        canonical_number(self.field()?)
    }
    pub fn boolean(&mut self) -> Result<Option<bool>, String> {
        match self.field()? {
            b"n" => Ok(None),
            b"f" => Ok(Some(false)),
            b"t" => Ok(Some(true)),
            _ => Err("Invalid snapshot boolean".into()),
        }
    }
    pub fn finish(self) -> Result<(), String> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err("Unexpected trailing Recovery data".into())
        }
    }
}
fn canonical_number(value: &[u8]) -> Result<u64, String> {
    if value.is_empty()
        || value.len() > 16
        || (value.len() > 1 && value[0] == b'0')
        || !value.iter().all(u8::is_ascii_digit)
    {
        return Err("Invalid snapshot number".into());
    }
    std::str::from_utf8(value)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|v| *v <= MAX_NUMBER)
        .ok_or_else(|| "Snapshot number out of range".into())
}
pub fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn decode(bytes: &[u8]) -> Result<LocalLog, String> {
    if bytes.len() > CAP {
        return Err("Recovery snapshot too large".into());
    }
    let mut r = Reader {
        rest: bytes
            .strip_prefix(HEADER)
            .ok_or("Invalid snapshot header")?,
    };
    let log_id = r.text()?;
    if !hex(&log_id, 16) {
        return Err("Invalid snapshot log ID".into());
    }
    let format = r.number()?;
    if !matches!(format, 3 | 4) {
        return Err("Recovery needs Catch log format 3 or 4; update Pagle first".into());
    }
    let test = r.boolean()?;
    let n = r.number()?;
    if n > 100 {
        return Err("Too many Recovery characters".into());
    }
    let mut characters = Vec::new();
    let mut guids = std::collections::HashSet::new();
    for _ in 0..n {
        let guid = r.text()?;
        let name = r.text()?;
        if !guids.insert(guid.clone()) {
            return Err("Repeated Recovery character".into());
        }
        let surname = match r.field()? {
            b"n" => None,
            [b's', rest @ ..] => {
                Some(String::from_utf8(rest.to_vec()).map_err(|_| "Invalid surname UTF-8")?)
            }
            _ => return Err("Invalid nullable surname".into()),
        };
        characters.push(Character {
            guid,
            name,
            surname,
            class_id: r.number()?,
            race_id: r.number()?,
            faction: r.text()?,
            realm_id: r.number()?,
            hardcore: r.boolean()?,
            first_seen: r.number()?,
            last_seen: r.number()?,
            self_found: r.boolean()?,
        });
    }
    if !characters
        .iter()
        .all(crate::pagle_log::valid_snapshot_character)
    {
        return Err("Invalid Recovery character metadata".into());
    }
    let n = r.number()?;
    if n > 100_000 {
        return Err("Too many Recovery entries".into());
    }
    let mut entries = Vec::new();
    for _ in 0..n {
        let entry = r.field()?;
        if entry.contains(&b'\n') {
            return Err("Recovery entry contains LF".into());
        }
        entries.push(entry.to_vec());
    }
    r.finish()?;
    Ok(LocalLog {
        log_id,
        format,
        test,
        characters,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pagle_log::{self, Found};

    const ADA: &str = r#"{ guid = "Player-4618-0A1B2C3D", name = "Ada", surname = "Brook",
        classID = 1, raceID = 1, faction = "Alliance", realmID = 4618, hardcore = false,
        selfFound = false, firstSeen = 1791731680, lastSeen = 1791735090 }"#;

    fn log(format: u64) -> LocalLog {
        let storage = if format >= 4 {
            "blockSize = 2, count = 3, blocks = { \"C1|1|1791735050|0|c\\nS1|1|1791735090|0|87|150|25\", \"S1|1|1791735091|0|87|150|25\" }"
        } else {
            "entries = { \"C1|1|1791735050|0|c\", \"S1|1|1791735090|0|87|150|25\", \"S1|1|1791735091|0|87|150|25\" }"
        };
        let source = format!(
            "PagleLog = {{ format = {format}, logId = \"3f9a2c1e7b0d4a65\", characters = {{ {ADA} }}, {storage} }}"
        );
        match pagle_log::parse(source.as_bytes()).expect("parses") {
            Found::Valid(log) => log,
            other => panic!("not valid: {other:?}"),
        }
    }

    #[test]
    fn recovery_snapshots_round_trip_in_format_3_and_format_4() {
        // recovery-v1: the snapshot names the log's format, 3 or 4, over LF-free entries
        let (three, four) = (log(3), log(4));
        assert_eq!(decode(&encode(&three)).expect("format 3"), three);
        assert_eq!(decode(&encode(&four)).expect("format 4"), four);
        assert_eq!(four.format, 4);
        assert_eq!(four.entries, three.entries);
        assert_ne!(encode(&three), encode(&four));
    }

    #[test]
    fn a_recovery_snapshot_in_another_format_is_refused() {
        // recovery-v1: only the formats the backend accepts (3, and 4 from the pin bump)
        for format in [0, 1, 2, 5] {
            let mut snapshot = log(3);
            snapshot.format = format;
            assert!(decode(&encode(&snapshot)).is_err(), "format {format}");
        }
    }
}

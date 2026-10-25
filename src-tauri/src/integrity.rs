//! Canonical Core records and versioned checkpoints. Module payload schemas
//! are validated by their owning contract in addition to this envelope.

use sha2::{Digest, Sha256};

const MAX: u64 = (1 << 53) - 1;

fn number(text: &[u8], minimum: i64, signed: bool) -> Option<i64> {
    let digits = if signed && text.starts_with(b"-") {
        &text[1..]
    } else {
        text
    };
    if digits.is_empty()
        || digits.len() > 16
        || !digits.iter().all(u8::is_ascii_digit)
        || (digits.len() > 1 && digits[0] == b'0')
        || (text.starts_with(b"-") && digits == b"0")
    {
        return None;
    }
    let value = std::str::from_utf8(text).ok()?.parse::<i64>().ok()?;
    (value >= minimum && value.unsigned_abs() <= MAX).then_some(value)
}

fn optional(text: &[u8], signed: bool) -> bool {
    text.is_empty() || number(text, if signed { -(MAX as i64) } else { 0 }, signed).is_some()
}

fn name(text: &[u8], hyphen: bool) -> bool {
    !text.is_empty()
        && text.iter().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.') || (hyphen && *b == b'-')
        })
}

fn ids(text: &[u8]) -> bool {
    if text.is_empty() {
        return true;
    }
    let mut previous = -1;
    for field in text.split(|b| *b == b',') {
        let Some(value) = number(field, 0, false) else {
            return false;
        };
        if value <= previous {
            return false;
        }
        previous = value;
    }
    true
}

fn items(text: &[u8]) -> bool {
    text.is_empty()
        || text.split(|b| *b == b',').all(|item| {
            let pair = item.split(|b| *b == b'x').collect::<Vec<_>>();
            pair.len() == 2
                && number(pair[0], 0, false).is_some()
                && number(pair[1], 1, false).is_some()
        })
}

fn entry_id(text: &[u8]) -> bool {
    let f = text.split(|b| *b == b'-').collect::<Vec<_>>();
    f.len() == 5
        && f[0] == b"Player"
        && !f[1].is_empty()
        && f[1].iter().all(u8::is_ascii_digit)
        && !f[2].is_empty()
        && f[2]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(b))
        && number(f[3], 0, false).is_some()
        && number(f[4], 0, false).is_some()
}

fn variety_target(text: &[u8]) -> bool {
    let f = text.split(|b| *b == b':').collect::<Vec<_>>();
    if f.len() != 2 || number(f[0], 1, false).is_none() {
        return false;
    }
    let mut previous = 0;
    for item in f[1].split(|b| *b == b',') {
        let Some(value) = number(item, 1, false) else {
            return false;
        };
        if value <= previous {
            return false;
        }
        previous = value;
    }
    true
}

fn board_payload(kind: &[u8], text: &[u8]) -> bool {
    let f = text.split(|b| *b == b';').collect::<Vec<_>>();
    if matches!(kind, b"start" | b"day") {
        return f.len() == 2
            && number(f[0], 0, false).is_some()
            && number(f[1], 0, false).is_some();
    }
    let start = match kind {
        b"issue" => 0,
        b"reroll" => 1,
        _ => return false,
    };
    if f.len() != start + 6 || (start == 1 && !entry_id(f[0])) {
        return false;
    }
    let (goal_type, goal, target) = (f[start], f[start + 1], f[start + 5]);
    if !matches!(goal_type, b"n" | b"e")
        || !number(f[start + 2], 0, false).is_some_and(|v| v <= 3)
        || number(f[start + 3], 0, false).is_none()
        || number(f[start + 4], 1, false).is_none()
    {
        return false;
    }
    match goal {
        b"haul" | b"quality" => number(target, 1, false).is_some(),
        b"species" => goal_type == b"n" && number(target, 1, false).is_some(),
        b"lure" => goal_type == b"n" && target.is_empty(),
        b"collector" => target.is_empty(),
        b"wanderer" => {
            if goal_type == b"n" {
                target.is_empty()
            } else {
                number(target, 1, false).is_some()
            }
        }
        b"variety" => {
            if goal_type == b"n" {
                number(target, 1, false).is_some()
            } else {
                variety_target(target)
            }
        }
        _ => false,
    }
}

fn hunt_payload(kind: &[u8], text: &[u8]) -> bool {
    if kind == b"run" {
        let f = text.split(|b| *b == b'|').collect::<Vec<_>>();
        return f.len() == 4
            && name(f[0], false)
            && !f[0].contains(&b'.')
            && number(f[1], 0, false).is_some()
            && number(f[2], 0, false).is_some()
            && matches!(f[3], b"r" | b"p");
    }
    if kind != b"def" {
        return false;
    }
    let f = text.splitn(3, |b| *b == b'|').collect::<Vec<_>>();
    if f.len() != 3
        || !f[0].starts_with(b"p_")
        || f[0].len() <= 2
        || !name(f[0], false)
        || f[0].contains(&b'.')
        || f[2].is_empty()
    {
        return false;
    }
    let parts = f[1].split(|b| *b == b'/').collect::<Vec<_>>();
    parts.len() == 2
        && number(parts[0], 1, false).is_some()
        && number(parts[1], 1, false).is_some()
        && number(parts[0], 1, false) <= number(parts[1], 1, false)
}

fn module_payload(module: &[u8], kind: &[u8], payload: &[u8]) -> bool {
    match module {
        b"Ledger" => match kind {
            b"title" => payload
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':')),
            b"crest" => payload.is_empty() || name(payload, false),
            _ => false,
        },
        b"TackleBoard" => board_payload(kind, payload),
        b"Hunts" => hunt_payload(kind, payload),
        _ => false,
    }
}

/// Validates Core fields without treating a valid Module envelope as a valid
/// Module payout, definition, or subtype. Hashing is independent of validation.
pub fn validate_record(entry: &[u8]) -> Result<(), String> {
    let invalid = || "invalid or unsupported canonical Core record".to_owned();
    if entry.contains(&b'\n') {
        return Err(invalid());
    }
    let f = entry.split(|b| *b == b'|').collect::<Vec<_>>();
    if f.len() < 4
        || number(f[1], 1, false).is_none()
        || number(f[2], 0, false).is_none()
        || number(f[3], 0, false).is_none()
    {
        return Err(invalid());
    }
    let valid = match f[0] {
        b"C1" if f.len() == 16 => {
            matches!(f[4], b"c" | b"g")
                && optional(f[5], false)
                && (f[5].is_empty() || number(f[5], 0, false).is_some_and(|v| v < 1440))
                && optional(f[6], false)
                && optional(f[7], false)
                && number(f[8], 0, false).is_some()
                && number(f[9], 0, false).is_some()
                && optional(f[10], false)
                && optional(f[11], true)
                && optional(f[12], false)
                && ids(f[13])
                && ids(f[14])
                && items(f[15])
                && (f[4] != b"g" || f[15].is_empty())
        }
        b"S1" if f.len() == 7 => {
            number(f[4], 0, false).is_some()
                && number(f[5], 0, false).is_some()
                && number(f[6], -(MAX as i64), true).is_some()
        }
        b"P1" if f.len() == 6 => {
            number(f[4], 1, false).is_some() && number(f[5], 0, false).is_some()
        }
        b"B1" if f.len() == 7 => number(f[4], 0, false).is_some() && !f[5].is_empty()
            && f[6].len() <= 64 && f[6].first().is_some_and(u8::is_ascii_alphanumeric)
            && name(f[6], true),
        b"U1" if f.len() == 6 => name(f[4], false) && number(f[5], 0, false).is_some(),
        b"K1" if f.len() == 6 => name(f[4], true) && number(f[5], 0, false).is_some(),
        b"A1" if f.len() >= 7 => {
            name(f[4], false)
                && name(f[5], false)
                && f[6..].iter().map(|part| part.len()).sum::<usize>() + f.len() - 7 <= 200
                && module_payload(f[4], f[5], &f[6..].join(&b'|'))
        }
        _ => false,
    };
    if valid { Ok(()) } else { Err(invalid()) }
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn natural(value: u64) -> Result<(), String> {
    if (1..=MAX).contains(&value) {
        Ok(())
    } else {
        Err("checkpoint integer out of range".to_owned())
    }
}

/// Computes the initial digest of a log, including a held empty log.
pub fn checkpoint_initial(log_id: &str, format: u64) -> Result<String, String> {
    if !hex(log_id, 16) {
        return Err("invalid log ID".to_owned());
    }
    natural(format)?;
    Ok(digest_hex(&Sha256::digest(
        format!("PGL-CHECKPOINT|1|{log_id}|{format}\n").as_bytes(),
    )))
}

/// Advances the digest over one exact byte string and its resolved GUID.
pub fn checkpoint_next(
    previous: &str,
    sequence: u64,
    guid: &str,
    entry: &[u8],
) -> Result<String, String> {
    let valid_guid = guid.strip_prefix("Player-").is_some_and(|tail| {
        tail.split_once('-').is_some_and(|(server, id)| {
            !server.is_empty()
                && server.bytes().all(|b| b.is_ascii_digit())
                && !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
        })
    });
    if !hex(previous, 64) || !valid_guid || entry.contains(&b'\n') {
        return Err("invalid checkpoint digest, character GUID, or entry bytes".to_owned());
    }
    natural(sequence)?;
    let mut hash = Sha256::new();
    hash.update(format!(
        "PGL-ENTRY|1|{previous}|{sequence}|{}|{guid}|{}|",
        guid.len(),
        entry.len()
    ));
    hash.update(entry);
    hash.update(b"\n");
    Ok(digest_hex(&hash.finalize()))
}

fn digest_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vectors() -> serde_json::Value {
        serde_json::from_str(include_str!("../tests/fixtures/integrity-vectors.json")).unwrap()
    }

    #[test]
    fn canonical_core_records_agree_with_shared_adversarial_contract() {
        for case in vectors()["records"].as_array().unwrap() {
            assert_eq!(
                validate_record(case["entry"].as_str().unwrap().as_bytes()).is_ok(),
                case["valid"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn checkpoints_match_independently_derived_utf8_byte_frames() {
        let suite = vectors();
        let mut digest = checkpoint_initial(
            suite["logId"].as_str().unwrap(),
            suite["format"].as_u64().unwrap(),
        )
        .unwrap();
        assert_eq!(digest, suite["initial"].as_str().unwrap());
        for variant in suite["initialVariants"].as_array().unwrap() {
            assert_eq!(
                checkpoint_initial(
                    variant["logId"].as_str().unwrap(),
                    variant["format"].as_u64().unwrap()
                )
                .unwrap(),
                variant["digest"].as_str().unwrap()
            );
        }
        for variant in suite["entryVariants"].as_array().unwrap() {
            assert_eq!(
                checkpoint_next(
                    variant["previous"].as_str().unwrap(),
                    variant["sequence"].as_u64().unwrap(),
                    variant["guid"].as_str().unwrap(),
                    variant["entry"].as_str().unwrap().as_bytes()
                )
                .unwrap(),
                variant["digest"].as_str().unwrap()
            );
        }
        for step in suite["steps"].as_array().unwrap() {
            digest = checkpoint_next(
                &digest,
                step["sequence"].as_u64().unwrap(),
                step["guid"].as_str().unwrap(),
                step["entry"].as_str().unwrap().as_bytes(),
            )
            .unwrap();
            assert_eq!(digest, step["digest"].as_str().unwrap());
        }
    }
}

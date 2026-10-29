//! Private, authenticated Recovery. Packages are data, staged for one selected
//! install; SavedVariables remain exclusively the game's responsibility.
mod keys;
pub mod snapshot;
pub mod window;

use crate::in_game::folders::sha256_hex;
use crate::pagle_log::{self, Found, LocalLog};
use crate::upload::plan::End;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

pub const DATA_CAP: usize = 24 * 1024 * 1024;
pub const REPLY_CAP: usize = 33 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pending {
    #[serde(default)]
    pub staged_folder: Option<String>,
    #[serde(default)]
    pub package_digest: Option<String>,
    /// The local snapshot digest the package expects to replace. While the
    /// local log still has it, the addon hasn't applied the package.
    #[serde(default)]
    pub expected_digest: Option<String>,
    pub application_id: String,
    pub replacement_digest: String,
    pub accepted_seq: u64,
    pub staged: u64,
    pub instructions: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Signed {
    pub application_id: String,
    pub key_id: String,
    pub payload: String,
    pub signature: String,
}
pub struct Verified {
    pub pending: Pending,
    pub lua: Vec<u8>,
}

pub fn snapshot_digest(log: &LocalLog) -> String {
    sha256_hex(&snapshot::encode(log))
}

/// Reads the selected source using the non-executing SavedVariables parser.
/// Malformed LF-free entry bytes remain data for server-side staging.
pub fn read_source(folder: &str) -> Result<LocalLog, String> {
    let file = crate::installs::paths::lookup_path(
        std::path::Path::new(folder),
        &["SavedVariables", "Pagle.lua"],
    )
    .ok_or("This folder has no Pagle.lua")?;
    let bytes = crate::installs::paths::read_file(&file, crate::readings::read::PAGLE_LUA_CAP)
        .map_err(|_| "Couldn't read Pagle.lua")?;
    match pagle_log::parse_recovery(&bytes).map_err(|_| "Pagle.lua isn't SavedVariables data")? {
        Found::Valid(log)
            if log.format == 3 && log.characters.len() <= 100 && log.len() <= 100_000 =>
        {
            Ok(log)
        }
        _ => Err("Recovery needs a complete format-3 Catch log. Update Pagle first.".into()),
    }
}

pub fn verify(signed: Signed, local: &LocalLog, checkpoint: &End) -> Result<Verified, String> {
    let modulus = keys::modulus(&signed.key_id)
        .ok_or("Unknown Recovery signing key; update Pagle Companion")?;
    verify_with_modulus(signed, local, checkpoint, modulus)
}
fn verify_with_modulus(
    signed: Signed,
    local: &LocalLog,
    checkpoint: &End,
    modulus: &str,
) -> Result<Verified, String> {
    if signed.payload.len() > DATA_CAP.div_ceil(3) * 4
        || signed.signature.len() != 344
        || !snapshot::hex(&signed.application_id, 64)
    {
        return Err("Recovery package is invalid or too large".into());
    }
    let payload = STANDARD
        .decode(&signed.payload)
        .map_err(|_| "Invalid Recovery Base64")?;
    let signature = STANDARD
        .decode(&signed.signature)
        .map_err(|_| "Invalid Recovery signature encoding")?;
    if payload.len() > DATA_CAP || signature.len() != 256 {
        return Err("Recovery package exceeds its limits".into());
    }
    let n = (0..modulus.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&modulus[i..i + 2], 16).expect("shipped modulus"))
        .collect::<Vec<_>>();
    let mut message = format!("PGL-RECOVERY|1|{}|{}|", signed.key_id, payload.len()).into_bytes();
    message.extend_from_slice(&payload);
    ring::signature::RsaPublicKeyComponents {
        n: n.as_slice(),
        e: &[1, 0, 1],
    }
    .verify(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        &message,
        &signature,
    )
    .map_err(|_| "Recovery signature does not verify")?;
    let mut r = snapshot::Reader {
        rest: payload
            .strip_prefix(b"PGL-RECOVERY-DATA|1\n")
            .ok_or("Invalid Recovery data version")?,
    };
    let application_id = r.text()?;
    if application_id != signed.application_id || sha256_hex(r.rest) != application_id {
        return Err("Recovery application ID does not match its data".into());
    }
    if r.field()? != b"app.pagle.recovery"
        || r.text()? != local.log_id
        || r.number()? != local.format
    {
        return Err("Recovery belongs to another Catch log".into());
    }
    let seq = r.number()?;
    let digest = r.text()?;
    let revision = r.number()?;
    if seq != checkpoint.seq
        || Some(digest.as_str()) != checkpoint.digest.as_deref()
        || revision != checkpoint.revision
    {
        return Err("Recovery checkpoint changed; prepare Recovery again".into());
    }
    let rules = r.text()?;
    let expected_digest = r.text()?;
    if rules.is_empty()
        || rules.len() > 256
        || !snapshot::hex(&digest, 64)
        || expected_digest != snapshot_digest(local)
    {
        return Err("Recovery no longer matches the full local Catch log".into());
    }
    let replacement_digest = r.text()?;
    let replacement = r.field()?;
    if replacement_digest != sha256_hex(replacement) {
        return Err("Recovery replacement digest differs".into());
    }
    let restored = snapshot::decode(replacement)?;
    if restored.log_id != local.log_id
        || restored.format != local.format
        || seq > restored.len()
        || restored.checkpoint(seq).as_deref() != Some(digest.as_str())
    {
        return Err("Recovery replacement does not preserve Accepted history".into());
    }
    if restored.checkpoint(restored.len()).is_none() {
        return Err("Recovery names a missing character".into());
    }
    for index in 1..=restored.len() {
        let entry = restored.entry(index).expect("restored entry");
        if crate::integrity::validate_record(entry).is_err() {
            return Err("Recovery has a noncanonical replacement entry".into());
        }
    }
    let staged = r.number()?;
    if staged > local.len() {
        return Err("Too many staged Recovery records".into());
    }
    let mut previous = 0;
    for _ in 0..staged {
        let seq = r.number()?;
        if seq <= previous || seq > local.len() {
            return Err("Invalid staged Recovery sequence".into());
        }
        previous = seq;
        match r.field()? {
            b"action_requires_review"
            | b"source_invalid"
            | b"conflicting_observation"
            | b"source_order_incompatible"
            | b"character_mapping"
            | b"capacity_limit" => {}
            _ => return Err("Unknown Recovery staging reason".into()),
        }
    }
    r.finish()?;
    let lua = format!(
        "PagleRecoveryData = {{ v = 1, keyId = \"{}\", payload = \"{}\", sig = \"{}\" }}\n",
        signed.key_id, signed.payload, signed.signature
    )
    .into_bytes();
    Ok(Verified {
        pending: Pending {
            staged_folder: None,
            package_digest: Some(sha256_hex(&lua)),
            expected_digest: Some(expected_digest),
            application_id,
            replacement_digest,
            accepted_seq: seq,
            staged,
            instructions: String::new(),
        },
        lua,
    })
}

#[cfg(test)]
pub(crate) fn verify_test(
    signed: Signed,
    local: &LocalLog,
    checkpoint: &End,
) -> Result<Verified, String> {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../fixtures/recovery/staged-v1.json")).unwrap();
    if signed.key_id != "testKey" {
        return verify(signed, local, checkpoint);
    }
    verify_with_modulus(
        signed,
        local,
        checkpoint,
        fixture["modulus"].as_str().unwrap(),
    )
}

pub fn refusal_message(code: Option<&str>) -> &'static str {
    match code {
        Some("missing_held_log") => "Pagle has no Accepted history for this Catch log.",
        Some("missing_recovery" | "stale_checkpoint") => {
            "Accepted history or this Recovery plan changed. Prepare Recovery again."
        }
        Some("malformed_snapshot") => "This local Catch log is not a complete Recovery snapshot.",
        Some("too_large") => {
            "This Catch log is too large for automatic Recovery. Contact Pagle support."
        }
        Some("operator_review_required" | "invalid_accepted_history" | "inconsistent_history") => {
            "Pagle could not establish consistency automatically. Contact Pagle support for Operator review. Your history is preserved."
        }
        Some("recovery_not_uploaded") => {
            "Save the restored Catch log in game, then use Upload now to upload its complete history and check Recovery."
        }
        _ => {
            "Recovery is waiting for a successful Upload and current server validation. Upload again to retry the check."
        }
    }
}

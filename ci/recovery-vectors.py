#!/usr/bin/env python3
"""Rebuild public Recovery fixtures independently of the Rust implementation."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("backend", type=Path, help="Path to a pagle-backend checkout")
parser.add_argument("--format", type=int, help="Fail unless the backend's signed-v1.json is at this Catch log format")
parser.add_argument("--rules", help="Fail unless its B1 entry names this rule set (it keeps the one the backend wrote)")
args = parser.parse_args()
resources = args.backend / "src/test/resources"
output = Path(__file__).resolve().parent.parent / "src-tauri/fixtures/recovery"
key = resources / "pushed-content/signing/test-only-private-key.pem"
public_key = resources / "pushed-content/signing/test-key-public.pem"
SNAPSHOT_HEADER = b"PGL-SNAPSHOT|1\n"
DATA_HEADER = b"PGL-RECOVERY-DATA|1\n"
base = json.loads((resources / "recovery/signed-v1.json").read_text())
replacement = base64.b64decode(base["replacementSnapshot"])


def net(value):
    if isinstance(value, int):
        value = str(value)
    if isinstance(value, str):
        value = value.encode()
    return str(len(value)).encode() + b":" + value + b","


def netstrings(data):
    """Every netstring in DATA, in order; nothing may follow the last."""
    out, at = [], 0
    while at < len(data):
        colon = data.index(b":", at)
        length = int(data[at:colon])
        end = colon + 1 + length
        assert data[end:end + 1] == b",", "not a netstring at %d" % at
        out.append(data[colon + 1:end])
        at = end + 1
    return out


def sha(value):
    return hashlib.sha256(value).hexdigest()


def sign(fields, domain=b"PGL-RECOVERY|1|testKey|", trailing=b""):
    body = b"".join(map(net, fields)) + trailing
    identity = sha(body)
    payload = DATA_HEADER + net(identity) + body
    message = domain + str(len(payload)).encode() + b"|" + payload
    signature = subprocess.run(
        ["openssl", "dgst", "-sha256", "-sign", str(key)],
        input=message, capture_output=True, check=True,
    ).stdout
    return {
        "applicationId": identity,
        "keyId": "testKey",
        "payload": base64.b64encode(payload).decode(),
        "signature": base64.b64encode(signature).decode(),
    }


# The format, the rule set and the plan's own fields come from the backend's signed package, which is the
# authority for them: a pin bump (OPS-R131) re-signs it at another format and this follows. What it must still
# be is the one-entry plan the vectors below are built on, and consistent with itself. The staged pairs are this
# script's own: the backend's plan stages nothing, and these vectors add a local source record it must stage.
payload = base64.b64decode(base["payload"])
assert payload.startswith(DATA_HEADER), "signed-v1.json is not Recovery DATA"
data = netstrings(payload[len(DATA_HEADER):])
body_fields = data[1:]
assert data[0].decode() == sha(b"".join(map(net, body_fields))), "applicationId is not the SHA-256 of the body"
assert base["applicationId"] == data[0].decode(), "applicationId differs from the payload's"
(application, log_id, plan_format, accepted_seq, accepted_digest, accepted_revision, rules_version,
 source_digest, result_digest, signed_replacement, staged_count, *staged) = (field for field in body_fields)
assert application == b"app.pagle.recovery"
assert signed_replacement == replacement, "the package's replacement is not its replacementSnapshot"
assert sha(replacement) == result_digest.decode(), "the replacement does not match its signed digest"
assert accepted_digest.decode() == base["acceptedDigest"]
assert (int(accepted_seq), int(accepted_revision)) == (1, 1), "Update the fixture recipe for changed backend base data"
log_format = int(plan_format)
assert args.format in (None, log_format), "signed-v1.json is at format %d, not %d" % (log_format, args.format)
assert replacement.startswith(SNAPSHOT_HEADER)
snapshot = netstrings(replacement[len(SNAPSHOT_HEADER):])
assert snapshot[0] == log_id and int(snapshot[1]) == log_format, "the replacement is at another log or format than the plan"
characters = int(snapshot[3])
entries = snapshot[4 + 11 * characters:]
assert len(entries) == 2 and entries[0] == b"1", "Update the fixture recipe for changed backend base data"
entry = entries[1]
assert entry.split(b"|")[0] == b"B1"
rule_set = entry.split(b"|")[-1].decode()
assert args.rules in (None, rule_set), "its B1 entry names %s, not %s" % (rule_set, args.rules)
assert replacement.endswith(net(1) + net(entry))
prefix = replacement[:-len(net(1) + net(entry))]
source = prefix + net(2) + net(entry) + net(b"opaque broken source record")
fields = [
    application, log_id, log_format, 1,
    base["acceptedDigest"], 1, rules_version,
    sha(source), sha(replacement), replacement, 1, 2, "source_invalid",
]
modulus = subprocess.run(
    ["openssl", "rsa", "-pubin", "-in", str(public_key), "-modulus", "-noout"],
    capture_output=True, check=True,
).stdout.decode().strip().split("=")[1].lower()
# The local SavedVariables the plan was made from: the snapshot's characters, then the entries in the layout the
# format stores them in (one string each up to format 3, newline-joined blocks from format 4: LOG-R17).


def lua_nullable_bool(value):
    return {b"n": None, b"f": "false", b"t": "true"}[value]


def lua_character(at):
    guid, name, surname, class_id, race_id, faction, realm_id, hardcore, first, last, self_found = (
        snapshot[4 + 11 * at + i] for i in range(11))
    items = [("guid", '"%s"' % guid.decode()), ("name", '"%s"' % name.decode())]
    if surname != b"n":
        items.append(("surname", '"%s"' % surname[1:].decode()))
    items += [("classID", class_id.decode()), ("raceID", race_id.decode()), ("faction", '"%s"' % faction.decode()),
              ("realmID", realm_id.decode()), ("hardcore", lua_nullable_bool(hardcore)),
              ("firstSeen", first.decode()), ("lastSeen", last.decode()), ("selfFound", lua_nullable_bool(self_found))]
    return "{" + ", ".join("%s=%s" % item for item in items if item[1] is not None) + "}"


test_flag = lua_nullable_bool(snapshot[2])
local_entries = [entry.decode(), "opaque broken source record"]
if log_format >= 4:
    storage = 'blockSize = 256, count = %d, blocks = {"%s"}' % (len(local_entries), "\\n".join(local_entries))
else:
    storage = "entries = {%s}" % ", ".join('"%s"' % e for e in local_entries)
source_lua = 'PagleDB = { unknown = "preserve me" }\nPagleLog = { logId = "%s", format = %d,%s\ncharacters = {%s},\n%s }\n' % (
    log_id.decode(), log_format, "" if test_flag is None else " test = %s," % test_flag,
    ", ".join(lua_character(at) for at in range(characters)), storage)
fixture = {
    "signed": sign(fields),
    "sourceSnapshot": base64.b64encode(source).decode(),
    "replacementSnapshot": base["replacementSnapshot"],
    "sourceLua": source_lua,
    "acceptedDigest": base["acceptedDigest"],
    "modulus": modulus,
}
variants = {
    "pushed-content-domain": sign(fields, domain=b"PGL-CONTENT|1|testKey|"),
    "trailing-data": sign(fields, trailing=net("unexpected")),
}
changes = {
    "wrong-target": {1: "ffffffffffffffff"},
    "capacity-limit": {12: "capacity_limit"},
    "unknown-staging-reason": {12: "made_up_reason"},
    "wrong-checkpoint": {4: "0" * 64},
    "wrong-source-snapshot": {7: "0" * 64},
    "wrong-result-digest": {8: "0" * 64},
}
malformed = prefix + net(2) + net(entry) + net("malformed replacement")
changes["noncanonical-replacement"] = {8: sha(malformed), 9: malformed}
bad_boolean = replacement.replace(b"1:f,", b"1:0,", 1)
changes["noncanonical-boolean"] = {8: sha(bad_boolean), 9: bad_boolean}
for name, replacements in changes.items():
    changed = fields.copy()
    for index, value in replacements.items():
        changed[index] = value
    variants[name] = sign(changed)
output.mkdir(parents=True, exist_ok=True)
for name, value in [("staged-v1.json", fixture), ("adversarial-v1.json", variants)]:
    (output / name).write_text(json.dumps(value, indent=2) + "\n")
shutil.copyfile(resources / "recovery/snapshot-v1.json", output / "snapshot-v1.json")
print("Recovery fixtures at format %d, rule set %s" % (log_format, rule_set))

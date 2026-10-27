#!/usr/bin/env python3
"""Rebuild public Recovery fixtures independently of the Rust implementation."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("backend", type=Path, help="Path to a pagle-backend checkout")
args = parser.parse_args()
resources = args.backend / "src/test/resources"
output = Path(__file__).resolve().parent.parent / "src-tauri/fixtures/recovery"
key = resources / "pushed-content/signing/test-only-private-key.pem"
public_key = resources / "pushed-content/signing/test-key-public.pem"
base = json.loads((resources / "recovery/signed-v1.json").read_text())
replacement = base64.b64decode(base["replacementSnapshot"])


def net(value):
    if isinstance(value, int):
        value = str(value)
    if isinstance(value, str):
        value = value.encode()
    return str(len(value)).encode() + b":" + value + b","


def sha(value):
    return hashlib.sha256(value).hexdigest()


def sign(fields, domain=b"PGL-RECOVERY|1|testKey|", trailing=b""):
    body = b"".join(map(net, fields)) + trailing
    identity = sha(body)
    payload = b"PGL-RECOVERY-DATA|1\n" + net(identity) + body
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


entry = b"B1|1|1792422000|0|70338|1.60.1|pagle-2026a"
assert replacement.endswith(net(1) + net(entry)), "Update the fixture recipe for changed backend base data"
prefix = replacement[:-len(net(1) + net(entry))]
source = prefix + net(2) + net(entry) + net(b"opaque broken source record")
fields = [
    "app.pagle.recovery", "0123456789abcdef", 3, 1,
    base["acceptedDigest"], 1, "pagle-ranked-1-fixture",
    sha(source), sha(replacement), replacement, 1, 2, "source_invalid",
]
modulus = subprocess.run(
    ["openssl", "rsa", "-pubin", "-in", str(public_key), "-modulus", "-noout"],
    capture_output=True, check=True,
).stdout.decode().strip().split("=")[1].lower()
source_lua = '''PagleDB = { unknown = "preserve me" }
PagleLog = { logId = "0123456789abcdef", format = 3,
characters = {{guid="Player-4618-0A1B2C3D", name="Ada", surname="Brook", classID=1, raceID=1, faction="Alliance", realmID=4618, hardcore=false, firstSeen=1792422000, lastSeen=1793000000, selfFound=false}},
entries = {"B1|1|1792422000|0|70338|1.60.1|pagle-2026a", "opaque broken source record"} }
'''
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

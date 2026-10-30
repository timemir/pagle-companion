These are shared and independently signed Recovery v1 vectors.

- `snapshot-v1.json` is a byte-for-byte copy of backend `src/test/resources/recovery/snapshot-v1.json`; the same vectors exercise the actual addon exporter. Nil, false, and UTF-8 bytes remain distinct.
- `staged-v1.json` describes one accepted B1 and an opaque malformed local record, which remains staged as `source_invalid`. Its source Lua is data for the production SavedVariables parser.
- `adversarial-v1.json` uses valid RSA signatures over malformed or mismatched data, plus a signature from a different domain. `capacity-limit` is the valid control.

Regenerate all three files with `python3 ci/recovery-vectors.py ../pagle-backend --format 4` from the Companion root (`--format` and `--rules` only assert what the backend's `signed-v1.json` already says; the format, the rule set, the log and the checkpoint come from it, so a pin bump that re-signs it at another format needs no edit here). The Catch log format follows the accepted format (backend `recovery-v1.md`); the source Lua is written in that format's layout (`entries` up to format 3, `blocks` from format 4). The last two files are framed and hashed independently with Python's standard library and signed by OpenSSL using the backend's published test-only private key. `modulus` is that key's public modulus. The production trust ring contains only `pagle-2026a` and `pagle-2026b`; the explicitly enabled test harness supplies the fixture key.

The decoded source and replacement cap is 16 MiB. Signed DATA is capped at 24 MiB; its Base64 JSON response is capped at 33 MiB. Framing is defined by backend `docs/contracts/recovery-v1.md`.

These are shared and independently signed Recovery v1 vectors.

- `snapshot-v1.json` comes from backend `src/test/resources/recovery/snapshot-v1.json`; the same vectors exercise the actual addon exporter. Nil, false, and UTF-8 bytes remain distinct.
- `staged-v1.json` describes one accepted B1 and an opaque malformed local record, which remains staged as `source_invalid`. Its source Lua is data for the production SavedVariables parser.
- `adversarial-v1.json` uses valid RSA signatures over malformed or mismatched data, plus a signature from a different domain. `capacity-limit` is the valid control.

Regenerate the last two files with `python3 ci/recovery-vectors.py ../pagle-backend` from the Companion root. They are framed and hashed independently with Python's standard library and signed by OpenSSL using the backend's published test-only private key. `modulus` is that key's public modulus. The production trust ring contains only `pagle-2026a` and `pagle-2026b`; the explicitly enabled test harness supplies the fixture key.

The decoded source and replacement cap is 16 MiB. Signed DATA is capped at 24 MiB; its Base64 JSON response is capped at 33 MiB. Framing is defined by backend `docs/contracts/recovery-v1.md`.

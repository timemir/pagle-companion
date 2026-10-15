# pagle-companion

The Pagle Companion. Its spec lives in [`docs/spec/companion/`](https://github.com/timemir/pagle-backend/tree/main/docs/spec/companion) in `pagle-backend`.

## Development

The Companion is a [Tauri 2](https://v2.tauri.app/) tray app: a Rust core in `src-tauri/` and a small web UI in `ui/`, served as static files.

You need Rust (stable), Node.js LTS and [Tauri's prerequisites](https://v2.tauri.app/start/prerequisites/) for your platform: the MSVC build tools and WebView2 on Windows, or WebKitGTK 4.1, libayatana-appindicator, librsvg and libxdo on Linux.

```sh
npm ci                   # the Tauri CLI
npm run tauri dev        # run the tray app in development
cd src-tauri
cargo test               # every test cites the CMP or DST rule it checks
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

A debug build talks to `PAGLE_COMPANION_BASE_URL` when it is set, for a local backend; release builds always talk to `https://pagle.app`.

Tester packages come from the **Test build** workflow, started by hand on any branch.

`tauri.conf.json` sets `bundle.createUpdaterArtifacts`, so a local `npx tauri build` needs the updater signing key in `TAURI_SIGNING_PRIVATE_KEY` (and its password in `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`), or the tester builds' overlay, which turns the signatures off:

```sh
npx tauri build --config '{"bundle":{"createUpdaterArtifacts":false}}'
```

The release workflow's scripts in `ci/` have their own tests: `bash ci/test.sh` (needs git, jq and minisign).

## Releasing

A Release is published by `.github/workflows/release.yml` from an annotated version tag ([DST-R40 to R49](https://github.com/timemir/pagle-backend/blob/main/docs/spec/companion/distribution.md#releases)). A published Release is permanent: it is never edited or deleted, and a broken one is fixed forward.

1. Set the same `MAJOR.MINOR.PATCH` version, higher than the latest Release's and without a suffix, in `src-tauri/tauri.conf.json` and `src-tauri/Cargo.toml`, and merge that to `main`.
2. Tag that commit with the release notes as the tag's message. They become the Release's body and the updater's notes, and are final once pushed:

   ```sh
   git tag -a v1.2.3 -m "<release notes>"
   git push origin v1.2.3
   ```

The workflow checks the tag, builds both packages into a draft, verifies their updater signatures, writes `latest.json` and only then publishes.

**A failed run.** If `check` failed, fix the cause, delete the tag (`git push --delete origin v1.2.3`, `git tag -d v1.2.3`) and tag again. If `draft`, `build-windows` or `build-linux` failed, delete the draft Release and the tag, then push the tag again. Only `publish` and `winget` may be re-run in place ([DST-R44](https://github.com/timemir/pagle-backend/blob/main/docs/spec/companion/distribution.md#releases)).

**Procedures** in the spec:
- Defender flags a Release: submit it as a false positive ([DST-R24](https://github.com/timemir/pagle-backend/blob/main/docs/spec/companion/distribution.md#windows-channels), [Outside the code](https://github.com/timemir/pagle-backend/blob/main/docs/spec/companion/distribution.md#outside-the-code)).
- The updater key leaks ([DST-R26](https://github.com/timemir/pagle-backend/blob/main/docs/spec/companion/distribution.md#windows-channels)).
- Raising the minimum Companion version ([DST-R66](https://github.com/timemir/pagle-backend/blob/main/docs/spec/companion/distribution.md#version-gate)).

## Licence

The Companion is MIT-licensed. See [LICENSE](LICENSE).

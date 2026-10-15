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

## Licence

The Companion is MIT-licensed. See [LICENSE](LICENSE).

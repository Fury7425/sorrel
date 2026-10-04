# sorrel

Working name for a native desktop AI workspace: chats, projects, files, scheduled tasks and approvals, driven by the official `claude` and `codex` CLIs (plus Cursor, Gemini and OpenCode over ACP), so you use the subscriptions you already have. Rust and GPUI, no webview.

- `docs/ARCHITECTURE.md`: the design and the decision log.
- `docs/internals.md`: how it works now.
- `docs/PERF.md`: measurements.

## Run

```
cargo run --release -p sorrel                 # opens the window and runs the engine in-process
cargo run --release -p sorrel -- --daemon     # engine only, on a local socket; windows attach to it
cargo test --workspace
```

Sign in with each CLI's own login (`claude auth login`, `codex login`, or Settings > Sign in for Codex). Sorrel never reads or stores those tokens. Settings also takes API keys for API-key mode.

Building on Windows needs the MSVC toolchain and the Windows 10/11 SDK; Linux needs the packages listed in `.github/workflows/ci.yml`.

## Release

Push a tag such as `v0.1.0`. GitHub Actions builds Windows, macOS and Linux packages and publishes a release, signing them when the secrets listed in `scripts/package.sh` are set.

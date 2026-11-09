# Contributing

Thanks for considering a contribution!

## Build

```bash
cargo build --release          # any OS with a stable toolchain (1.85+)
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Pre-push hook (do this once per clone)

`master` only accepts green pushes — enable the bundled hook so fmt, clippy
and tests run BEFORE the push instead of failing in CI:

```bash
git config core.hooksPath .githooks
```

If a CI run still fails, read the failed STEP and the exact log line before
explaining it (a rustup-install script line in the log is not a failure).

Windows without MSVC Build Tools: run `bash scripts/fetch-tools.sh` once, then
prefix cargo commands with `source scripts/gnu-env.sh`.

## Adding a harness backend

Backends implement `trait Backend` in `src/backend/`. Two patterns exist:

- **server-backed** (`opencode.rs`): REST + SSE over an authenticated HTTP
  client; the app can autospawn the server;
- **CLI-backed** (`hermes.rs`, `dsh.rs`): spawn a subprocess per message,
  parse its NDJSON events, keep a local session store
  (`Arc<Mutex<…>>` + atomic writes — never a second copy of the truth).

Register the kind in `backend::create_backend` / `supported_kind`, add a
`[[harness]]` entry to the default config in `src/config.rs`, and follow the
event contract: all UI events carry the session id the UI already tracks;
`SessionCreated` announces real ids for rebinding. Verify with the headless
E2E mode: `cargo run -- --send "hello" --harness <id>`.

## Style

- Small commits with clear messages; keep `clippy -D warnings` green.
- User-visible strings go through `src/i18n.rs` (both RU and EN).
- The NC-era visual language (blue screen, silver dialogs, cyan frames,
  yellow F-keys) is intentional — match the existing theme.

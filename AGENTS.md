# Brim — Agent Guide

## Project overview

**Brim** is a pure-Rust package manager and app store for Fedora Linux. It
unifies three package sources behind a single async engine and exposes them
through three frontends in one `brim` binary:

- **Backends**: DNF5 (official Fedora RPMs), COPR (community projects, via
  the read-only COPR REST API for search/info and the `dnf copr` plugin for
  enable/disable), and Flatpak (Flathub).
- **Frontends**: a terminal CLI (default), a native GTK4/Libadwaita desktop
  app (`brim gui`), and a web dashboard with a REST API (`brim web`).

- Version `0.2.0`, Rust edition 2021, license **GPL-2.0-only**.
- Repository: <https://github.com/b4lol/brim>
- Target platform: Fedora Linux (developed on Fedora 44) with `dnf5`, `dnf`
  (COPR plugin), and `flatpak` installed. Missing tools degrade gracefully —
  unavailable backends are skipped rather than failing the whole operation.

## Technology stack

- **Language**: Rust (stable toolchain, pinned via `rust-toolchain.toml`;
  `rustfmt` and `clippy` components required).
- **Async runtime**: Tokio (multi-thread). The CLI and web frontend run on a
  tokio runtime; the GUI must **not** (GTK owns the main loop — see
  `src/main.rs`).
- **HTTP**: `reqwest` with rustls (pure-Rust TLS, no `curl` at runtime) for
  the COPR API, Flathub trending, and icon downloads — all funneled through
  one shared client in `src/core/http.rs` (30 s timeout, body size caps).
- **Web server**: `hyper` 1.x (HTTP/1) + `hyper-util`, serving an embedded
  SPA compiled into the binary from `static/` via `include_str!`.
- **GUI**: `gtk4` 0.11 (feature `v4_10`) and `libadwaita` 0.9 (feature
  `v1_5`) — requires system `gtk4-devel` and `libadwaita-devel` packages.
- **CLI**: `clap` 4 (derive), `colored`, `indicatif`.
- **Serialization**: `serde` / `serde_json` (config, sync export, REST API).
- **Errors**: `thiserror` (`BrimError` in `src/core/error.rs`).

## Build and test commands

The full verification suite (identical to CI, all four must pass):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

Install the binary locally:

```bash
cargo install --path . --locked
```

GUI build prerequisites (Fedora): `sudo dnf5 install gtk4-devel libadwaita-devel`.

CI (`.github/workflows/ci.yml`) runs on Ubuntu 24.04 (installs
`libgtk-4-dev libadwaita-1-dev`), plus a weekly scheduled run, and a second
job running `cargo audit` (RUSTSEC) and `cargo deny check licenses`.

## Code organization

Single binary crate (`src/main.rs` dispatches to one frontend):

```
src/
├── main.rs              Entry point: SIGPIPE fix, subcommand dispatch.
├── core/                Shared engine used by every frontend.
│   ├── backend.rs       Object-safe `Backend` trait (search/list/info/
│   │                    install/remove/updates/upgrade/repo mgmt).
│   ├── backends/
│   │   ├── mod.rs       Shared helpers: probe, validate_arg, timeouts.
│   │   ├── dnf5.rs      DNF5 backend (wraps the `dnf5` CLI).
│   │   ├── copr.rs      COPR backend (REST API + `dnf copr` plugin).
│   │   ├── flatpak.rs   Flatpak backend (wraps the `flatpak` CLI).
│   │   └── cache.rs     Backend result caching.
│   ├── manager.rs       `PackageManager`: fan-out across backends, merged
│   │                    results, tx_lock serializing transactions.
│   ├── models.rs        Package, SourceType, TransactionResult, SystemStats...
│   ├── config.rs        Shared JSON config (~/.config/brim/config.json).
│   ├── error.rs         BrimError / Result.
│   ├── http.rs          Shared reqwest client (rustls, timeouts, caps).
│   ├── sync.rs          Export/import of the installed set as JSON.
│   ├── trending.rs      Flathub popular collection (24 h disk cache).
│   └── fsutil.rs        Filesystem helpers.
├── cli/                 Terminal frontend (clap subcommands, tables,
│                        prompts, output sanitizing, banner).
├── gui/                 GTK4/Libadwaita frontend: window, rows, css, icons,
│                        worker (dedicated thread owning a tokio runtime;
│                        GUI ⇄ worker over async-channel).
├── web/                 Web frontend: hyper server (mod.rs), routes.rs,
│                        static_files.rs (embeds static/ into the binary).
static/                  SPA assets (index.html, style.css, app.js) embedded
                         at compile time — edit them, not generated copies.
```

Key runtime architecture facts:

- `PackageManager` fans read operations out to all **available** backends
  concurrently and tolerates individual backend failures (partial results,
  never a panic). Transactions (install/remove/upgrade) are serialized
  through `tx_lock` so two package-manager processes never mutate the system
  at once.
- Backends spawn external tools (`dnf5`, `dnf`, `flatpak`) with `LC_ALL=C`
  so output parsers see stable English headers. Queries use cache-only mode
  with fallback and a 20 s `QUERY_TIMEOUT`; transactions run without a
  timeout.
- The GUI never blocks its main loop: all engine work runs on a worker
  thread with its own tokio runtime (`src/gui/worker.rs`).
- Caches: `~/.cache/brim/trending.json` (24 h), `~/.cache/brim/icons/`.
- Config: `~/.config/brim/config.json`; unknown keys are preserved across
  load/save; sources disabled there are never constructed as backends.

## Code style guidelines

- Rust edition 2021, formatted with `rustfmt` defaults, clippy-clean with
  `-D warnings` (warnings are CI failures).
- **Parsers must be pure functions** over captured fixture strings, never
  executing real tools — this is what makes the test suite hermetic (see
  the inline `const ..._OUT: &str` fixtures in `src/core/backends/*.rs`).
- Doc comments (`//!`/`///`) explain *why*, especially around quirks:
  dnf5 exit code 100 for `check-update`, COPR search via REST API because
  the dnf plugin lacks `search`, SIGPIPE handling, etc.
- Keep changes minimal and focused; match the surrounding style. No new
  dependencies without need — `deny.toml` restricts dependency licenses
  (GPL-2.0-compatible allow-list, checked by `cargo deny` in CI).
- Errors: use `BrimError` variants; a real backend failure must never be
  disguised as `NotFound`.

## Testing instructions

- All tests are **inline** `#[cfg(test)]` modules next to the code they test
  (`#[test]` and `#[tokio::test]`; ~170 total). There is no top-level
  `tests/` directory.
- The suite is hermetic: no test spawns a real system command, so
  `cargo test --all-targets` runs anywhere, not just on Fedora.
- `PackageManager::with_backends` accepts mock backends for engine tests
  (see `MockBackend` in `src/core/manager.rs`).
- When changing a parser, add fixtures captured from real tool output.
- When changing behavior (commands, endpoints, models), update the README
  and this file.

## Security considerations

- **The web server binds 127.0.0.1 only — never widen this.** The localhost
  binding is the security boundary; `POST /api/*` endpoints perform real
  system transactions.
- Web API hardening: per-session random 128-bit token (`x-brim-token`
  header, constant-time comparison; printed to the TTY or written to a
  0600-permission file when stdout is piped), exact-host `Origin` check on
  mutating endpoints (cross-origin rejected with 403), loopback-host guard
  against DNS rebinding, 64 KiB request-body cap, 64-connection semaphore,
  30 s cache TTL on `/api/stats`.
- CLI install/remove/upgrade ask for confirmation unless `--yes`; the prompt
  names the resolved source before running the transaction.
- Backend argument injection: `validate_arg` rejects user input starting
  with `-` (dnf5 has no `--` end-of-options support); other backends pass
  `--` as defense in depth.
- HTTP response bodies are capped (16 MiB text, 8 MiB binary) so a hostile
  server cannot exhaust memory; all spawns use `kill_on_drop` so timed-out
  queries leave no orphan processes.
- CLI sanitizes terminal output (`src/cli/sanitize.rs`) before printing
  tool-provided strings.
- CI audits dependencies weekly with `cargo audit` (RUSTSEC) and enforces
  the license allow-list in `deny.toml`.

## Deployment

No packaging/release pipeline beyond CI. Users install from source with
`cargo install --path . --locked` (single `brim` binary; the SPA is embedded,
so no static assets need shipping). The release profile uses thin LTO,
`codegen-units = 1`, and stripped symbols.

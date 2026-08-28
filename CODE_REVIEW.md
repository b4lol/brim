# Code Review Checklist & Standards

This checklist outlines the quality standards and review guidelines for pull requests and code modifications in **Brim**.

---

## 📋 General Checklist

### 1. Code Quality & Rust Idioms
- [ ] Code is formatted with `cargo fmt --all`.
- [ ] Code compiles cleanly with zero warnings: `cargo clippy --all-targets -- -D warnings`.
- [ ] No `unwrap()` or `expect()` in production runtime paths without invariant justifications.
- [ ] Errors are properly categorized using `BrimError` (`src/core/error.rs`) rather than generic error types.
- [ ] Minimal memory allocations (prefer string slices `&str` and borrowing where appropriate).

### 2. Hermetic & Deterministic Testing
- [ ] Every new backend parser is implemented as a pure function over fixture strings.
- [ ] Unit tests use embedded `const ...: &str` fixtures representing real tool outputs.
- [ ] No tests execute real OS commands (`Command::new`) or perform live network requests.
- [ ] All tests pass via `cargo test --all-targets`.

### 3. Concurrency & Thread Safety
- [ ] GTK4 UI thread is never blocked with synchronous I/O or sleep operations.
- [ ] Tokio async operations run on appropriate runtimes (`src/gui/worker.rs` for GUI, main runtime for CLI/Web).
- [ ] Package mutating operations properly go through `PackageManager`'s `tx_lock` serialization.

### 4. Security & Safety
- [ ] User-supplied CLI arguments passed to external tools are validated using `validate_arg` (no leading `-`).
- [ ] Web API mutations validate `x-brim-token` using constant-time comparison.
- [ ] Web endpoints enforce loopback `Origin` and `Host` header constraints.
- [ ] New dependencies comply with the license allowlist in `deny.toml`.

### 5. Documentation & Metadata
- [ ] Public API functions and modules have doc comments explaining *why* quirks exist.
- [ ] Any user-facing CLI flag or config option is documented in `README.md`.
- [ ] Meaningful entries added to `CHANGELOG.md` when applicable.

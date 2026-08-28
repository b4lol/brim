# Contributing to Brim

Thank you for your interest in contributing to **Brim**!

Brim is a pure-Rust unified package manager and app store for Linux (Fedora and Debian/Ubuntu), licensed under the **GPL-2.0-only** license.

---

## 🛠️ Development Setup

### 1. Prerequisites

- **Rust toolchain**: Stable toolchain (managed via `rustup` and pinned in `rust-toolchain.toml`). Ensure `rustfmt` and `clippy` components are installed:
  ```bash
  rustup component add rustfmt clippy
  ```
- **System development libraries** (for GTK4/Libadwaita GUI):
  - **Fedora**: `sudo dnf5 install gtk4-devel libadwaita-devel`
  - **Debian / Ubuntu**: `sudo apt install libgtk-4-dev libadwaita-1-dev`
- **Optional / Recommended**: [just](https://github.com/casey/just) command runner.

### 2. Building & Local Installation

Clone the repository and build:

```bash
git clone https://github.com/b4lol/brim.git
cd brim
cargo build
```

Install locally using `just` (or cargo):

```bash
just install
# or manually:
cargo install --path . --locked
sudo install -m 0755 ~/.cargo/bin/brim /usr/local/bin/brim
```

> **Note**: Installing to `/usr/local/bin` allows `sudo brim ...` transactions to resolve properly when running privileged commands.

---

## 🧪 Verification & Testing Suite

Every contribution must pass the full verification suite before being merged. You can run the entire suite with:

```bash
just check
```

Or run the individual steps directly:

```bash
# 1. Code formatting
cargo fmt --all -- --check

# 2. Linter (no warnings allowed)
cargo clippy --all-targets -- -D warnings

# 3. Hermetic test suite
cargo test --all-targets

# 4. Release compilation
cargo build --release
```

---

## 📐 Code Guidelines & Architecture Rules

1. **Hermetic & Pure Parsers**:
   - All backend parsers (e.g. for `dnf5`, `apt`, `flatpak`, `copr`) **must be pure functions** operating over captured fixture strings.
   - Tests must never invoke live external commands or make real network calls. Use `const ..._OUT: &str` fixtures inline in test modules.
2. **Error Handling**:
   - Use `BrimError` variants located in [`src/core/error.rs`](src/core/error.rs).
   - Real backend errors must never be swallowed or disguised as `NotFound`.
3. **Threading & Concurrency**:
   - The CLI and Web server run within Tokio async runtimes.
   - The GTK4 GUI **must not** block its main thread. Heavy computations and async backend operations must run on the dedicated worker thread (`src/gui/worker.rs`) communicating via `async-channel`.
4. **Dependencies & Licensing**:
   - Maintain a minimal dependency tree.
   - All new dependencies must comply with the GPL-2.0-compatible allowlist in [`deny.toml`](deny.toml) (enforced by `cargo deny`).
5. **Security First**:
   - Validate CLI arguments against injection using [`validate_arg`](src/core/backends/mod.rs).
   - The Web server must remain bound exclusively to `127.0.0.1`. Mutating endpoints require `x-brim-token` and strict `Origin` header validation.

---

## 🔀 Submitting Pull Requests

1. Create a feature branch from `main` (`git checkout -b feature/my-feature`).
2. Implement your changes with accompanying unit tests.
3. Verify that `just check` passes with 0 warnings and 0 errors.
4. Commit with descriptive, conventional commit messages (e.g., `feat: add support for ...`, `fix: handle edge case in ...`, `docs: update ...`).
5. Push to your fork and submit a Pull Request against the `main` branch.

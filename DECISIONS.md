# Architecture Decision Records (ADRs)

This document records the key architectural and design decisions made throughout the development of **Brim**.

---

## ADR-001: Pure Parsers & Hermetic Testing

- **Status**: Accepted
- **Context**: Backend tools (`dnf5`, `apt`, `flatpak`) vary across systems and environments. Running live tool commands in CI or tests would make the test suite fragile, non-portable, and dependent on specific Linux distributions.
- **Decision**: All command output parsing functions must be pure functions operating on captured fixture strings. Tests must never spawn external processes or make network calls.
- **Consequences**: The test suite (185+ tests) runs anywhere within ~0.1s without root privileges or specific Linux distributions installed.

---

## ADR-002: Dedicated Worker Thread with Tokio Runtime for GTK4 GUI

- **Status**: Accepted
- **Context**: GTK4 / Libadwaita requires ownership of the main OS thread and event loop. Running Tokio async tasks directly inside the GTK main loop or blocking it during heavy I/O causes UI stuttering and unresponsiveness.
- **Decision**: The GUI spawns a dedicated background worker thread that owns its own multi-threaded Tokio runtime. Communication between the GTK UI and the worker uses lock-free `async-channel` queues.
- **Consequences**: The GUI maintains smooth 60+ FPS performance even during intensive package searches or multi-gigabyte downloads.

---

## ADR-003: Pure-Rust TLS with `rustls`

- **Status**: Accepted
- **Context**: Relying on OpenSSL or runtime `curl` creates external library dependencies and potential ABI mismatch issues across different Linux distributions.
- **Decision**: Standardize HTTP communication on `reqwest` configured with `rustls` and pure-Rust cryptography (`aws-lc-rs` / Ring).
- **Consequences**: No system `libssl` or `libcurl` runtime dependencies required; smaller attack surface and consistent TLS behavior across distributions.

---

## ADR-004: Localhost Loopback Security Boundary & Token Authorization for Web Server

- **Status**: Accepted
- **Context**: The embedded web server exposes endpoints capable of executing system transactions (`install`, `remove`, `upgrade`).
- **Decision**:
  - The server binds strictly to `127.0.0.1` (never `0.0.0.0`).
  - Mutating operations require a per-session random 128-bit `x-brim-token` header.
  - Strict loopback `Origin` and `Host` header checks are enforced to eliminate CSRF and DNS Rebinding vulnerabilities.
- **Consequences**: Robust security boundary ensuring web browsers or network attackers cannot trigger unauthorized system package installations.

---

## ADR-005: COPR REST API for Discovery and Plugin for Mutations

- **Status**: Accepted
- **Context**: Fedora's `dnf copr` command-line plugin provides enable/disable capabilities but lacks a search command.
- **Decision**: Use the official Fedora COPR read-only REST API for search and package inspection, while delegating repository activation/deactivation to the `dnf copr` plugin.
- **Consequences**: Full COPR project discovery capability without requiring unsupported CLI hacks.

---

## ADR-006: Asynchronous Mutex-Protected Transaction Serialization (`tx_lock`)

- **Status**: Accepted
- **Context**: Simultaneous package installations across threads or frontends can result in database locking errors (e.g. RPM `/var/lib/rpm/.rpm.lock` or DPKG lock).
- **Decision**: `PackageManager` wraps all mutating operations (`install`, `remove`, `upgrade`) with an asynchronous lock (`tx_lock`).
- **Consequences**: Transactions are strictly executed in sequence, preventing lock contention and system state corruption.

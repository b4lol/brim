# Security Policy

Brim executes system-level operations, manages repositories, and runs local HTTP and GUI interfaces. Security and stability are top priorities.

---

## 🔒 Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.2.x   | :white_check_mark: |
| < 0.2.0 | :x:                |

---

## 🛡️ Security Architecture & Protections

### 1. Local Web Daemon & REST API Hardening
- **Localhost Boundary**: The embedded web server binds **strictly to `127.0.0.1`** (loopback interface). It must never be exposed to public or local area network interfaces.
- **Session Token Authentication**: Mutating API endpoints (`POST /api/install`, `POST /api/remove`, `POST /api/upgrade`, etc.) require a secure, per-session 128-bit random token supplied in the `x-brim-token` header. Token verification uses constant-time comparison to prevent timing attacks.
- **CSRF & DNS Rebinding Protection**: Mutating endpoints enforce exact loopback `Host` and `Origin` header matching. Cross-origin requests from web browsers are rejected with `403 Forbidden`.
- **Resource Caps**:
  - Request body size capped at 64 KiB.
  - Connection semaphore limits active HTTP connections to 64.
  - Statistics cache TTL prevents resource exhaustion.

### 2. Argument Injection Prevention
- All user-supplied arguments destined for underlying tools (`dnf5`, `dnf`, `apt`, `flatpak`) are inspected by `validate_arg`.
- Leading dash/hyphen characters (`-`) are rejected to prevent flag injection attacks against tools that lack explicit end-of-options (`--`) delimiters.

### 3. Terminal Output Sanitization
- External package metadata, summaries, and descriptions are sanitized before terminal rendering (`src/cli/sanitize.rs`) to strip harmful ANSI escape codes and terminal control sequences.

### 4. Memory & Dependency Safety
- Pure Rust networking stack with `rustls` (no OpenSSL C dependency).
- HTTP response bodies are capped (16 MiB text, 8 MiB binary) to prevent OOM exhaustion.
- Automated CI dependency audits with `cargo audit` (RUSTSEC) and license compliance via `cargo deny`.

---

## 🚨 Reporting a Vulnerability

If you discover a security vulnerability in Brim:

1. **Do not create a public GitHub issue.**
2. Please disclose the vulnerability responsibly via private security advisories on GitHub or by emailing the project maintainer: `security@b4.lol` (or via maintainer profile).
3. Include detailed steps to reproduce the issue, proof of concept code (if applicable), and your assessment of the impact.
4. You will receive an acknowledgment within 48 hours, followed by updates as a patch is prepared and released.

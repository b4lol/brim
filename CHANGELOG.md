# Changelog

All notable changes to **Brim** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Planned
- Arch Linux (`pacman` / AUR) backend support.
- Snapcraft (`snapd`) integration.
- Graphical storage and disk utilization analyzer in GUI.
- Server-Sent Events (SSE) streaming for real-time transaction logs in Web UI.

---

## [0.2.0] - 2026-08-28

### Added
- **APT Backend**: Added support for Debian and Ubuntu systems using `apt-get`, `apt-cache`, and `dpkg-query`.
- **Desktop Application (`brim gui`)**: Native GTK4 and Libadwaita desktop app store with Trending, Updates, Installed, and Settings views.
- **Web Dashboard & REST API (`brim web`)**: Embedded glassmorphic web dashboard with session token authentication, Origin header verification, and per-source statistics.
- **Flathub Trending Integration**: 24-hour disk-cached trending app catalog from Flathub.
- **Numbered Search Cache (`brim install <#>`)**: Indexed search cache (`~/.cache/brim/last-search.json`) allowing quick numbered installation from CLI.
- **Sync & Backup**: Export and import installed package configurations via JSON.
- **Justfile Automation**: Local and system-wide installation recipes (`just install`, `just check`, `just completions`).
- **Comprehensive Verification Suite**: 185+ hermetic unit tests with pure parsers and full CI automation.

### Security
- Embedded web server strictly locked to `127.0.0.1`.
- Per-session 128-bit random token validation with constant-time comparison.
- DNS rebinding and cross-origin protection on all mutating endpoints.
- Input argument sanitizer and flag injection prevention (`validate_arg`).
- Pure-Rust TLS using `rustls` without runtime `curl` or OpenSSL dependencies.

---

## [0.1.0] - 2026-01-15

### Added
- Initial release of the unified package engine.
- DNF5 backend integration for Fedora Linux.
- COPR community repository discovery via REST API.
- Flatpak CLI wrapper and remote manager.
- Basic terminal CLI with search, install, remove, and upgrade operations.

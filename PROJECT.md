# Brim Project Overview

## 📌 Mission

Brim delivers a unified, performant, and secure package management and app store experience for Linux users and sysadmins. By abstracting DNF5, APT, COPR, and Flatpak behind an asynchronous Rust core, Brim eliminates the friction of juggling multiple package managers with differing syntaxes, flags, and configurations.

---

## 📊 Project Metadata

- **Name**: Brim
- **Current Version**: 0.2.0
- **Language**: Rust (Edition 2021, Stable)
- **License**: GNU General Public License v2.0 only (`GPL-2.0-only`)
- **Primary Repository**: <https://github.com/b4lol/brim>
- **Target OS Platforms**:
  - Fedora Linux (DNF5 + Flatpak + COPR)
  - Debian & Ubuntu Linux (APT + Flatpak)

---

## 🏛️ Ecosystem Structure

| Layer | Technologies / Crates | Primary Responsibilities |
|---|---|---|
| **Core Engine** | `tokio`, `futures`, `thiserror`, `serde` | Backend orchestration, caching, sync export, concurrency locks |
| **Backends** | DNF5, APT, COPR, Flatpak | Subprocess execution, output parsing, remote API queries |
| **CLI Frontend** | `clap`, `colored`, `indicatif`, `clap_complete` | Terminal UX, numbered search cache, interactive confirmations |
| **GUI Frontend** | `gtk4` (0.11), `libadwaita` (0.9), `async-channel` | Native GNOME desktop app, trending catalog, repository manager |
| **Web Frontend** | `hyper` (1.x), `hyper-util`, embedded SPA | Local browser dashboard, REST API, system statistics |

---

## 📚 Documentation Index

- [`README.md`](README.md): Quickstart, CLI syntax, and feature overview.
- [`CONTRIBUTING.md`](CONTRIBUTING.md): Guidelines for development and PR submissions.
- [`ARCHITECTURE.md`](ARCHITECTURE.md): Deep architectural details and diagrams.
- [`DECISIONS.md`](DECISIONS.md): Architectural Decision Records (ADRs).
- [`SECURITY.md`](SECURITY.md): Security policy and hardening mechanisms.
- [`ROADMAP.md`](ROADMAP.md): High-level feature roadmap.
- [`TODO.md`](TODO.md): Action items and task checklist.
- [`CHANGELOG.md`](CHANGELOG.md): Historical change log.
- [`CODE_REVIEW.md`](CODE_REVIEW.md): PR review checklist and quality criteria.

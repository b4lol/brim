# Brim Product Roadmap

This document outlines the strategic vision, feature milestones, and release targets for **Brim**.

---

## 🎯 Vision

To build the fastest, safest, and most intuitive unified Linux package manager and app store — combining the efficiency of native command-line tooling, the beauty of modern GNOME/Libadwaita desktop design, and the flexibility of lightweight web interfaces.

---

## 🗺️ Release Milestones

### Phase 1: Cross-Distribution Expansion (`v0.3.0`)
- [ ] **Arch Linux Backend**: Complete support for `pacman` and AUR helpers (`paru` / `yay`).
- [ ] **Snapcraft Support**: Optional Snap package discovery and management backend.
- [ ] **Dynamic Source Detection**: Automatic backend activation based on detected host OS capabilities.

### Phase 2: Observability & Interactive UI (`v0.4.0`)
- [ ] **Disk & Storage Analyzer**: Visual representation of disk consumption per package source.
- [ ] **Dependency Graph**: Interactive dependency tree inspection in GUI and CLI.
- [ ] **Real-time Log Streaming**: Server-Sent Events (SSE) streaming for long-running transactions in the Web Dashboard.
- [ ] **Desktop Notification Daemon**: Background checking for pending security updates.

### Phase 3: Enterprise Packaging & Production Readiness (`v1.0.0`)
- [ ] **Official Linux Packaging**: Ready-to-use `.rpm` (COPR / Fedora), `.deb` (PPA), and Flatpak bundles.
- [ ] **Transaction History & Rollback**: Comprehensive auditing of package changes with rollback capabilities where supported.
- [ ] **Declarative System Profiles**: Export and reproduce identical system packages across machines.
- [ ] **Plugin / Extensibility Architecture**: API for community backend plugins.

---

## 🔗 Related Documents
- Detailed task items and checkboxes: [`TODO.md`](TODO.md)
- Architectural details: [`ARCHITECTURE.md`](ARCHITECTURE.md)

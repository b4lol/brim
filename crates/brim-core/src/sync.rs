//! Portable package-list export/import ("sync files").
//!
//! A sync file is pretty JSON: `{ "version": 1, "entries": [...] }`.
//! Export covers the installed set; import reinstalls each entry from its
//! recorded source. Entries from COPR repos are indistinguishable from
//! official RPMs once installed, so sources are `FedoraOfficial` or
//! `Flatpak` in practice.

use serde::{Deserialize, Serialize};

use crate::models::{Package, SourceType};

/// Current sync file format version.
const SYNC_VERSION: u32 = 1;

/// One exported entry: enough to reinstall the package from its source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncEntry {
    pub id: String,
    pub source: SourceType,
    #[serde(default)]
    pub name: String,
}

#[derive(Serialize, Deserialize)]
struct SyncFile {
    version: u32,
    entries: Vec<SyncEntry>,
}

/// Serialize installed packages into a sync file (pure).
pub fn export_sync(packages: &[Package]) -> String {
    let file = SyncFile {
        version: SYNC_VERSION,
        entries: packages
            .iter()
            .map(|p| SyncEntry {
                id: p.id.clone(),
                source: p.source,
                name: p.name.clone(),
            })
            .collect(),
    };
    serde_json::to_string_pretty(&file).expect("sync serialization cannot fail")
}

/// Parse a sync file (pure, total): malformed JSON, newer versions and
/// empty ids yield nothing usable and are skipped.
pub fn parse_import(text: &str) -> Vec<SyncEntry> {
    let Ok(file) = serde_json::from_str::<SyncFile>(text) else {
        return Vec::new();
    };
    if file.version > SYNC_VERSION {
        return Vec::new();
    }
    file.entries
        .into_iter()
        .filter(|e| !e.id.trim().is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SourceType;

    #[test]
    fn export_import_round_trip() {
        let packages = vec![
            Package::new("htop.x86_64", "htop", SourceType::FedoraOfficial),
            Package::new("org.videolan.VLC", "VLC", SourceType::Flatpak),
        ];
        let text = export_sync(&packages);
        let entries = parse_import(&text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "htop.x86_64");
        assert_eq!(entries[0].source, SourceType::FedoraOfficial);
        assert_eq!(entries[1].id, "org.videolan.VLC");
        assert_eq!(entries[1].source, SourceType::Flatpak);
    }

    #[test]
    fn parse_import_is_total_on_garbage() {
        assert!(parse_import("").is_empty());
        assert!(parse_import("{not json").is_empty());
        assert!(parse_import(r#"{"version": 99, "entries": []}"#).is_empty());
        assert!(parse_import(
            r#"{"version": 1, "entries": [{"id": "  ", "source": "FedoraOfficial"}]}"#
        )
        .is_empty());
    }
}

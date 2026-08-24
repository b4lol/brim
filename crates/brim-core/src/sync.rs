//! Portable package-list export/import ("sync files").
//!
//! A sync file is pretty JSON: `{ "version": 1, "entries": [...] }`.
//! Export covers the installed set; import reinstalls each entry from its
//! recorded source. Entries from COPR repos are indistinguishable from
//! official RPMs once installed, so sources are `FedoraOfficial` or
//! `Flatpak` in practice.

use serde::{Deserialize, Serialize};

use crate::error::{BrimError, Result};
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

/// Parse a sync file. Malformed JSON and newer format versions are
/// errors so frontends can warn the user instead of silently importing
/// nothing; individual entries with empty ids are skipped.
pub fn parse_import(text: &str) -> Result<Vec<SyncEntry>> {
    let file = serde_json::from_str::<SyncFile>(text)
        .map_err(|e| BrimError::Parse(format!("invalid sync file: {e}")))?;
    if file.version > SYNC_VERSION {
        return Err(BrimError::Parse(format!(
            "sync file version {} is newer than supported version {SYNC_VERSION}",
            file.version
        )));
    }
    Ok(file
        .entries
        .into_iter()
        .filter(|e| !e.id.trim().is_empty())
        .collect())
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
        let entries = parse_import(&text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "htop.x86_64");
        assert_eq!(entries[0].source, SourceType::FedoraOfficial);
        assert_eq!(entries[1].id, "org.videolan.VLC");
        assert_eq!(entries[1].source, SourceType::Flatpak);
    }

    #[test]
    fn parse_import_rejects_malformed_and_newer_files() {
        assert!(parse_import("").is_err());
        assert!(parse_import("{not json").is_err());
        assert!(matches!(
            parse_import(r#"{"version": 99, "entries": []}"#),
            Err(BrimError::Parse(_))
        ));
    }

    #[test]
    fn parse_import_skips_empty_ids() {
        let entries = parse_import(
            r#"{"version": 1, "entries": [{"id": "  ", "source": "FedoraOfficial"}]}"#,
        )
        .unwrap();
        assert!(entries.is_empty());
    }
}

//! Core data models shared by all Brim frontends and backends.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Origin of a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceType {
    FedoraOfficial,
    Copr,
    Flatpak,
    Debian,
}

impl fmt::Display for SourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SourceType::FedoraOfficial => "Fedora",
            SourceType::Copr => "COPR",
            SourceType::Flatpak => "Flatpak",
            SourceType::Debian => "Debian",
        };
        write!(f, "{}", s)
    }
}

impl SourceType {
    /// CSS badge class used by frontends to style the source badge.
    pub fn badge_class(&self) -> &'static str {
        match self {
            SourceType::FedoraOfficial => "badge-fedora",
            SourceType::Copr => "badge-copr",
            SourceType::Flatpak => "badge-flatpak",
            SourceType::Debian => "badge-debian",
        }
    }
}

/// A configured package repository (flatpak remote or COPR repo).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInfo {
    /// Remote name (`flathub`) or COPR `owner/project`.
    pub id: String,
    /// Display title (falls back to `id`).
    pub title: String,
    /// Remote URL; empty for COPR repos.
    pub url: String,
    pub kind: RepoKind,
    pub enabled: bool,
}

/// Which repository system a [`RepoInfo`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoKind {
    FlatpakRemote,
    CoprRepo,
}

/// Installation state of a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum PackageStatus {
    Installed,
    #[default]
    Available,
    UpdateAvailable,
}

impl fmt::Display for PackageStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PackageStatus::Installed => "Installed",
            PackageStatus::Available => "Available",
            PackageStatus::UpdateAvailable => "Update Available",
        };
        write!(f, "{}", s)
    }
}

/// Application category, either set by a backend or guessed from metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Category {
    Development,
    Gaming,
    Productivity,
    System,
    Multimedia,
    Graphics,
    Utilities,
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Category::Development => "Development",
            Category::Gaming => "Gaming",
            Category::Productivity => "Productivity",
            Category::System => "System",
            Category::Multimedia => "Multimedia",
            Category::Graphics => "Graphics",
            Category::Utilities => "Utilities",
        };
        write!(f, "{}", s)
    }
}

impl Category {
    /// Heuristic keyword matcher used when a backend provides no category.
    ///
    /// Matches whole words: the lowercased `name + summary` text is split
    /// into tokens on non-alphanumeric characters, so e.g. "udev" does not
    /// match the "dev" keyword.
    pub fn guess(name: &str, summary: &str) -> Category {
        let text = format!("{} {}", name, summary).to_lowercase();
        let tokens: Vec<&str> = text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .collect();
        let has = |keywords: &[&str]| keywords.iter().any(|k| tokens.contains(k));
        if has(&["game"]) {
            Category::Gaming
        } else if has(&["compiler", "ide", "dev"]) {
            Category::Development
        } else if has(&["office", "note"]) {
            Category::Productivity
        } else if has(&["audio", "video", "player"]) {
            Category::Multimedia
        } else if has(&["image", "draw", "photo"]) {
            Category::Graphics
        } else if has(&["tool", "util"]) {
            Category::Utilities
        } else {
            Category::System
        }
    }
}

/// A package as seen by the user, unified across all backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub description: String,
    pub version: String,
    pub installed_version: Option<String>,
    pub source: SourceType,
    pub status: PackageStatus,
    pub category: Category,
    pub icon_name: String,
    pub size_mb: f64,
    pub rating: f32,
    pub downloads: u64,
    pub copr_owner: Option<String>,
    pub copr_project: Option<String>,
    pub flatpak_ref: Option<String>,
    /// Flatpak remote the package comes from (search results only).
    #[serde(default)]
    pub flatpak_remote: Option<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
}

impl Package {
    /// Create a package with sane defaults; backends fill in the rest.
    pub fn new(id: impl Into<String>, name: impl Into<String>, source: SourceType) -> Self {
        let name = name.into();
        Package {
            id: id.into(),
            category: Category::guess(&name, ""),
            name,
            summary: String::new(),
            description: String::new(),
            version: String::new(),
            installed_version: None,
            source,
            status: PackageStatus::Available,
            icon_name: "package-x-generic".to_string(),
            size_mb: 0.0,
            rating: 0.0,
            downloads: 0,
            copr_owner: None,
            copr_project: None,
            flatpak_ref: None,
            flatpak_remote: None,
            license: None,
            homepage: None,
        }
    }

    /// Recompute the category from the current name and summary. Backends
    /// call this after filling in the real summary, which is a better
    /// signal than the name alone used at construction time.
    pub fn refresh_category(&mut self) {
        self.category = Category::guess(&self.name, &self.summary);
    }
}

/// Aggregate statistics for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemStats {
    pub installed: usize,
    pub updates_pending: usize,
    pub sources: Vec<SourceStat>,
}

/// Per-source statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceStat {
    pub source: SourceType,
    pub installed: usize,
    pub updates: usize,
}

/// Kind of package operation performed by a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransactionAction {
    Install,
    Remove,
    Upgrade,
    /// A repository add/remove/enable/disable change.
    RepoChange,
}

/// Outcome of a package transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionResult {
    pub success: bool,
    pub action: TransactionAction,
    pub package_id: String,
    pub message: String,
    pub output: String,
}

impl TransactionResult {
    /// Successful transaction result.
    pub fn ok(
        action: TransactionAction,
        package_id: impl Into<String>,
        message: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        TransactionResult {
            success: true,
            action,
            package_id: package_id.into(),
            message: message.into(),
            output: output.into(),
        }
    }

    /// Failed transaction result.
    pub fn err(
        action: TransactionAction,
        package_id: impl Into<String>,
        message: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        TransactionResult {
            success: false,
            action,
            package_id: package_id.into(),
            message: message.into(),
            output: output.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_type_display_and_badge() {
        assert_eq!(SourceType::FedoraOfficial.to_string(), "Fedora");
        assert_eq!(SourceType::Copr.badge_class(), "badge-copr");
    }

    #[test]
    fn package_serde_roundtrip() {
        let pkg = Package::new("htop", "htop", SourceType::FedoraOfficial);
        let json = serde_json::to_string(&pkg).unwrap();
        let back: Package = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "htop");
        assert_eq!(back.source, SourceType::FedoraOfficial);
    }

    #[test]
    fn category_guess_matches_keywords() {
        assert_eq!(Category::guess("godot", "game engine"), Category::Gaming);
        assert_eq!(Category::guess("gcc", "C compiler"), Category::Development);
    }

    #[test]
    fn category_guess_matches_whole_words_only() {
        // "udev" contains the substring "dev" but must not be Development.
        assert_eq!(Category::guess("udev", "device manager"), Category::System);
        // "ideology" contains the substring "ide" but must not match either.
        assert_eq!(
            Category::guess("foo", "an ideology primer"),
            Category::System
        );
    }

    #[test]
    fn category_guess_multi_word_name() {
        assert_eq!(Category::guess("godot game engine", ""), Category::Gaming);
    }

    #[test]
    fn refresh_category_uses_summary() {
        let mut pkg = Package::new("godot", "godot", SourceType::FedoraOfficial);
        assert_eq!(pkg.category, Category::System);
        pkg.summary = "Game engine for 2D and 3D games".to_string();
        pkg.refresh_category();
        assert_eq!(pkg.category, Category::Gaming);
    }
}

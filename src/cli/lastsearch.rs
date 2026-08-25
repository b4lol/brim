//! Numbered-install cache: the result order of the last table-mode search,
//! persisted so `brim install <#>` can resolve a row number back to the
//! exact package the user saw.

use std::path::{Path, PathBuf};

use crate::core::{BrimError, Package, Result};

/// The cache file: `~/.cache/brim/last-search.json`.
fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("brim").join("last-search.json"))
}

/// Persist the displayed result order. Best effort: a failed write only
/// means number-install is unavailable, never a search failure.
pub async fn save(packages: &[Package]) {
    let Some(path) = cache_path() else {
        return;
    };
    save_to(&path, packages).await;
}

/// Write the cache to an explicit path (split out for tests).
async fn save_to(path: &Path, packages: &[Package]) {
    let Ok(text) = serde_json::to_string(packages) else {
        return;
    };
    let _ = crate::core::fsutil::write_atomic(path, &text).await;
}

/// The package shown as result `number` (1-based) in the last search.
pub async fn package_at(number: usize) -> Result<Package> {
    let path = cache_path()
        .ok_or_else(|| BrimError::InvalidInput("cannot determine cache directory".to_string()))?;
    let packages = load_from(&path).await;
    pick(&packages, number)
}

/// Read the cache from an explicit path; missing or corrupt files yield
/// an empty list (number-install then reports "no cached results").
async fn load_from(path: &Path) -> Vec<Package> {
    let text = tokio::fs::read_to_string(path).await.unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_default()
}

/// Pick result `number` (1-based) out of `packages`, with actionable
/// errors for the two ways a number cannot resolve.
fn pick(packages: &[Package], number: usize) -> Result<Package> {
    if packages.is_empty() {
        return Err(BrimError::InvalidInput(
            "no cached search results — run 'brim search <query>' first, or install by package id"
                .to_string(),
        ));
    }
    packages
        .get(number.wrapping_sub(1))
        .cloned()
        .ok_or_else(|| {
            BrimError::InvalidInput(format!(
                "result #{number} is out of range (last search has {} results)",
                packages.len()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SourceType;

    fn pkg(name: &str) -> Package {
        Package::new(name, name, SourceType::FedoraOfficial)
    }

    #[test]
    fn pick_resolves_one_based_numbers() {
        let packages = vec![pkg("a"), pkg("b"), pkg("c")];
        assert_eq!(pick(&packages, 1).unwrap().name, "a");
        assert_eq!(pick(&packages, 3).unwrap().name, "c");
    }

    #[test]
    fn pick_rejects_zero_and_overflow() {
        let packages = vec![pkg("a")];
        assert!(pick(&packages, 0).is_err());
        assert!(pick(&packages, 2).is_err());
    }

    #[test]
    fn pick_explains_missing_cache() {
        let err = pick(&[], 1).unwrap_err().to_string();
        assert!(err.contains("brim search"), "unexpected message: {err}");
    }

    #[tokio::test]
    async fn save_then_load_round_trips() {
        let path =
            std::env::temp_dir().join(format!("brim-lastsearch-{}.json", std::process::id()));
        save_to(&path, &[pkg("one"), pkg("two")]).await;
        let loaded = load_from(&path).await;
        assert_eq!(loaded.len(), 2);
        assert_eq!(pick(&loaded, 2).unwrap().name, "two");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn load_tolerates_missing_and_corrupt_files() {
        let missing =
            std::env::temp_dir().join(format!("brim-lastsearch-x-{}.json", std::process::id()));
        assert!(load_from(&missing).await.is_empty());
        tokio::fs::write(&missing, "{nope").await.unwrap();
        assert!(load_from(&missing).await.is_empty());
        let _ = std::fs::remove_file(&missing);
    }
}

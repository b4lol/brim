//! Query-keyed disk cache for slow backend searches.
//!
//! Some search paths are slow for reasons outside Brim's control (the
//! COPR `project/search` endpoint takes ~9 s server-side; `flatpak
//! search` spends ~4-6 s in the system helper). Search results change
//! slowly, so repeat searches are served from a short-lived cache under
//! `~/.cache/brim/<namespace>/`, keyed by a hash of the query.

use std::path::PathBuf;
use std::time::Duration;

/// The cache path for `key`, or `None` when no per-user cache directory
/// can be determined (no `XDG_CACHE_HOME`/`HOME` — same no-`/tmp`-fallback
/// rule as the trending cache). The key is hashed so arbitrary search
/// text never becomes a file name.
fn path(namespace: &str, key: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    Some(
        base.join("brim")
            .join(namespace)
            .join(format!("{:016x}.json", hasher.finish())),
    )
}

/// Read a fresh cached response; `None` when there is no cache path, the
/// entry is stale, unreadable, or `valid` rejects it (e.g. a body that
/// parses to an empty list counts as corrupt, same rule as the trending
/// cache).
pub(crate) async fn read(
    namespace: &str,
    key: &str,
    ttl: Duration,
    valid: impl Fn(&str) -> bool,
) -> Option<String> {
    let path = path(namespace, key)?;
    let metadata = tokio::fs::metadata(&path).await.ok()?;
    let fresh = metadata
        .modified()
        .ok()?
        .elapsed()
        .map(|age| age < ttl)
        .unwrap_or(false);
    if !fresh {
        return None;
    }
    let text = tokio::fs::read_to_string(&path).await.ok()?;
    if !valid(&text) {
        return None;
    }
    Some(text)
}

/// Best-effort write of a response to the cache.
pub(crate) async fn write(namespace: &str, key: &str, text: &str) {
    if let Some(path) = path(namespace, key) {
        let _ = crate::core::fsutil::write_atomic(&path, text).await;
    }
}

/// Best-effort removal of a cache entry (used to invalidate results a
/// transaction just made stale).
pub(crate) async fn remove(namespace: &str, key: &str) {
    if let Some(path) = path(namespace, key) {
        let _ = tokio::fs::remove_file(&path).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_path_is_query_keyed_and_namespaced() {
        let a = path("test-ns", "htop").unwrap();
        let b = path("test-ns", "htop").unwrap();
        let c = path("test-ns", "vlc").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.to_string_lossy().contains("test-ns"));
        assert!(a.extension().is_some_and(|e| e == "json"));
        // The raw query must never appear in the file name.
        let weird = path("test-ns", "../etc/passwd").unwrap();
        assert!(!weird.to_string_lossy().contains("passwd"));
    }
}

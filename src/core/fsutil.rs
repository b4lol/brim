//! Small filesystem helpers shared across the crate.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::core::error::Result;

/// Write `contents` to `path` atomically (temp file + rename), creating
/// parent directories as needed. This is the variant async callers must
/// use so the executor is never blocked.
pub(crate) async fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = tmp_path(path);
    tokio::fs::write(&tmp, contents).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Blocking variant of [`write_atomic`] for sync callers (e.g. the CLI's
/// config commands).
pub(crate) fn write_atomic_blocking(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path(path);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The temp file a [`write_atomic`] goes through before the rename.
///
/// The temp file sits next to the target (rename(2) requires the same
/// filesystem) and gets a unique per-process suffix, so concurrent
/// writers never collide — even two writers targeting the *same* file
/// (each renames its own temp; the last rename wins, but neither ever
/// captures the other's partially written temp file).
fn tmp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmp_path_sits_next_to_the_target() {
        let tmp = tmp_path(Path::new("/tmp/x/config.json"));
        assert_eq!(tmp.parent(), Some(Path::new("/tmp/x")));
        let name = tmp.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("config.json.tmp."));
    }

    #[test]
    fn tmp_path_is_unique_per_call() {
        let a = tmp_path(Path::new("/tmp/x/config.json"));
        let b = tmp_path(Path::new("/tmp/x/config.json"));
        assert_ne!(a, b);
    }
}

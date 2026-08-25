//! Object-safe abstraction over package management backends.

use async_trait::async_trait;

use crate::core::error::BrimError;
use crate::core::models::{Package, RepoInfo, SourceType, TransactionResult};

/// A package management backend (dnf5, COPR, Flatpak, ...).
///
/// The trait is object-safe so the manager can hold `Box<dyn Backend>`.
#[async_trait]
pub trait Backend: Send + Sync {
    /// The package source this backend manages.
    fn source(&self) -> SourceType;

    /// Whether the backend's underlying tool is usable on this system.
    async fn is_available(&self) -> bool;

    /// Search for packages matching `query`.
    async fn search(&self, query: &str) -> Result<Vec<Package>, BrimError>;

    /// List packages installed via this backend.
    async fn list_installed(&self) -> Result<Vec<Package>, BrimError>;

    /// Get detailed information about the package with the given id.
    async fn info(&self, id: &str) -> Result<Package, BrimError>;

    /// Install a package.
    async fn install(&self, pkg: &Package) -> Result<TransactionResult, BrimError>;

    /// Remove an installed package.
    async fn remove(&self, pkg: &Package) -> Result<TransactionResult, BrimError>;

    /// List packages with pending updates.
    async fn updates(&self) -> Result<Vec<Package>, BrimError>;

    /// Upgrade all packages managed by this backend.
    async fn upgrade(&self) -> Result<TransactionResult, BrimError>;

    /// List configured repositories (default: none).
    async fn list_repos(&self) -> Result<Vec<RepoInfo>, BrimError> {
        Ok(vec![])
    }

    /// Add a repository (default: unsupported).
    async fn add_repo(&self, id: &str, url: &str) -> Result<TransactionResult, BrimError> {
        let _ = (id, url);
        Err(BrimError::BackendUnavailable("repo management".to_string()))
    }

    /// Remove a repository (default: unsupported).
    async fn remove_repo(&self, id: &str) -> Result<TransactionResult, BrimError> {
        let _ = id;
        Err(BrimError::BackendUnavailable("repo management".to_string()))
    }

    /// Enable or disable a repository (default: unsupported).
    async fn set_repo_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<TransactionResult, BrimError> {
        let _ = (id, enabled);
        Err(BrimError::BackendUnavailable("repo management".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_trait_is_object_safe() {
        fn assert_obj(_: &dyn Backend) {}
        // compiles only if Backend is object safe
        let _ = assert_obj;
    }
}

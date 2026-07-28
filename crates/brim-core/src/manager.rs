//! Unified package manager engine tying all backends together.
//!
//! [`PackageManager`] is the API surface used by the CLI, GUI, and web
//! frontends. It fans read operations (search, list, updates) out to all
//! available backends concurrently and tolerates individual backend
//! failures: a broken backend yields partial results, never a panic.

use futures::future::join_all;

use crate::backend::Backend;
use crate::backends::{copr::CoprBackend, dnf5::Dnf5Backend, flatpak::FlatpakBackend};
use crate::error::BrimError;
use crate::models::{
    Package, PackageStatus, RepoInfo, SourceStat, SourceType, SystemStats, TransactionAction,
    TransactionResult,
};
use crate::sync::SyncEntry;
use crate::Result;

/// The unified engine that routes operations across all backends.
pub struct PackageManager {
    backends: Vec<Box<dyn Backend>>,
    http: reqwest::Client,
}

impl Default for PackageManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PackageManager {
    /// Create a manager with the standard backends, honoring the shared
    /// config: a disabled source's backend is never constructed, so
    /// fan-out reads skip it entirely and transactions targeting it fail
    /// routing like any unknown source.
    pub fn new() -> Self {
        Self::from_config(&crate::config::Config::load())
    }

    /// Create a manager over the standard backends enabled in `config`.
    pub fn from_config(config: &crate::config::Config) -> Self {
        let mut backends: Vec<Box<dyn Backend>> = Vec::new();
        if config.sources.dnf5 {
            backends.push(Box::new(Dnf5Backend::new()));
        }
        if config.sources.copr {
            backends.push(Box::new(CoprBackend::new()));
        }
        if config.sources.flatpak {
            backends.push(Box::new(FlatpakBackend::new()));
        }
        Self::with_backends(backends)
    }

    /// Create a manager over an explicit set of backends (used in tests).
    pub fn with_backends(backends: Vec<Box<dyn Backend>>) -> Self {
        PackageManager {
            backends,
            http: crate::http::client(),
        }
    }

    /// Search all available backends concurrently and merge the results.
    ///
    /// When `source` is `Some`, only that backend is queried. Failed
    /// backends are skipped. Results are sorted: exact-name matches to
    /// `query` first, then by rating descending, then by name ascending.
    pub async fn search(&self, query: &str, source: Option<SourceType>) -> Vec<Package> {
        let backends = self.active_backends(source).await;
        let results = join_all(backends.iter().map(|b| b.search(query))).await;
        let mut packages: Vec<Package> = results
            .into_iter()
            .filter_map(std::result::Result::ok)
            .flatten()
            .collect();
        sort_search_results(query, &mut packages);
        packages
    }

    /// List installed packages across all available backends.
    pub async fn list_installed(&self) -> Vec<Package> {
        let backends = self.active_backends(None).await;
        let results = join_all(backends.iter().map(|b| b.list_installed())).await;
        merge_sorted(results)
    }

    /// List packages with pending updates across all available backends.
    pub async fn updates(&self) -> Vec<Package> {
        let backends = self.active_backends(None).await;
        let results = join_all(backends.iter().map(|b| b.updates())).await;
        merge_sorted(results)
    }

    /// Get package details; the first backend hit wins.
    pub async fn info(&self, id: &str, source: Option<SourceType>) -> Result<Package> {
        for backend in self.active_backends(source).await {
            if let Ok(pkg) = backend.info(id).await {
                return Ok(pkg);
            }
        }
        Err(BrimError::NotFound(id.to_string()))
    }

    /// Install a package, routing to the backend that owns it.
    pub async fn install(&self, id: &str, source: Option<SourceType>) -> Result<TransactionResult> {
        let pkg = self.resolve(id, source).await?;
        self.backend_for(pkg.source).await?.install(&pkg).await
    }

    /// Remove an installed package, routing to the backend that owns it.
    pub async fn remove(&self, id: &str, source: Option<SourceType>) -> Result<TransactionResult> {
        let pkg = self.resolve(id, source).await?;
        self.backend_for(pkg.source).await?.remove(&pkg).await
    }

    /// List repositories across all available backends (flatpak remotes
    /// and COPR repos). Failed backends are skipped.
    pub async fn list_repos(&self) -> Vec<RepoInfo> {
        let backends = self.active_backends(None).await;
        let results = join_all(backends.iter().map(|b| b.list_repos())).await;
        results
            .into_iter()
            .filter_map(std::result::Result::ok)
            .flatten()
            .collect()
    }

    /// Add a flatpak remote (`flatpak remote-add --user`).
    pub async fn add_flatpak_remote(&self, name: &str, url: &str) -> Result<TransactionResult> {
        self.backend_for(SourceType::Flatpak)
            .await?
            .add_repo(name, url)
            .await
    }

    /// Delete a flatpak remote (`flatpak remote-delete`).
    pub async fn remove_flatpak_remote(&self, name: &str) -> Result<TransactionResult> {
        self.backend_for(SourceType::Flatpak)
            .await?
            .remove_repo(name)
            .await
    }

    /// Enable or disable a COPR repo (`owner/project`).
    pub async fn set_copr_repo_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<TransactionResult> {
        self.backend_for(SourceType::Copr)
            .await?
            .set_repo_enabled(id, enabled)
            .await
    }

    /// Resolve a user-facing id to a package.
    ///
    /// Prefers `info`, but some backends (notably flatpak, where `flatpak
    /// info` only covers installed refs) report `NotFound` for packages
    /// that search can see. On `NotFound`, fall back to an exact search
    /// hit — case-insensitive on `id`, or an exact `flatpak_ref` match —
    /// before giving up.
    pub async fn resolve(&self, id: &str, source: Option<SourceType>) -> Result<Package> {
        match self.info(id, source).await {
            Ok(pkg) => Ok(pkg),
            Err(not_found @ BrimError::NotFound(_)) => {
                let hit = self.search(id, source).await.into_iter().find(|p| {
                    p.id.eq_ignore_ascii_case(id) || p.flatpak_ref.as_deref() == Some(id)
                });
                hit.ok_or(not_found)
            }
            Err(e) => Err(e),
        }
    }

    /// Upgrade packages on all available backends and merge the output.
    ///
    /// Every backend is attempted even if some fail; the merged result is
    /// successful only if all backends succeeded.
    pub async fn upgrade(&self) -> Result<TransactionResult> {
        let backends = self.active_backends(None).await;
        let results = join_all(backends.iter().map(|b| b.upgrade())).await;

        let mut success = true;
        let mut output = String::new();
        for result in results {
            match result {
                Ok(tr) => {
                    success &= tr.success;
                    push_section(&mut output, &tr.message, &tr.output);
                }
                Err(e) => {
                    success = false;
                    push_section(&mut output, "backend error", &e.to_string());
                }
            }
        }

        let message = if success {
            "all backends upgraded successfully"
        } else {
            "one or more backends failed to upgrade"
        };
        Ok(TransactionResult {
            success,
            action: TransactionAction::Upgrade,
            package_id: "*".to_string(),
            message: message.to_string(),
            output,
        })
    }

    /// Export the installed set as a sync file (JSON).
    pub async fn export_sync(&self) -> String {
        crate::sync::export_sync(&self.list_installed().await)
    }

    /// Install every entry of a sync file sequentially; one result per
    /// entry, in file order (a failure never stops the batch).
    pub async fn import_sync(&self, entries: Vec<SyncEntry>) -> Vec<TransactionResult> {
        let mut results = Vec::new();
        for entry in entries {
            let result = self
                .install(&entry.id, Some(entry.source))
                .await
                .unwrap_or_else(|e| {
                    TransactionResult::err(
                        TransactionAction::Install,
                        &entry.id,
                        "install failed",
                        e.to_string(),
                    )
                });
            results.push(result);
        }
        results
    }

    /// Aggregate dashboard statistics across all available backends.
    pub async fn system_stats(&self) -> SystemStats {
        // The two fan-outs are independent — run them concurrently.
        let (installed, updates) = tokio::join!(self.list_installed(), self.updates());

        // Emit one SourceStat per known source, in a stable order.
        let sources = [
            SourceType::FedoraOfficial,
            SourceType::Copr,
            SourceType::Flatpak,
        ]
        .into_iter()
        .map(|source| SourceStat {
            source,
            installed: installed.iter().filter(|p| p.source == source).count(),
            updates: updates.iter().filter(|p| p.source == source).count(),
        })
        .collect();

        SystemStats {
            installed: installed.len(),
            updates_pending: updates.len(),
            sources,
        }
    }

    /// Flathub's most popular apps (~24h disk cache), with installed
    /// status marked by intersecting the installed list.
    pub async fn trending(&self) -> Vec<Package> {
        let (mut trending, installed) =
            tokio::join!(crate::trending::trending(&self.http), self.list_installed());
        for pkg in &mut trending {
            if installed.iter().any(|p| {
                p.source == SourceType::Flatpak
                    && (p.flatpak_ref.as_deref() == Some(pkg.id.as_str()) || p.id == pkg.id)
            }) {
                pkg.status = PackageStatus::Installed;
            }
        }
        trending
    }

    /// Available backends, filtered by `source` when given.
    async fn active_backends(&self, source: Option<SourceType>) -> Vec<&dyn Backend> {
        join_all(self.backends.iter().map(|b| async {
            match source {
                Some(s) if b.source() != s => None,
                _ if b.is_available().await => Some(b.as_ref()),
                _ => None,
            }
        }))
        .await
        .into_iter()
        .flatten()
        .collect()
    }

    /// The single available backend managing `source`, if any.
    async fn backend_for(&self, source: SourceType) -> Result<&dyn Backend> {
        self.active_backends(Some(source))
            .await
            .into_iter()
            .next()
            .ok_or_else(|| BrimError::BackendUnavailable(source.to_string()))
    }
}

/// Merge per-backend list results, skipping failures, sorted by name for
/// deterministic output.
fn merge_sorted(results: Vec<Result<Vec<Package>>>) -> Vec<Package> {
    let mut packages: Vec<Package> = results
        .into_iter()
        .filter_map(std::result::Result::ok)
        .flatten()
        .collect();
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    packages
}

/// Sort search hits: exact-name matches first, then rating descending,
/// then name ascending.
fn sort_search_results(query: &str, packages: &mut [Package]) {
    let query = query.to_lowercase();
    packages.sort_by(|a, b| {
        let exact_a = a.name.to_lowercase() == query;
        let exact_b = b.name.to_lowercase() == query;
        exact_b
            .cmp(&exact_a)
            .then_with(|| {
                b.rating
                    .partial_cmp(&a.rating)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.name.cmp(&b.name))
    })
}

/// Append a labelled section to merged command output.
fn push_section(buffer: &mut String, title: &str, body: &str) {
    if !buffer.is_empty() {
        buffer.push('\n');
    }
    buffer.push_str(&format!("== {title} ==\n{body}\n"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::PackageStatus;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;

    /// Backend returning canned data; counts how often each method is called.
    struct MockBackend {
        source: SourceType,
        available: bool,
        fail: bool,
        search_results: Vec<Package>,
        installed: Vec<Package>,
        updates: Vec<Package>,
        info_not_found: bool,
        search_calls: Arc<AtomicUsize>,
        install_calls: Arc<AtomicUsize>,
        upgrade_calls: Arc<AtomicUsize>,
    }

    impl MockBackend {
        fn new(source: SourceType) -> Self {
            MockBackend {
                source,
                available: true,
                fail: false,
                search_results: Vec::new(),
                installed: Vec::new(),
                updates: Vec::new(),
                info_not_found: false,
                search_calls: Arc::new(AtomicUsize::new(0)),
                install_calls: Arc::new(AtomicUsize::new(0)),
                upgrade_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn err<T>(&self) -> Result<T> {
            Err(BrimError::CommandFailed(format!(
                "{} backend broke",
                self.source
            )))
        }
    }

    #[async_trait]
    impl Backend for MockBackend {
        fn source(&self) -> SourceType {
            self.source
        }

        async fn is_available(&self) -> bool {
            self.available
        }

        async fn search(&self, _query: &str) -> Result<Vec<Package>> {
            self.search_calls.fetch_add(1, AtomicOrdering::SeqCst);
            if self.fail {
                self.err()
            } else {
                Ok(self.search_results.clone())
            }
        }

        async fn list_installed(&self) -> Result<Vec<Package>> {
            if self.fail {
                self.err()
            } else {
                Ok(self.installed.clone())
            }
        }

        async fn info(&self, id: &str) -> Result<Package> {
            if self.fail {
                return self.err();
            }
            if self.info_not_found {
                return Err(BrimError::NotFound(id.to_string()));
            }
            self.search_results
                .iter()
                .chain(self.installed.iter())
                .find(|p| p.id == id)
                .cloned()
                .ok_or_else(|| BrimError::NotFound(id.to_string()))
        }

        async fn install(&self, pkg: &Package) -> Result<TransactionResult> {
            self.install_calls.fetch_add(1, AtomicOrdering::SeqCst);
            if self.fail {
                self.err()
            } else {
                Ok(TransactionResult::ok(
                    TransactionAction::Install,
                    pkg.id.clone(),
                    "installed",
                    "mock install output",
                ))
            }
        }

        async fn remove(&self, pkg: &Package) -> Result<TransactionResult> {
            if self.fail {
                self.err()
            } else {
                Ok(TransactionResult::ok(
                    TransactionAction::Remove,
                    pkg.id.clone(),
                    "removed",
                    "mock remove output",
                ))
            }
        }

        async fn updates(&self) -> Result<Vec<Package>> {
            if self.fail {
                self.err()
            } else {
                Ok(self.updates.clone())
            }
        }

        async fn upgrade(&self) -> Result<TransactionResult> {
            self.upgrade_calls.fetch_add(1, AtomicOrdering::SeqCst);
            if self.fail {
                self.err()
            } else {
                Ok(TransactionResult::ok(
                    TransactionAction::Upgrade,
                    "*",
                    "upgraded",
                    format!("{} upgrade output", self.source),
                ))
            }
        }
    }

    fn pkg(id: &str, source: SourceType) -> Package {
        let mut p = Package::new(id, id, source);
        p.status = PackageStatus::Available;
        p
    }

    #[tokio::test]
    async fn search_merges_backends() {
        let mut dnf = MockBackend::new(SourceType::FedoraOfficial);
        let mut p = pkg("ripgrep", SourceType::FedoraOfficial);
        p.rating = 4.5;
        dnf.search_results = vec![p];

        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        flatpak.fail = true; // broken backend must not sink the search

        let mgr = PackageManager::with_backends(vec![Box::new(dnf), Box::new(flatpak)]);
        let results = mgr.search("ripgrep", None).await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "ripgrep");
    }

    #[tokio::test]
    async fn search_filters_by_source() {
        let dnf = MockBackend::new(SourceType::FedoraOfficial);
        let dnf_calls = dnf.search_calls.clone();

        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        flatpak.search_results = vec![pkg("org.example.App", SourceType::Flatpak)];

        let mgr = PackageManager::with_backends(vec![Box::new(dnf), Box::new(flatpak)]);
        let results = mgr.search("app", Some(SourceType::Flatpak)).await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, SourceType::Flatpak);
        assert_eq!(dnf_calls.load(AtomicOrdering::SeqCst), 0);
    }

    #[tokio::test]
    async fn search_sorts_exact_then_rating_then_name() {
        let mut dnf = MockBackend::new(SourceType::FedoraOfficial);
        let mut exact = pkg("foo", SourceType::FedoraOfficial);
        exact.rating = 1.0;
        let mut high = pkg("foo-high", SourceType::FedoraOfficial);
        high.rating = 5.0;
        let mut low = pkg("foo-low", SourceType::FedoraOfficial);
        low.rating = 2.0;
        dnf.search_results = vec![low, exact, high];

        let mgr = PackageManager::with_backends(vec![Box::new(dnf)]);
        let results = mgr.search("foo", None).await;
        let ids: Vec<&str> = results.iter().map(|p| p.id.as_str()).collect();

        assert_eq!(ids, vec!["foo", "foo-high", "foo-low"]);
    }

    #[tokio::test]
    async fn stats_aggregates_sources() {
        let mut dnf = MockBackend::new(SourceType::FedoraOfficial);
        dnf.installed = vec![
            pkg("a", SourceType::FedoraOfficial),
            pkg("b", SourceType::FedoraOfficial),
        ];
        dnf.updates = vec![pkg("a", SourceType::FedoraOfficial)];

        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        flatpak.installed = vec![pkg("org.example.App", SourceType::Flatpak)];

        let mgr = PackageManager::with_backends(vec![Box::new(dnf), Box::new(flatpak)]);
        let stats = mgr.system_stats().await;

        assert_eq!(stats.installed, 3);
        assert_eq!(stats.updates_pending, 1);

        let fedora = stats
            .sources
            .iter()
            .find(|s| s.source == SourceType::FedoraOfficial)
            .expect("fedora stat");
        assert_eq!(fedora.installed, 2);
        assert_eq!(fedora.updates, 1);

        let flatpak_stat = stats
            .sources
            .iter()
            .find(|s| s.source == SourceType::Flatpak)
            .expect("flatpak stat");
        assert_eq!(flatpak_stat.installed, 1);
        assert_eq!(flatpak_stat.updates, 0);
    }

    #[tokio::test]
    async fn info_returns_not_found_when_no_backend_knows_package() {
        let dnf = MockBackend::new(SourceType::FedoraOfficial);
        let mgr = PackageManager::with_backends(vec![Box::new(dnf)]);

        let err = mgr.info("nope", None).await.unwrap_err();
        assert!(matches!(err, BrimError::NotFound(_)));
    }

    #[tokio::test]
    async fn install_routes_to_owning_backend() {
        let mut dnf = MockBackend::new(SourceType::FedoraOfficial);
        dnf.search_results = vec![pkg("htop", SourceType::FedoraOfficial)];
        let dnf_installs = dnf.install_calls.clone();

        let flatpak = MockBackend::new(SourceType::Flatpak);
        let flatpak_installs = flatpak.install_calls.clone();

        let mgr = PackageManager::with_backends(vec![Box::new(dnf), Box::new(flatpak)]);
        let result = mgr.install("htop", None).await.unwrap();

        assert!(result.success);
        assert_eq!(result.action, TransactionAction::Install);
        assert_eq!(dnf_installs.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(flatpak_installs.load(AtomicOrdering::SeqCst), 0);
    }

    #[tokio::test]
    async fn install_falls_back_to_search_when_info_misses() {
        // FlatpakBackend::info fails for not-yet-installed refs, so
        // installing from search results must survive an info NotFound.
        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        let mut p = pkg("org.example.App", SourceType::Flatpak);
        p.flatpak_ref = Some("app/org.example.App/x86_64/stable".to_string());
        flatpak.search_results = vec![p];
        flatpak.info_not_found = true;
        let installs = flatpak.install_calls.clone();

        let mgr = PackageManager::with_backends(vec![Box::new(flatpak)]);
        let result = mgr
            .install(
                "app/org.example.App/x86_64/stable",
                Some(SourceType::Flatpak),
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(installs.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn install_search_fallback_matches_id_case_insensitively() {
        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        flatpak.search_results = vec![pkg("org.example.App", SourceType::Flatpak)];
        flatpak.info_not_found = true;
        let installs = flatpak.install_calls.clone();

        let mgr = PackageManager::with_backends(vec![Box::new(flatpak)]);
        let result = mgr
            .install("ORG.EXAMPLE.APP", Some(SourceType::Flatpak))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(installs.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn remove_falls_back_to_search_when_info_misses() {
        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        flatpak.search_results = vec![pkg("org.example.App", SourceType::Flatpak)];
        flatpak.info_not_found = true;

        let mgr = PackageManager::with_backends(vec![Box::new(flatpak)]);
        let result = mgr
            .remove("org.example.App", Some(SourceType::Flatpak))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.action, TransactionAction::Remove);
    }

    #[tokio::test]
    async fn install_returns_not_found_on_total_miss() {
        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        flatpak.info_not_found = true; // search results are also empty
        let installs = flatpak.install_calls.clone();

        let mgr = PackageManager::with_backends(vec![Box::new(flatpak)]);
        let err = mgr
            .install("org.missing.App", Some(SourceType::Flatpak))
            .await
            .unwrap_err();

        assert!(matches!(err, BrimError::NotFound(_)));
        assert_eq!(installs.load(AtomicOrdering::SeqCst), 0);
    }

    #[tokio::test]
    async fn install_returns_not_found_when_backend_is_broken() {
        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        flatpak.fail = true; // a broken backend must surface as a plain miss

        let mgr = PackageManager::with_backends(vec![Box::new(flatpak)]);
        let err = mgr
            .install("org.example.App", Some(SourceType::Flatpak))
            .await
            .unwrap_err();

        assert!(matches!(err, BrimError::NotFound(_)));
    }

    #[tokio::test]
    async fn upgrade_fails_if_any_backend_fails() {
        let dnf = MockBackend::new(SourceType::FedoraOfficial);
        let dnf_upgrades = dnf.upgrade_calls.clone();

        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        flatpak.fail = true;

        let mgr = PackageManager::with_backends(vec![Box::new(dnf), Box::new(flatpak)]);
        let result = mgr.upgrade().await.unwrap();

        assert!(!result.success);
        assert_eq!(result.action, TransactionAction::Upgrade);
        // All backends were still attempted despite the failure.
        assert_eq!(dnf_upgrades.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn from_config_omits_disabled_sources() {
        use crate::config::Config;
        let mut config = Config::default();
        config.sources.copr = false;
        let pm = PackageManager::from_config(&config);
        let sources: Vec<SourceType> = pm.backends.iter().map(|b| b.source()).collect();
        assert!(sources.contains(&SourceType::FedoraOfficial));
        assert!(!sources.contains(&SourceType::Copr));
        assert!(sources.contains(&SourceType::Flatpak));
        let all = PackageManager::from_config(&Config::default());
        assert_eq!(all.backends.len(), 3);
        let none = PackageManager::from_config(&{
            let mut c = Config::default();
            c.sources.dnf5 = false;
            c.sources.copr = false;
            c.sources.flatpak = false;
            c
        });
        assert!(none.backends.is_empty());
    }

    #[tokio::test]
    async fn unavailable_backends_are_skipped() {
        let mut dnf = MockBackend::new(SourceType::FedoraOfficial);
        dnf.available = false;
        let dnf_calls = dnf.search_calls.clone();

        let mgr = PackageManager::with_backends(vec![Box::new(dnf)]);
        let results = mgr.search("anything", None).await;

        assert!(results.is_empty());
        assert_eq!(dnf_calls.load(AtomicOrdering::SeqCst), 0);
    }
}

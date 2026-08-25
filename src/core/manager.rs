//! Unified package manager engine tying all backends together.
//!
//! [`PackageManager`] is the API surface used by the CLI, GUI, and web
//! frontends. It fans read operations (search, list, updates) out to all
//! available backends concurrently and tolerates individual backend
//! failures: a broken backend yields partial results, never a panic.

use futures::future::join_all;
use tokio::sync::Mutex;

use crate::core::backend::Backend;
use crate::core::backends::{copr::CoprBackend, dnf5::Dnf5Backend, flatpak::FlatpakBackend};
use crate::core::error::BrimError;
use crate::core::models::{
    Package, PackageStatus, RepoInfo, SourceStat, SourceType, SystemStats, TransactionAction,
    TransactionResult,
};
use crate::core::sync::SyncEntry;
use crate::core::Result;

/// The unified engine that routes operations across all backends.
pub struct PackageManager {
    backends: Vec<Box<dyn Backend>>,
    http: reqwest::Client,
    /// Serializes transactions (install/remove/upgrade): concurrent
    /// requests (e.g. from the web API) must never spawn two package
    /// manager processes mutating the system at once.
    tx_lock: Mutex<()>,
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
    ///
    /// This constructor reads the config synchronously; async callers
    /// (frontends on a tokio runtime) should use
    /// [`PackageManager::new_async`] so the executor is never blocked.
    pub fn new() -> Self {
        Self::from_config(&crate::core::config::Config::load())
    }

    /// Async variant of [`PackageManager::new`] for callers already on a
    /// tokio runtime.
    pub async fn new_async() -> Self {
        Self::from_config(&crate::core::config::Config::load_async().await)
    }

    /// Create a manager over the standard backends enabled in `config`.
    pub fn from_config(config: &crate::core::config::Config) -> Self {
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
            http: crate::core::http::client(),
            tx_lock: Mutex::new(()),
        }
    }

    /// Search all available backends concurrently and merge the results.
    ///
    /// When `source` is `Some`, only that backend is queried. Failed
    /// backends are skipped (use [`PackageManager::search_with_errors`] to
    /// observe their errors). Results are sorted: exact-name matches to
    /// `query` first, then by rating descending, then by name ascending.
    pub async fn search(&self, query: &str, source: Option<SourceType>) -> Vec<Package> {
        self.search_with_errors(query, source).await.0
    }

    /// Like [`PackageManager::search`], but also reports per-backend
    /// errors so callers can tell "no results" apart from "every backend
    /// failed". Each error is paired with the source that produced it.
    pub async fn search_with_errors(
        &self,
        query: &str,
        source: Option<SourceType>,
    ) -> (Vec<Package>, Vec<(SourceType, BrimError)>) {
        let backends = self.active_backends(source).await;
        let results = join_all(backends.iter().map(|b| b.search(query))).await;
        let (mut packages, errors) = split_results(backends, results);
        sort_search_results(query, &mut packages);
        (packages, errors)
    }

    /// Search all available backends concurrently, yielding each backend's
    /// result as soon as it completes (fast backends first).
    ///
    /// This is the streaming counterpart of
    /// [`PackageManager::search_with_errors`] for frontends that want to
    /// show early results while slow backends (e.g. COPR's ~9 s endpoint)
    /// are still in flight. Each item pairs the source with its outcome;
    /// batches are neither merged nor sorted — that is the caller's job.
    pub async fn search_stream<'a>(
        &'a self,
        query: &'a str,
        source: Option<SourceType>,
    ) -> impl futures::stream::Stream<Item = (SourceType, Result<Vec<Package>>)> + 'a {
        let backends = self.active_backends(source).await;
        backends
            .into_iter()
            .map(|b| async move { (b.source(), b.search(query).await) })
            .collect::<futures::stream::FuturesUnordered<_>>()
    }

    /// List installed packages across all available backends.
    ///
    /// Failed backends are skipped (use
    /// [`PackageManager::list_installed_with_errors`] to observe their
    /// errors).
    pub async fn list_installed(&self) -> Vec<Package> {
        self.list_installed_with_errors().await.0
    }

    /// Like [`PackageManager::list_installed`], but also reports
    /// per-backend errors.
    pub async fn list_installed_with_errors(&self) -> (Vec<Package>, Vec<(SourceType, BrimError)>) {
        let backends = self.active_backends(None).await;
        let results = join_all(backends.iter().map(|b| b.list_installed())).await;
        let (mut packages, errors) = split_results(backends, results);
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        (packages, errors)
    }

    /// List packages with pending updates across all available backends.
    ///
    /// Failed backends are skipped (use
    /// [`PackageManager::updates_with_errors`] to observe their errors).
    pub async fn updates(&self) -> Vec<Package> {
        self.updates_with_errors().await.0
    }

    /// Like [`PackageManager::updates`], but also reports per-backend
    /// errors so callers can tell "no updates" apart from "a backend
    /// failed and its updates are invisible".
    pub async fn updates_with_errors(&self) -> (Vec<Package>, Vec<(SourceType, BrimError)>) {
        let backends = self.active_backends(None).await;
        let results = join_all(backends.iter().map(|b| b.updates())).await;
        let (mut packages, errors) = split_results(backends, results);
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        (packages, errors)
    }

    /// Get package details; the first backend hit wins.
    ///
    /// [`BrimError::NotFound`] is returned only when every backend reports
    /// the package as genuinely unknown; when some backend failed for
    /// another reason (network, broken tool), the last such error is
    /// returned instead so a broken backend is never disguised as a miss.
    pub async fn info(&self, id: &str, source: Option<SourceType>) -> Result<Package> {
        let mut last_backend_error: Option<BrimError> = None;
        for backend in self.active_backends(source).await {
            match backend.info(id).await {
                Ok(pkg) => return Ok(pkg),
                Err(BrimError::NotFound(_)) => {}
                Err(e) => last_backend_error = Some(e),
            }
        }
        match last_backend_error {
            Some(e) => Err(e),
            None => Err(BrimError::NotFound(id.to_string())),
        }
    }

    /// Install a package, routing to the backend that owns it.
    pub async fn install(&self, id: &str, source: Option<SourceType>) -> Result<TransactionResult> {
        let pkg = self.resolve(id, source).await?;
        self.install_package(&pkg).await
    }

    /// Install an already-resolved package (skips the redundant
    /// [`PackageManager::resolve`] round-trip frontends would otherwise
    /// trigger). Transactions are serialized: a concurrent install,
    /// remove, or upgrade waits for this one to finish.
    pub async fn install_package(&self, pkg: &Package) -> Result<TransactionResult> {
        let _guard = self.tx_lock.lock().await;
        self.backend_for(pkg.source).await?.install(pkg).await
    }

    /// Remove an installed package, routing to the backend that owns it.
    pub async fn remove(&self, id: &str, source: Option<SourceType>) -> Result<TransactionResult> {
        let pkg = self.resolve(id, source).await?;
        self.remove_package(&pkg).await
    }

    /// Remove an already-resolved package (see
    /// [`PackageManager::install_package`]).
    pub async fn remove_package(&self, pkg: &Package) -> Result<TransactionResult> {
        let _guard = self.tx_lock.lock().await;
        self.backend_for(pkg.source).await?.remove(pkg).await
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
    /// successful only if all backends succeeded. Transactions are
    /// serialized against installs and removes.
    pub async fn upgrade(&self) -> Result<TransactionResult> {
        let _guard = self.tx_lock.lock().await;
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
        crate::core::sync::export_sync(&self.list_installed().await)
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
        let (mut trending, installed) = tokio::join!(
            crate::core::trending::trending(&self.http),
            self.list_installed()
        );
        let installed_refs: std::collections::HashSet<&str> = installed
            .iter()
            .filter(|p| p.source == SourceType::Flatpak)
            .flat_map(|p| {
                [Some(p.id.as_str()), p.flatpak_ref.as_deref()]
                    .into_iter()
                    .flatten()
            })
            .collect();
        for pkg in &mut trending {
            if installed_refs.contains(pkg.id.as_str()) {
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

/// Split per-backend list results into merged packages and per-backend
/// errors, preserving backend order for both.
fn split_results(
    backends: Vec<&dyn Backend>,
    results: Vec<Result<Vec<Package>>>,
) -> (Vec<Package>, Vec<(SourceType, BrimError)>) {
    let mut packages = Vec::new();
    let mut errors = Vec::new();
    for (backend, result) in backends.into_iter().zip(results) {
        match result {
            Ok(pkgs) => packages.extend(pkgs),
            Err(e) => errors.push((backend.source(), e)),
        }
    }
    (packages, errors)
}

/// Sort search hits: exact-name matches first, then rating descending,
/// then name ascending.
pub(crate) fn sort_search_results(query: &str, packages: &mut [Package]) {
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
    use crate::core::models::PackageStatus;
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
    async fn install_surfaces_backend_error_when_backend_is_broken() {
        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        flatpak.fail = true; // a broken backend must surface its real error

        let mgr = PackageManager::with_backends(vec![Box::new(flatpak)]);
        let err = mgr
            .install("org.example.App", Some(SourceType::Flatpak))
            .await
            .unwrap_err();

        assert!(matches!(err, BrimError::CommandFailed(_)));
    }

    #[tokio::test]
    async fn info_returns_backend_error_when_not_a_real_miss() {
        // A network/tool failure must not be disguised as NotFound.
        let mut dnf = MockBackend::new(SourceType::FedoraOfficial);
        dnf.fail = true;

        let mgr = PackageManager::with_backends(vec![Box::new(dnf)]);
        let err = mgr.info("htop", None).await.unwrap_err();

        assert!(matches!(err, BrimError::CommandFailed(_)));
    }

    #[tokio::test]
    async fn search_with_errors_reports_failing_backends() {
        let mut dnf = MockBackend::new(SourceType::FedoraOfficial);
        dnf.search_results = vec![pkg("ripgrep", SourceType::FedoraOfficial)];

        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        flatpak.fail = true;

        let mgr = PackageManager::with_backends(vec![Box::new(dnf), Box::new(flatpak)]);
        let (packages, errors) = mgr.search_with_errors("ripgrep", None).await;

        assert_eq!(packages.len(), 1);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, SourceType::Flatpak);
        assert!(matches!(errors[0].1, BrimError::CommandFailed(_)));
        // The plain search keeps working over partial results.
        assert_eq!(mgr.search("ripgrep", None).await.len(), 1);
    }

    #[tokio::test]
    async fn search_stream_yields_every_backend_outcome() {
        use futures::StreamExt;

        let mut dnf = MockBackend::new(SourceType::FedoraOfficial);
        dnf.search_results = vec![pkg("ripgrep", SourceType::FedoraOfficial)];

        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        flatpak.fail = true;

        let mgr = PackageManager::with_backends(vec![Box::new(dnf), Box::new(flatpak)]);
        let outcomes: Vec<_> = mgr.search_stream("ripgrep", None).await.collect().await;

        // Both backends report exactly once, with the same success/failure
        // split as search_with_errors (order is completion order, so only
        // the aggregate is asserted).
        assert_eq!(outcomes.len(), 2);
        let packages: Vec<_> = outcomes
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .flatten()
            .collect();
        let failed: Vec<_> = outcomes.iter().filter(|(_, r)| r.is_err()).collect();
        assert_eq!(packages.len(), 1);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, SourceType::Flatpak);
    }

    #[tokio::test]
    async fn list_installed_with_errors_reports_failing_backends() {
        let mut dnf = MockBackend::new(SourceType::FedoraOfficial);
        dnf.installed = vec![pkg("htop", SourceType::FedoraOfficial)];

        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        flatpak.fail = true;

        let mgr = PackageManager::with_backends(vec![Box::new(dnf), Box::new(flatpak)]);
        let (packages, errors) = mgr.list_installed_with_errors().await;

        assert_eq!(packages.len(), 1);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, SourceType::Flatpak);
        assert_eq!(mgr.list_installed().await.len(), 1);
    }

    #[tokio::test]
    async fn install_package_skips_resolve() {
        // A frontend that already resolved the package must not pay for a
        // second resolve: info/search stay untouched.
        let mut flatpak = MockBackend::new(SourceType::Flatpak);
        flatpak.info_not_found = true; // resolve would fail here
        let searches = flatpak.search_calls.clone();
        let installs = flatpak.install_calls.clone();

        let mgr = PackageManager::with_backends(vec![Box::new(flatpak)]);
        let result = mgr
            .install_package(&pkg("org.example.App", SourceType::Flatpak))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(installs.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(searches.load(AtomicOrdering::SeqCst), 0);
    }

    /// Backend whose transactions sleep so lock contention is observable.
    struct SlowBackend {
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Backend for SlowBackend {
        fn source(&self) -> SourceType {
            SourceType::Flatpak
        }

        async fn is_available(&self) -> bool {
            true
        }

        async fn search(&self, _query: &str) -> Result<Vec<Package>> {
            Ok(vec![])
        }

        async fn list_installed(&self) -> Result<Vec<Package>> {
            Ok(vec![])
        }

        async fn info(&self, id: &str) -> Result<Package> {
            Err(BrimError::NotFound(id.to_string()))
        }

        async fn install(&self, pkg: &Package) -> Result<TransactionResult> {
            let n = self.in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            self.max_in_flight.fetch_max(n, AtomicOrdering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
            Ok(TransactionResult::ok(
                TransactionAction::Install,
                pkg.id.clone(),
                "installed",
                "",
            ))
        }

        async fn remove(&self, pkg: &Package) -> Result<TransactionResult> {
            Ok(TransactionResult::ok(
                TransactionAction::Remove,
                pkg.id.clone(),
                "removed",
                "",
            ))
        }

        async fn updates(&self) -> Result<Vec<Package>> {
            Ok(vec![])
        }

        async fn upgrade(&self) -> Result<TransactionResult> {
            Ok(TransactionResult::ok(
                TransactionAction::Upgrade,
                "*",
                "upgraded",
                "",
            ))
        }
    }

    #[tokio::test]
    async fn transactions_are_serialized() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let backend = SlowBackend {
            in_flight: in_flight.clone(),
            max_in_flight: max_in_flight.clone(),
        };
        let mgr = PackageManager::with_backends(vec![Box::new(backend)]);
        let p = pkg("org.example.App", SourceType::Flatpak);

        let (a, b) = tokio::join!(mgr.install_package(&p), mgr.install_package(&p));

        a.unwrap();
        b.unwrap();
        // The two installs ran one after another, never concurrently.
        assert_eq!(max_in_flight.load(AtomicOrdering::SeqCst), 1);
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
        use crate::core::config::Config;
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

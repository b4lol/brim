//! Worker thread owning the tokio runtime and the brim-core engine.
//!
//! The GTK main loop must never block on a backend call, so all
//! `PackageManager` work happens on this dedicated thread. The GUI sends
//! [`CoreRequest`]s and receives [`CoreEvent`]s over async channels.
//!
//! Each incoming request is spawned as its own local task, so a long-running
//! transaction (install/remove/upgrade) does not block pending searches.
//! Events may therefore arrive out of order; the GUI must not assume ordering.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::core::{
    Package, PackageManager, SourceType, SystemStats, TransactionAction, TransactionResult,
};
use async_channel::{Receiver, Sender};

/// Channel capacity for both request and event channels. The GUI is a
/// single-user app; if a channel is ever full the message is dropped (with an
/// `eprintln!`) rather than growing memory without bound.
const CHANNEL_CAPACITY: usize = 128;

/// How often collected icon fetch results are flushed to the GUI as a single
/// batched event.
const ICON_FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum number of icon downloads running concurrently; the rest wait on a
/// semaphore instead of firing hundreds of HTTP requests at once.
const MAX_ICON_FETCHES: usize = 8;

/// Completed icon fetches (app id + cached file on success) waiting for the
/// next batched flush to the GUI.
type IconResults = Rc<RefCell<Vec<(String, Option<PathBuf>)>>>;

/// How long a cached search result answers an identical query.
const SEARCH_CACHE_TTL: Duration = Duration::from_secs(60);

/// One-entry search cache: the last query and its results.
struct SearchCache {
    query: String,
    results: Vec<Package>,
    at: std::time::Instant,
}

/// Whether a cache entry created `at` is still valid at `now`.
fn is_fresh(at: std::time::Instant, now: std::time::Instant, ttl: Duration) -> bool {
    now.duration_since(at) < ttl
}

/// Requests sent from the GTK main loop to the core worker.
pub enum CoreRequest {
    Search(String),
    LoadInstalled,
    LoadUpdates,
    LoadStats,
    LoadTrending,
    /// Configured repositories (flatpak remotes + COPR repos) for the
    /// repository groups on the Settings page.
    LoadRepos,
    /// Add a flatpak remote (name, .flatpakrepo URL).
    AddFlatpakRemote(String, String),
    /// Remove a flatpak remote by name.
    RemoveFlatpakRemote(String),
    /// Enable or disable a COPR repo (owner/project id).
    SetCoprEnabled(String, bool),
    Install(String, Option<SourceType>),
    Remove(String, Option<SourceType>),
    Upgrade,
    /// Write the installed set as a sync file to the given path.
    ExportSyncTo(std::path::PathBuf),
    /// Read and parse a sync file at the given path.
    ParseSyncFile(std::path::PathBuf),
    /// Install the packages parsed from a sync file.
    ImportSync(Vec<crate::core::SyncEntry>),
    /// Rebuild the PackageManager from the on-disk config (source switches
    /// changed in Settings).
    ReloadConfig,
    /// Download a Flathub CDN icon for an app id into Brim's icon cache.
    FetchIcon(String),
}

/// Events sent from the core worker back to the GTK main loop.
pub enum CoreEvent {
    /// Results of a search, tagged with the request's query so the GUI can
    /// drop stale results (requests run concurrently, so a slow earlier
    /// search can finish after a newer one).
    SearchResults(String, Vec<Package>),
    Installed(Vec<Package>),
    Updates(Vec<Package>),
    Stats(SystemStats),
    /// Flathub's most popular apps for the Trending page.
    Trending(Vec<Package>),
    /// Configured repositories for the Settings page's repository groups.
    Repos(Vec<crate::core::RepoInfo>),
    TransactionDone(TransactionResult),
    /// Sync export finished: Ok(path written) or Err(message).
    SyncExported(Result<String, String>),
    /// A sync file was parsed (empty on unreadable/invalid files).
    SyncParsed(Vec<crate::core::SyncEntry>),
    /// The manager was rebuilt after a config change; pages should refresh.
    ConfigReloaded,
    /// A batch of finished Flathub icon fetches: app id plus the cached file
    /// on success (`None` on failure — rows keep their themed fallback).
    /// Batched so a flood of downloads cannot crowd core events (search
    /// results, transaction outcomes) out of the bounded event channel.
    IconsReady(Vec<(String, Option<PathBuf>)>),
    /// Worker-side failure the user should see (thread spawn, runtime
    /// build, unreadable sync file).
    Error(String),
}

/// Spawn the core worker thread.
///
/// Returns a sender for [`CoreRequest`]s; events are delivered on `tx`.
/// If the thread cannot be spawned, an [`CoreEvent::Error`] is queued and the
/// request channel is left closed: requests silently go nowhere and the GUI
/// event loop reports the dead worker instead of panicking.
pub fn spawn(tx: Sender<CoreEvent>) -> Sender<CoreRequest> {
    let (req_tx, req_rx): (Sender<CoreRequest>, Receiver<CoreRequest>) =
        async_channel::bounded(CHANNEL_CAPACITY);
    let worker_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("brim-core-worker".to_string())
        .spawn(move || run(req_rx, worker_tx));
    if let Err(error) = spawned {
        eprintln!("brim-gui: failed to spawn core worker thread: {error}");
        let _ = tx.try_send(CoreEvent::Error(format!(
            "failed to spawn core worker thread: {error}"
        )));
    }
    req_tx
}

fn run(rx: Receiver<CoreRequest>, tx: Sender<CoreEvent>) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("brim-gui: failed to build worker tokio runtime: {error}");
            let _ = tx.try_send(CoreEvent::Error(format!(
                "failed to build worker tokio runtime: {error}"
            )));
            return;
        }
    };

    rt.block_on(async move {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                // Swappable manager: ReloadConfig replaces the Rc; in-flight
                // tasks keep their snapshot and finish safely.
                let manager: Rc<RefCell<Rc<PackageManager>>> =
                    Rc::new(RefCell::new(Rc::new(PackageManager::new_async().await)));
                // Icon fetch results collect here and are flushed as one
                // batched event every ICON_FLUSH_INTERVAL (see below).
                let icon_results: IconResults = Rc::new(RefCell::new(Vec::new()));
                // App ids with an icon download currently in flight: the GUI
                // may request the same icon once per row, so duplicates are
                // dropped here instead of downloading the same file twice.
                let icon_inflight: Rc<RefCell<std::collections::HashSet<String>>> =
                    Rc::new(RefCell::new(std::collections::HashSet::new()));
                let icon_permits = Arc::new(tokio::sync::Semaphore::new(MAX_ICON_FETCHES));
                let http_client = crate::core::http::client();
                // One-entry search cache: an identical query within
                // SEARCH_CACHE_TTL is answered from memory instead of
                // re-running the backends. Cleared on ReloadConfig and on
                // transactions so stale results are never shown after the
                // installed set changes.
                let search_cache: Rc<RefCell<Option<SearchCache>>> = Rc::new(RefCell::new(None));
                // Bumped every time the cache is invalidated (config reload,
                // transaction). A search task started before the invalidation
                // would otherwise repopulate the cache with pre-transaction
                // results when it finishes after it.
                let cache_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
                // Flusher: coalesces icon results into batched events so a
                // flood of completed downloads can never crowd core events
                // out of the bounded event channel. Ends when the runtime
                // drops it after the request channel closes.
                {
                    let icon_results = icon_results.clone();
                    let tx = tx.clone();
                    tokio::task::spawn_local(async move {
                        loop {
                            tokio::time::sleep(ICON_FLUSH_INTERVAL).await;
                            let batch: Vec<_> = icon_results.borrow_mut().drain(..).collect();
                            if !batch.is_empty()
                                && tx.try_send(CoreEvent::IconsReady(batch)).is_err()
                            {
                                eprintln!("brim-gui: event channel full or closed; dropping icons");
                            }
                        }
                    });
                }
                // Dispatch loop: spawn one task per request so a slow
                // transaction never blocks queued searches. When the request
                // channel closes (GUI gone), the loop ends and the runtime
                // drops any in-flight tasks.
                while let Ok(request) = rx.recv().await {
                    if let CoreRequest::ReloadConfig = request {
                        reload_manager(&manager, &crate::core::Config::load());
                        // Sources changed: drop cached search results so the
                        // next identical query re-runs against the new config.
                        *search_cache.borrow_mut() = None;
                        cache_generation.set(cache_generation.get() + 1);
                        if tx.try_send(CoreEvent::ConfigReloaded).is_err() {
                            eprintln!("brim-gui: event channel full or closed; dropping event");
                        }
                        continue;
                    }
                    // Icon fetches are intercepted here: they report into
                    // the batching queue instead of sending their own event.
                    if let CoreRequest::FetchIcon(app_id) = request {
                        // One in-flight download per app id; binds during a
                        // flood would otherwise duplicate the same fetch.
                        if !icon_inflight.borrow_mut().insert(app_id.clone()) {
                            continue;
                        }
                        let icon_results = icon_results.clone();
                        let icon_inflight = icon_inflight.clone();
                        let permits = icon_permits.clone();
                        let client = http_client.clone();
                        tokio::task::spawn_local(async move {
                            let Ok(_permit) = permits.acquire().await else {
                                icon_inflight.borrow_mut().remove(&app_id);
                                return;
                            };
                            let path =
                                crate::gui::icons::fetch_flathub_icon(&client, &app_id).await;
                            icon_inflight.borrow_mut().remove(&app_id);
                            icon_results.borrow_mut().push((app_id, path));
                        });
                        continue;
                    }
                    // Search cache hit: an identical query inside the TTL is
                    // answered from memory without spawning a backend task.
                    if let CoreRequest::Search(query) = &request {
                        let hit = search_cache.borrow().as_ref().and_then(|c| {
                            (c.query.trim() == query.trim()
                                && is_fresh(c.at, std::time::Instant::now(), SEARCH_CACHE_TTL))
                            .then(|| c.results.clone())
                        });
                        if let Some(results) = hit {
                            let event = CoreEvent::SearchResults(query.clone(), results);
                            if tx.try_send(event).is_err() {
                                eprintln!("brim-gui: event channel full or closed; dropping event");
                            }
                            continue;
                        }
                    }
                    // Transactions change installed state: cached searches
                    // would otherwise serve pre-transaction results.
                    if matches!(
                        request,
                        CoreRequest::Install(..)
                            | CoreRequest::Remove(..)
                            | CoreRequest::Upgrade
                            | CoreRequest::ImportSync(..)
                    ) {
                        *search_cache.borrow_mut() = None;
                        cache_generation.set(cache_generation.get() + 1);
                    }
                    let snapshot = manager.borrow().clone();
                    let tx = tx.clone();
                    let search_cache = search_cache.clone();
                    let cache_generation = cache_generation.clone();
                    let started_at = cache_generation.get();
                    tokio::task::spawn_local(async move {
                        let event = handle(request, &snapshot, &tx).await;
                        if let CoreEvent::SearchResults(query, packages) = &event {
                            // Cache only when no transaction or config reload
                            // landed while the search was running; those
                            // results predate the installed-set change.
                            if started_at == cache_generation.get() {
                                *search_cache.borrow_mut() = Some(SearchCache {
                                    query: query.clone(),
                                    results: packages.clone(),
                                    at: std::time::Instant::now(),
                                });
                            }
                        }
                        // Bounded channel: drop with a log if the GUI is
                        // somehow 128 events behind.
                        if tx.try_send(event).is_err() {
                            eprintln!("brim-gui: event channel full or closed; dropping event");
                        }
                    });
                }
            })
            .await;
    });
}

/// Swap in a freshly built manager from `config`. In-flight tasks hold
/// their own `Rc` snapshot and finish on the old one.
fn reload_manager(manager: &RefCell<Rc<PackageManager>>, config: &crate::core::Config) {
    *manager.borrow_mut() = Rc::new(PackageManager::from_config(config));
}

/// Execute one request against the manager and produce the reply event.
/// Per-backend failures of a partially successful request (updates fetch)
/// are reported as extra [`CoreEvent::Error`]s on `tx` — the GUI toasts
/// them, so a broken backend is not invisible.
async fn handle(
    request: CoreRequest,
    manager: &PackageManager,
    tx: &Sender<CoreEvent>,
) -> CoreEvent {
    match request {
        CoreRequest::Search(query) => {
            let results = manager.search(&query, None).await;
            CoreEvent::SearchResults(query, results)
        }
        CoreRequest::LoadInstalled => CoreEvent::Installed(manager.list_installed().await),
        CoreRequest::LoadUpdates => {
            let (packages, errors) = manager.updates_with_errors().await;
            for (source, error) in errors {
                let event = CoreEvent::Error(format!("{source} backend failed: {error}"));
                if tx.try_send(event).is_err() {
                    eprintln!("brim-gui: event channel full or closed; dropping event");
                }
            }
            CoreEvent::Updates(packages)
        }
        CoreRequest::LoadStats => CoreEvent::Stats(manager.system_stats().await),
        CoreRequest::LoadTrending => CoreEvent::Trending(manager.trending().await),
        CoreRequest::LoadRepos => CoreEvent::Repos(manager.list_repos().await),
        CoreRequest::AddFlatpakRemote(name, url) => {
            let result = manager
                .add_flatpak_remote(&name, &url)
                .await
                .unwrap_or_else(|e| {
                    TransactionResult::err(
                        TransactionAction::RepoChange,
                        &name,
                        "add remote failed",
                        e.to_string(),
                    )
                });
            CoreEvent::TransactionDone(result)
        }
        CoreRequest::RemoveFlatpakRemote(name) => {
            let result = manager
                .remove_flatpak_remote(&name)
                .await
                .unwrap_or_else(|e| {
                    TransactionResult::err(
                        TransactionAction::RepoChange,
                        &name,
                        "remove remote failed",
                        e.to_string(),
                    )
                });
            CoreEvent::TransactionDone(result)
        }
        CoreRequest::SetCoprEnabled(id, enabled) => {
            let result = manager
                .set_copr_repo_enabled(&id, enabled)
                .await
                .unwrap_or_else(|e| {
                    TransactionResult::err(
                        TransactionAction::RepoChange,
                        &id,
                        "repo change failed",
                        e.to_string(),
                    )
                });
            CoreEvent::TransactionDone(result)
        }
        CoreRequest::Install(id, source) => {
            let result = manager.install(&id, source).await.unwrap_or_else(|e| {
                TransactionResult::err(
                    TransactionAction::Install,
                    &id,
                    "install failed",
                    e.to_string(),
                )
            });
            CoreEvent::TransactionDone(result)
        }
        CoreRequest::Remove(id, source) => {
            let result = manager.remove(&id, source).await.unwrap_or_else(|e| {
                TransactionResult::err(
                    TransactionAction::Remove,
                    &id,
                    "remove failed",
                    e.to_string(),
                )
            });
            CoreEvent::TransactionDone(result)
        }
        CoreRequest::Upgrade => {
            let result = manager.upgrade().await.unwrap_or_else(|e| {
                TransactionResult::err(
                    TransactionAction::Upgrade,
                    "*",
                    "upgrade failed",
                    e.to_string(),
                )
            });
            CoreEvent::TransactionDone(result)
        }
        CoreRequest::ExportSyncTo(path) => {
            let json = manager.export_sync().await;
            let label = path.to_string_lossy().into_owned();
            let result = tokio::fs::write(&path, json)
                .await
                .map(|()| label)
                .map_err(|e| e.to_string());
            CoreEvent::SyncExported(result)
        }
        CoreRequest::ParseSyncFile(path) => match tokio::fs::read_to_string(&path).await {
            Ok(text) => match crate::core::sync::parse_import(&text) {
                Ok(entries) => CoreEvent::SyncParsed(entries),
                // A corrupt or too-new sync file is not an empty one: tell
                // the user instead of showing the misleading "no packages
                // found" toast.
                Err(error) => {
                    CoreEvent::Error(format!("Could not import {}: {error}", path.display()))
                }
            },
            // An unreadable file is not an empty one either.
            Err(error) => CoreEvent::Error(format!("Could not read {}: {error}", path.display())),
        },
        CoreRequest::ImportSync(entries) => {
            let results = manager.import_sync(entries).await;
            let succeeded = results.iter().filter(|r| r.success).count();
            let failed = results.len() - succeeded;
            let message = format!("Import finished: {succeeded} succeeded, {failed} failed");
            let result = if failed == 0 {
                TransactionResult::ok(TransactionAction::Install, "*", message, "")
            } else {
                TransactionResult::err(TransactionAction::Install, "*", message, "")
            };
            CoreEvent::TransactionDone(result)
        }
        CoreRequest::FetchIcon(_) => {
            // Intercepted by the dispatch loop, which batches icon results;
            // `handle` is never called with this variant.
            unreachable!("FetchIcon is handled in the dispatch loop")
        }
        CoreRequest::ReloadConfig => {
            // Intercepted by the dispatch loop, which swaps the manager;
            // `handle` is never called with this variant.
            unreachable!("ReloadConfig is handled in the dispatch loop")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn search_event_echoes_the_query() {
        // An empty backend set is hermetic: nothing is probed or spawned.
        let manager = PackageManager::with_backends(vec![]);
        let (tx, _rx) = async_channel::bounded(1);
        let event = handle(CoreRequest::Search("htop".to_string()), &manager, &tx).await;
        let CoreEvent::SearchResults(query, packages) = event else {
            panic!("expected SearchResults");
        };
        assert_eq!(query, "htop");
        assert!(packages.is_empty());
    }

    #[test]
    fn search_cache_freshness() {
        let now = std::time::Instant::now();
        let at = now - std::time::Duration::from_secs(30);
        assert!(is_fresh(at, now, SEARCH_CACHE_TTL));
        let stale = now - std::time::Duration::from_secs(120);
        assert!(!is_fresh(stale, now, SEARCH_CACHE_TTL));
    }

    #[test]
    fn reload_manager_replaces_the_manager() {
        use std::cell::RefCell;
        let manager: RefCell<Rc<PackageManager>> =
            RefCell::new(Rc::new(PackageManager::with_backends(vec![])));
        let before = Rc::as_ptr(&manager.borrow());
        reload_manager(&manager, &crate::core::Config::default());
        let after = Rc::as_ptr(&manager.borrow());
        assert!(!std::ptr::eq(before, after));
    }
}

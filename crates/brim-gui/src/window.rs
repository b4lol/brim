//! Main application window and core-to-GTK plumbing.
//!
//! All brim-core calls go to the worker thread (`worker.rs`); results come
//! back over an `async_channel` and are applied on the GTK main loop inside
//! `glib::spawn_future_local`. The worker dispatches requests concurrently,
//! so events may arrive out of order — every handler below applies its event
//! independently and never assumes a previous event has landed.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use adw::{
    ApplicationWindow, Clamp, HeaderBar, Toast, ToastOverlay, ToolbarView, ViewStack, ViewSwitcher,
};
use async_channel::Sender;
use brim_core::{Package, PackageStatus, RepoInfo, SourceType};
use futures::FutureExt;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Align, Box, Button, Image, Label, Orientation, ScrolledWindow, SearchEntry};
use libadwaita as adw;

use crate::icons::{self, IconChoice};
use crate::rows;
use crate::worker::{self, CoreEvent, CoreRequest};

/// How long to wait after the last keystroke before searching.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

/// Pure debounce/eligibility decision: dispatch a search only for the latest
/// generation and only when the query is non-empty.
fn should_dispatch(query: &str, gen: u64, current: u64) -> bool {
    gen == current && !query.trim().is_empty()
}

/// After a successful transaction, decide which query (if any) to re-run so
/// the Trending page reflects new installed state. An empty last query means
/// the user is not searching, so Trending is reloaded instead.
fn refresh_query(last_query: &str) -> Option<String> {
    let query = last_query.trim();
    if query.is_empty() {
        None
    } else {
        Some(query.to_string())
    }
}

/// Stale-result guard: accept a tagged SearchResults event only when its
/// query still matches the query the page is showing (trimmed compare).
/// The worker runs searches concurrently, so a slow earlier search can
/// finish after a newer one and must not overwrite its results.
fn accepts_results(tagged: &str, current: &str) -> bool {
    tagged.trim() == current.trim()
}

/// Build and present the main window.
pub fn build(app: &adw::Application) {
    // Bounded channels: the GUI is single-user, so if a channel is ever full
    // the message is dropped (with an eprintln) instead of growing unbounded.
    let (event_tx, event_rx) = async_channel::bounded::<CoreEvent>(128);
    let request_tx = worker::spawn(event_tx);

    let window = ApplicationWindow::new(app);
    window.set_title(Some("Brim"));
    window.set_default_size(1100, 760);

    let header = HeaderBar::new();
    let stack = ViewStack::new();

    let switcher = ViewSwitcher::new();
    switcher.set_stack(Some(&stack));
    switcher.set_policy(adw::ViewSwitcherPolicy::Wide);
    header.set_title_widget(Some(&switcher));

    let search = SearchEntry::new();
    search.set_placeholder_text(Some("Search packages…"));
    search.set_hexpand(false);
    header.pack_end(&search);

    // Transaction pending state: buttons disabled while their transaction is
    // in flight, re-enabled on TransactionDone. Row buttons are tracked in
    // `pending_buttons`; successful transactions also refill the lists.
    // `pending_ids` tracks the package ids themselves so a recycled row
    // (rebind) keeps its button insensitive while its transaction runs.
    let pending_buttons: Rc<RefCell<Vec<Button>>> = Rc::new(RefCell::new(Vec::new()));
    let pending_ids: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));
    let upgrade_in_flight = Rc::new(Cell::new(false));

    // The shared config, loaded once; Settings switches mutate and save it.
    let shared_config = Rc::new(RefCell::new(brim_core::Config::load()));

    // Action callbacks shared by every list row and the detail dialog:
    // `on_action` runs Install/Update/Remove, `on_activate` opens the dialog.
    let on_action: rows::ActionFn = {
        let tx = request_tx.clone();
        let pending = pending_buttons.clone();
        let pending_ids = pending_ids.clone();
        // Remove confirmations are presented on the window, not the clicked
        // button: dialog buttons die with the closing detail dialog.
        let alert_parent = window.clone();
        Rc::new(move |pkg, button| {
            handle_row_action(pkg, button, &alert_parent, &tx, &pending, &pending_ids)
        })
    };
    let on_activate: rows::ActivateFn = {
        let window = window.clone();
        let on_action = on_action.clone();
        Rc::new(move |pkg| open_package_dialog(pkg, &window, on_action.clone()))
    };

    // Pages: Trending, Updates, Installed, COPR Spotlight, Settings,
    // Repositories.
    let toast_overlay = ToastOverlay::new();
    toast_overlay.set_child(Some(&stack));

    let (trending_store, trending_page) = flow_page(
        &stack,
        "trending",
        "Trending",
        "emblem-favorite-symbolic",
        on_action.clone(),
        on_activate.clone(),
        pending_ids.clone(),
    );
    let (updates_store, updates_page, stats_label, upgrade_all) = updates_page(
        &stack,
        &request_tx,
        &upgrade_in_flight,
        on_action.clone(),
        on_activate.clone(),
        pending_ids.clone(),
    );
    let (installed_store, installed_page) = installed_page(
        &stack,
        &request_tx,
        &window,
        on_action.clone(),
        on_activate.clone(),
        pending_ids.clone(),
    );
    let (spotlight_store, spotlight_page) = flow_page(
        &stack,
        "copr",
        "COPR Spotlight",
        "starred-symbolic",
        on_action.clone(),
        on_activate.clone(),
        pending_ids.clone(),
    );
    settings_page(&stack, &request_tx, &shared_config);
    // Repositories page: flatpak remotes and COPR repos as preferences
    // groups with entry rows plus one boxed ListBox each for data rows.
    let repos_groups = repos_page(&stack, &request_tx);

    let toolbar = ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toast_overlay));
    window.set_content(Some(&toolbar));

    // Debounced live search.
    let last_query = Rc::new(RefCell::new(String::new()));
    let search_generation = Rc::new(Cell::new(0u64));
    {
        let tx = request_tx.clone();
        let gen = search_generation.clone();
        let last_query = last_query.clone();
        search.connect_search_changed(move |entry| {
            let query = entry.text().to_string();
            gen.set(gen.get() + 1);
            let my_gen = gen.get();
            if query.trim().is_empty() {
                // Clearing the entry restores Trending instead of an empty page.
                *last_query.borrow_mut() = String::new();
                if tx.try_send(CoreRequest::LoadTrending).is_err() {
                    eprintln!("brim-gui: request channel full or closed; dropping trending load");
                }
                return;
            }
            let tx = tx.clone();
            let gen = gen.clone();
            let last_query = last_query.clone();
            glib::timeout_add_local_once(SEARCH_DEBOUNCE, move || {
                if should_dispatch(&query, my_gen, gen.get()) {
                    *last_query.borrow_mut() = query.clone();
                    if tx.try_send(CoreRequest::Search(query)).is_err() {
                        eprintln!("brim-gui: request channel full or closed; dropping search");
                    }
                }
            });
        });
    }

    // Load page data when a page becomes visible.
    {
        let tx = request_tx.clone();
        stack.connect_visible_child_name_notify(move |stack| {
            match stack.visible_child_name().as_deref() {
                Some("updates") => {
                    let _ = tx.try_send(CoreRequest::LoadUpdates);
                    let _ = tx.try_send(CoreRequest::LoadStats);
                }
                Some("installed") => {
                    let _ = tx.try_send(CoreRequest::LoadInstalled);
                }
                Some("repos") => {
                    let _ = tx.try_send(CoreRequest::LoadRepos);
                }
                _ => {}
            }
        });
    }

    // Graceful shutdown: closing the window wakes the event loop so it can
    // break and drop its request sender. Once the window is destroyed, the
    // widget signal closures drop their senders too, the worker's request
    // channel closes, and the worker thread (with its tokio runtime) exits —
    // no leaked thread or future when a new window is activated later.
    let (shutdown_tx, shutdown_rx) = async_channel::bounded::<()>(1);
    window.connect_close_request(move |_| {
        let _ = shutdown_tx.try_send(());
        glib::Propagation::Proceed
    });

    // Apply worker events on the GTK main loop.
    {
        let toast_overlay = toast_overlay.clone();
        let last_query = last_query.clone();
        let tx = request_tx.clone();
        let pending_buttons = pending_buttons.clone();
        let pending_ids = pending_ids.clone();
        let upgrade_in_flight = upgrade_in_flight.clone();
        let shared_config = shared_config.clone();
        let repos_groups = repos_groups.clone();
        let window = window.clone();
        glib::spawn_future_local(async move {
            loop {
                let shutdown = shutdown_rx.recv().fuse();
                let event = event_rx.recv().fuse();
                futures::pin_mut!(shutdown, event);
                let event = futures::select! {
                    _ = shutdown => break,
                    event = event => event,
                };
                match event {
                    Ok(event) => match event {
                        CoreEvent::Trending(packages) => {
                            // Trending only paints when the user is not
                            // searching — a slow load must not overwrite
                            // fresh search results.
                            if last_query.borrow().trim().is_empty() {
                                populate(
                                    &trending_store,
                                    &trending_page,
                                    &packages,
                                    &tx,
                                    &shared_config,
                                );
                            }
                        }
                        CoreEvent::SearchResults(query, packages) => {
                            // Trending and COPR Spotlight are both fed from
                            // the query currently shown, so they accept only
                            // events tagged with that query. Anything else is
                            // a stale result from a superseded search — drop
                            // it instead of overwriting newer results.
                            if accepts_results(&query, &last_query.borrow()) {
                                populate(
                                    &trending_store,
                                    &trending_page,
                                    &packages,
                                    &tx,
                                    &shared_config,
                                );
                                let copr: Vec<Package> = packages
                                    .iter()
                                    .filter(|p| p.source == SourceType::Copr)
                                    .cloned()
                                    .collect();
                                populate(
                                    &spotlight_store,
                                    &spotlight_page,
                                    &copr,
                                    &tx,
                                    &shared_config,
                                );
                            }
                        }
                        CoreEvent::Installed(packages) => {
                            populate(
                                &installed_store,
                                &installed_page,
                                &packages,
                                &tx,
                                &shared_config,
                            );
                        }
                        CoreEvent::SyncExported(result) => {
                            let message = match result {
                                Ok(path) => format!("Exported to {path}"),
                                Err(error) => format!("Export failed: {error}"),
                            };
                            toast_overlay.add_toast(Toast::new(&message));
                        }
                        CoreEvent::SyncParsed(entries) => {
                            // The worker read and parsed the file; only the
                            // confirm dialog runs here on the main loop.
                            if entries.is_empty() {
                                toast_overlay
                                    .add_toast(Toast::new("No packages found in that file"));
                            } else {
                                let confirm = adw::AlertDialog::builder()
                                    .heading("Import package list?")
                                    .body(format!(
                                        "This installs {} packages from the file. Already-installed packages are skipped by the backends.",
                                        entries.len()
                                    ))
                                    .build();
                                confirm
                                    .add_responses(&[("cancel", "Cancel"), ("import", "Import")]);
                                confirm.set_response_appearance(
                                    "import",
                                    adw::ResponseAppearance::Suggested,
                                );
                                confirm.set_default_response(Some("cancel"));
                                confirm.set_close_response("cancel");
                                let tx = tx.clone();
                                confirm.connect_response(None, move |_, response| {
                                    if response == "import"
                                        && tx.try_send(CoreRequest::ImportSync(entries.clone())).is_err()
                                    {
                                        eprintln!("brim-gui: request channel full or closed; dropping sync import");
                                    }
                                });
                                confirm.present(Some(&window));
                            }
                        }
                        CoreEvent::Updates(packages) => {
                            populate(
                                &updates_store,
                                &updates_page,
                                &packages,
                                &tx,
                                &shared_config,
                            );
                        }
                        CoreEvent::Stats(stats) => {
                            stats_label.set_text(&format!(
                                "{} installed · {} updates pending",
                                stats.installed, stats.updates_pending
                            ));
                        }
                        CoreEvent::Repos(repos) => {
                            fill_repo_groups(&repos_groups, &repos, &tx, &window);
                        }
                        CoreEvent::TransactionDone(result) => {
                            // Transaction finished: unlock the pending state.
                            for button in pending_buttons.borrow_mut().drain(..) {
                                button.set_sensitive(true);
                            }
                            pending_ids.borrow_mut().clear();
                            upgrade_in_flight.set(false);
                            upgrade_all.set_sensitive(true);
                            // Failure toasts append the command output only
                            // when there is any (the sync-import summary has
                            // none, and a trailing ": " reads like a bug).
                            let message = if result.success || result.output.trim().is_empty() {
                                result.message.clone()
                            } else {
                                format!("{}: {}", result.message, result.output)
                            };
                            toast_overlay.add_toast(Toast::new(&message));
                            // Always rebuild the repo lists: the Repos event
                            // re-enables the Add/Enable buttons even when a
                            // repo action failed.
                            let _ = tx.try_send(CoreRequest::LoadRepos);
                            // Refresh on failure too: a partially failed sync
                            // import still installed some packages, so the
                            // Installed/Updates pages would otherwise be stale.
                            let _ = tx.try_send(CoreRequest::LoadUpdates);
                            let _ = tx.try_send(CoreRequest::LoadInstalled);
                            if let Some(query) = refresh_query(&last_query.borrow()) {
                                let _ = tx.try_send(CoreRequest::Search(query));
                            } else {
                                let _ = tx.try_send(CoreRequest::LoadTrending);
                            }
                        }
                        CoreEvent::ConfigReloaded => {
                            // Sources changed: refresh every page like a
                            // successful transaction does.
                            toast_overlay.add_toast(Toast::new("Settings applied"));
                            let _ = tx.try_send(CoreRequest::LoadStats);
                            let _ = tx.try_send(CoreRequest::LoadUpdates);
                            let _ = tx.try_send(CoreRequest::LoadInstalled);
                            if let Some(query) = refresh_query(&last_query.borrow()) {
                                let _ = tx.try_send(CoreRequest::Search(query));
                            } else {
                                let _ = tx.try_send(CoreRequest::LoadTrending);
                            }
                        }
                        CoreEvent::IconsReady(batch) => {
                            // Icons landed in the cache: rebind matching
                            // rows so they pick up the file.
                            for (app_id, path) in batch {
                                if path.is_none() {
                                    continue;
                                }
                                rows::rebind_matching(&trending_store, &app_id);
                                rows::rebind_matching(&spotlight_store, &app_id);
                                rows::rebind_matching(&installed_store, &app_id);
                                rows::rebind_matching(&updates_store, &app_id);
                            }
                        }
                        CoreEvent::Error(message) => {
                            toast_overlay.add_toast(Toast::new(&message));
                        }
                    },
                    // The event channel closed without a shutdown request:
                    // the worker thread is dead. Tell the user instead of
                    // silently breaking.
                    Err(_) => {
                        toast_overlay.add_toast(Toast::new("Brim core worker stopped"));
                        break;
                    }
                }
            }
        });
    }

    // Initial content: the Trending page.
    let _ = request_tx.try_send(CoreRequest::LoadTrending);

    window.present();
}

/// Add a scrollable virtualized list page to the view stack; returns the
/// store to fill and the stack toggling the empty state.
fn flow_page(
    stack: &ViewStack,
    name: &str,
    title: &str,
    icon: &str,
    on_action: rows::ActionFn,
    on_activate: rows::ActivateFn,
    pending_ids: Rc<RefCell<HashSet<String>>>,
) -> (gio::ListStore, gtk4::Stack) {
    let (view, store) = rows::package_list(on_action, on_activate, pending_ids);
    let scroll = ScrolledWindow::builder()
        .child(&view)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .build();
    let empty = Label::new(Some("Nothing here yet"));
    empty.add_css_class("dim-label");
    let page = gtk4::Stack::new();
    page.add_named(&scroll, Some("list"));
    page.add_named(&empty, Some("empty"));
    page.set_visible_child_name("empty");
    let clamp = Clamp::builder().child(&page).maximum_size(900).build();
    stack.add_titled_with_icon(&clamp, Some(name), title, icon);
    (store, page)
}

/// The Updates page: a stats line and an Upgrade All button above the list.
fn updates_page(
    stack: &ViewStack,
    request_tx: &Sender<CoreRequest>,
    upgrade_in_flight: &Rc<Cell<bool>>,
    on_action: rows::ActionFn,
    on_activate: rows::ActivateFn,
    pending_ids: Rc<RefCell<HashSet<String>>>,
) -> (gio::ListStore, gtk4::Stack, Label, Button) {
    let page = Box::new(Orientation::Vertical, 0);

    let bar = Box::new(Orientation::Horizontal, 12);
    bar.set_margin_start(12);
    bar.set_margin_end(12);
    bar.set_margin_top(12);

    let stats_label = Label::new(Some("Loading stats…"));
    stats_label.set_hexpand(true);
    stats_label.set_halign(Align::Start);
    stats_label.add_css_class("dim-label");
    bar.append(&stats_label);

    let upgrade_all = Button::with_label("Upgrade All");
    upgrade_all.add_css_class("suggested-action");
    {
        let tx = request_tx.clone();
        let in_flight = upgrade_in_flight.clone();
        upgrade_all.connect_clicked(move |button| {
            if in_flight.get() {
                return;
            }
            let dialog = adw::AlertDialog::builder()
                .heading("Upgrade all packages?")
                .body("This upgrades every package with a pending update across all sources.")
                .build();
            dialog.add_responses(&[("cancel", "Cancel"), ("upgrade", "Upgrade")]);
            dialog.set_response_appearance("upgrade", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let tx = tx.clone();
            let in_flight = in_flight.clone();
            let btn = button.clone();
            dialog.connect_response(None, move |_, response| {
                if response == "upgrade" {
                    if tx.try_send(CoreRequest::Upgrade).is_err() {
                        eprintln!("brim-gui: request channel full or closed; dropping upgrade");
                        // Not queued: leave the button usable.
                        return;
                    }
                    in_flight.set(true);
                    btn.set_sensitive(false);
                }
            });
            dialog.present(Some(button));
        });
    }
    bar.append(&upgrade_all);
    page.append(&bar);

    let (view, store) = rows::package_list(on_action, on_activate, pending_ids);
    let scroll = ScrolledWindow::builder()
        .child(&view)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .build();
    let empty = Label::new(Some("Nothing here yet"));
    empty.add_css_class("dim-label");
    let page_stack = gtk4::Stack::new();
    page_stack.add_named(&scroll, Some("list"));
    page_stack.add_named(&empty, Some("empty"));
    page_stack.set_visible_child_name("empty");
    page_stack.set_vexpand(true);
    let clamp = Clamp::builder()
        .child(&page_stack)
        .maximum_size(900)
        .build();
    page.append(&clamp);

    stack.add_titled_with_icon(
        &page,
        Some("updates"),
        "Updates",
        "software-update-available-symbolic",
    );
    (store, page_stack, stats_label, upgrade_all)
}

/// The Installed page: an Export/Import sync bar above the list. Both flows
/// only open file dialogs here (pure UI); all file I/O and brim-core calls
/// run in the worker — export answers `SyncExported`, import answers
/// `SyncParsed`, and the confirm dialog is presented from that event.
fn installed_page(
    stack: &ViewStack,
    request_tx: &Sender<CoreRequest>,
    window: &ApplicationWindow,
    on_action: rows::ActionFn,
    on_activate: rows::ActivateFn,
    pending_ids: Rc<RefCell<HashSet<String>>>,
) -> (gio::ListStore, gtk4::Stack) {
    let page = Box::new(Orientation::Vertical, 0);

    let bar = Box::new(Orientation::Horizontal, 12);
    bar.set_margin_start(12);
    bar.set_margin_end(12);
    bar.set_margin_top(12);

    let spacer = Label::new(None);
    spacer.set_hexpand(true);
    bar.append(&spacer);

    let export_button = Button::with_label("Export");
    {
        let tx = request_tx.clone();
        let window = window.clone();
        export_button.connect_clicked(move |_| {
            let tx = tx.clone();
            let window = window.clone();
            glib::spawn_future_local(async move {
                let dialog = gtk4::FileDialog::new();
                dialog.set_initial_name(Some("brim-sync.json"));
                // Err means the user cancelled the dialog.
                if let Ok(file) = dialog.save_future(Some(&window)).await {
                    if let Some(path) = file.path() {
                        if tx.try_send(CoreRequest::ExportSyncTo(path)).is_err() {
                            eprintln!(
                                "brim-gui: request channel full or closed; dropping sync export"
                            );
                        }
                    }
                }
            });
        });
    }
    bar.append(&export_button);

    let import_button = Button::with_label("Import");
    {
        let tx = request_tx.clone();
        let window = window.clone();
        import_button.connect_clicked(move |_| {
            let tx = tx.clone();
            let window = window.clone();
            glib::spawn_future_local(async move {
                let dialog = gtk4::FileDialog::new();
                let Ok(file) = dialog.open_future(Some(&window)).await else {
                    return; // cancelled
                };
                let Some(path) = file.path() else {
                    return;
                };
                // The worker reads and parses the file; the SyncParsed event
                // continues the flow (toast or confirm dialog).
                if tx.try_send(CoreRequest::ParseSyncFile(path)).is_err() {
                    eprintln!("brim-gui: request channel full or closed; dropping sync import");
                }
            });
        });
    }
    bar.append(&import_button);
    page.append(&bar);

    let (view, store) = rows::package_list(on_action, on_activate, pending_ids);
    let scroll = ScrolledWindow::builder()
        .child(&view)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .build();
    let empty = Label::new(Some("Nothing here yet"));
    empty.add_css_class("dim-label");
    let page_stack = gtk4::Stack::new();
    page_stack.add_named(&scroll, Some("list"));
    page_stack.add_named(&empty, Some("empty"));
    page_stack.set_visible_child_name("empty");
    page_stack.set_vexpand(true);
    let clamp = Clamp::builder()
        .child(&page_stack)
        .maximum_size(900)
        .build();
    page.append(&clamp);

    stack.add_titled_with_icon(
        &page,
        Some("installed"),
        "Installed",
        "package-x-generic-symbolic",
    );
    (store, page_stack)
}

/// Replace a page's contents with rows for `packages`. Missing icons are
/// queued for a CDN fetch first (config-gated); the IconsReady event rebinds
/// the rows so they pick up the freshly cached file.
fn populate(
    store: &gio::ListStore,
    page: &gtk4::Stack,
    packages: &[Package],
    request_tx: &Sender<CoreRequest>,
    config: &Rc<RefCell<brim_core::Config>>,
) {
    if config.borrow().gui.icon_downloads {
        for pkg in packages {
            if let Some(app_id) = icons::fetch_candidate(pkg) {
                if request_tx.try_send(CoreRequest::FetchIcon(app_id)).is_err() {
                    eprintln!("brim-gui: request channel full or closed; dropping icon fetch");
                    break;
                }
            }
        }
    }
    rows::fill(store, packages);
    page.set_visible_child_name(if packages.is_empty() { "empty" } else { "list" });
}

/// Run the row/dialog action button: confirm destructive removes, lock the
/// button, and dispatch the transaction. The Remove confirmation is presented
/// on `alert_parent` (the window) rather than the button: the button may live
/// inside a detail dialog that closes right after the click.
fn handle_row_action(
    pkg: &Package,
    button: &Button,
    alert_parent: &ApplicationWindow,
    request_tx: &Sender<CoreRequest>,
    pending_buttons: &Rc<RefCell<Vec<Button>>>,
    pending_ids: &Rc<RefCell<HashSet<String>>>,
) {
    if pkg.status == PackageStatus::Installed {
        // Removing is destructive: ask first.
        let dialog = adw::AlertDialog::builder()
            .heading("Remove package?")
            .body(format!(
                "This removes {} from the system. This cannot be undone.",
                pkg.name
            ))
            .build();
        dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let tx = request_tx.clone();
        let pending = pending_buttons.clone();
        let pending_ids = pending_ids.clone();
        let id = pkg.id.clone();
        let source = pkg.source;
        let btn = button.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "remove" {
                if tx
                    .try_send(CoreRequest::Remove(id.clone(), Some(source)))
                    .is_err()
                {
                    eprintln!("brim-gui: request channel full or closed; dropping remove");
                    // Not queued: leave the button usable.
                    return;
                }
                btn.set_sensitive(false);
                pending.borrow_mut().push(btn.clone());
                pending_ids.borrow_mut().insert(id.clone());
            }
        });
        dialog.present(Some(alert_parent));
    } else {
        // Install or Update: lock the button once the transaction is queued
        // so a double click cannot queue a duplicate. Locking before the
        // send would leave the button dead when the send fails.
        if request_tx
            .try_send(CoreRequest::Install(pkg.id.clone(), Some(pkg.source)))
            .is_err()
        {
            eprintln!("brim-gui: request channel full or closed; dropping install");
            return;
        }
        button.set_sensitive(false);
        pending_buttons.borrow_mut().push(button.clone());
        pending_ids.borrow_mut().insert(pkg.id.clone());
    }
}

/// Open the MD3 detail dialog for a package.
fn open_package_dialog(pkg: &Package, parent: &impl IsA<gtk4::Widget>, on_action: rows::ActionFn) {
    let dialog = adw::Dialog::new();
    dialog.set_title(&pkg.name);
    dialog.set_content_width(420);
    dialog.set_content_height(480);

    let toolbar = ToolbarView::new();
    toolbar.add_top_bar(&HeaderBar::new());

    let content = Box::new(Orientation::Vertical, 12);
    content.add_css_class("md3-dialog-content");

    let icon = match icons::resolve_immediate(pkg) {
        IconChoice::File(path) => Image::from_file(&path),
        IconChoice::Theme(name) => Image::from_icon_name(&name),
    };
    icon.set_pixel_size(64);
    content.append(&icon);

    let name = Label::new(Some(&pkg.name));
    name.add_css_class("md3-dialog-title");
    content.append(&name);

    let meta = Label::new(Some(&dialog_meta(pkg)));
    meta.add_css_class("md3-subtitle");
    content.append(&meta);

    let description = Label::new(Some(if pkg.description.trim().is_empty() {
        pkg.summary.trim()
    } else {
        pkg.description.trim()
    }));
    description.add_css_class("md3-body");
    description.set_wrap(true);
    description.set_xalign(0.0);
    content.append(&description);

    let (label, destructive) = match pkg.status {
        PackageStatus::Available => ("Install", false),
        PackageStatus::UpdateAvailable => ("Update", false),
        PackageStatus::Installed => ("Remove", true),
    };
    let button = Button::with_label(label);
    button.add_css_class(if destructive {
        "destructive-action"
    } else {
        "suggested-action"
    });
    {
        let pkg = pkg.clone();
        let dialog = dialog.clone();
        button.connect_clicked(move |button| {
            // Close first: the Remove confirmation is presented on the
            // window, so it must not race a dying parent dialog.
            dialog.close();
            on_action(&pkg, button);
        });
    }
    content.append(&button);

    let scroll = ScrolledWindow::builder().child(&content).build();
    toolbar.set_content(Some(&scroll));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));
}

/// Meta line for the detail dialog.
fn dialog_meta(pkg: &Package) -> String {
    let mut parts = vec![pkg.source.to_string()];
    if !pkg.version.is_empty() {
        parts.push(pkg.version.clone());
    }
    if let Some(license) = &pkg.license {
        parts.push(license.clone());
    }
    if pkg.downloads > 0 {
        parts.push(format!("{} installs/mo", format_count(pkg.downloads)));
    }
    parts.join(" · ")
}

/// Human-friendly count: 31.0M / 12.3k / 42.
fn format_count(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

/// The Settings page: shared-config switches backed by preferences groups.
fn settings_page(
    stack: &ViewStack,
    request_tx: &Sender<CoreRequest>,
    config: &Rc<RefCell<brim_core::Config>>,
) {
    let page = adw::PreferencesPage::new();

    let sources = adw::PreferencesGroup::builder()
        .title("Sources")
        .description("Disabled sources are skipped everywhere: search, lists, updates and stats.")
        .build();
    for (title, subtitle, key) in [
        ("Fedora (DNF5)", "Official Fedora RPMs", "sources.dnf5"),
        ("COPR", "Community projects", "sources.copr"),
        ("Flatpak", "Flathub applications", "sources.flatpak"),
    ] {
        let row = adw::SwitchRow::builder()
            .title(title)
            .subtitle(subtitle)
            .active(config.borrow().get(key).unwrap_or(true))
            .build();
        connect_config_switch(&row, request_tx, config, key);
        sources.add(&row);
    }
    page.add(&sources);

    let interface = adw::PreferencesGroup::builder().title("Interface").build();
    let icons = adw::SwitchRow::builder()
        .title("Download app icons")
        .subtitle("Fetch missing Flathub icons from the CDN")
        .active(config.borrow().gui.icon_downloads)
        .build();
    connect_config_switch(&icons, request_tx, config, "gui.icon_downloads");
    interface.add(&icons);
    page.add(&interface);

    stack.add_titled_with_icon(
        &page,
        Some("settings"),
        "Settings",
        "emblem-system-symbolic",
    );
}

/// Wire a SwitchRow to a config key: on toggle, update the shared config,
/// save it, and ask the worker to rebuild the manager.
fn connect_config_switch(
    row: &adw::SwitchRow,
    request_tx: &Sender<CoreRequest>,
    config: &Rc<RefCell<brim_core::Config>>,
    key: &'static str,
) {
    let tx = request_tx.clone();
    let config = config.clone();
    row.connect_active_notify(move |row| {
        {
            let mut cfg = config.borrow_mut();
            if !cfg.set(key, row.is_active()) {
                return;
            }
            if let Err(error) = cfg.save() {
                eprintln!("brim-gui: failed to save config: {error}");
                return;
            }
        }
        if tx.try_send(CoreRequest::ReloadConfig).is_err() {
            eprintln!("brim-gui: request channel full or closed; dropping config reload");
        }
    });
}

/// Handles for the Repositories page, so the Repos event can rebuild rows
/// and re-enable the add buttons.
#[derive(Clone)]
struct RepoGroups {
    flatpak_list: gtk4::ListBox,
    copr_list: gtk4::ListBox,
    flatpak_add_button: Button,
    copr_enable_button: Button,
}

/// A boxed-list container for data rows inside a preferences group.
fn repo_list_box() -> gtk4::ListBox {
    let list = gtk4::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk4::SelectionMode::None);
    list
}

/// The Repositories page (flatpak remotes + COPR repos).
fn repos_page(stack: &ViewStack, request_tx: &Sender<CoreRequest>) -> RepoGroups {
    let page = adw::PreferencesPage::new();

    // — Flatpak remotes —
    let flatpak = adw::PreferencesGroup::builder()
        .title("Flatpak Remotes")
        .build();
    let name_entry = adw::EntryRow::builder().title("Remote name").build();
    let url_entry = adw::EntryRow::builder()
        .title("Remote URL (.flatpakrepo)")
        .build();
    let add_button = Button::with_label("Add");
    add_button.add_css_class("suggested-action");
    add_button.set_valign(Align::Center);
    {
        let tx = request_tx.clone();
        let name_entry = name_entry.clone();
        let url_entry = url_entry.clone();
        add_button.connect_clicked(move |button| {
            let name = name_entry.text().trim().to_string();
            let url = url_entry.text().trim().to_string();
            if name.is_empty() || url.is_empty() {
                return;
            }
            if tx.try_send(CoreRequest::AddFlatpakRemote(name, url)).is_err() {
                eprintln!("brim-gui: request channel full or closed; dropping remote add");
                // Not queued: leave the button usable.
                return;
            }
            button.set_sensitive(false);
        });
    }
    url_entry.add_suffix(&add_button);
    flatpak.add(&name_entry);
    flatpak.add(&url_entry);
    let flatpak_list = repo_list_box();
    flatpak.add(&flatpak_list);
    page.add(&flatpak);

    // — COPR repos —
    let copr = adw::PreferencesGroup::builder().title("COPR Repos").build();
    let copr_entry = adw::EntryRow::builder().title("owner/project").build();
    let enable_button = Button::with_label("Enable");
    enable_button.add_css_class("suggested-action");
    enable_button.set_valign(Align::Center);
    {
        let tx = request_tx.clone();
        let copr_entry = copr_entry.clone();
        enable_button.connect_clicked(move |button| {
            let id = copr_entry.text().trim().to_string();
            if !id.contains('/') {
                return;
            }
            if tx.try_send(CoreRequest::SetCoprEnabled(id, true)).is_err() {
                eprintln!("brim-gui: request channel full or closed; dropping copr enable");
                // Not queued: leave the button usable.
                return;
            }
            button.set_sensitive(false);
        });
    }
    copr_entry.add_suffix(&enable_button);
    copr.add(&copr_entry);
    let copr_list = repo_list_box();
    copr.add(&copr_list);
    page.add(&copr);

    stack.add_titled_with_icon(
        &page,
        Some("repos"),
        "Repositories",
        "system-software-install-symbolic",
    );
    RepoGroups {
        flatpak_list,
        copr_list,
        flatpak_add_button: add_button,
        copr_enable_button: enable_button,
    }
}

/// Rebuild the repo rows of both groups and re-enable the add buttons.
/// `window` parents the confirmation dialogs: presenting on a row button is
/// unsafe because the next Repos event clears the lists and destroys it.
fn fill_repo_groups(
    groups: &RepoGroups,
    repos: &[RepoInfo],
    request_tx: &Sender<CoreRequest>,
    window: &ApplicationWindow,
) {
    clear_list(&groups.flatpak_list);
    clear_list(&groups.copr_list);
    groups.flatpak_add_button.set_sensitive(true);
    groups.copr_enable_button.set_sensitive(true);

    for repo in repos {
        let row = adw::ActionRow::builder()
            .title(&repo.title)
            .subtitle(if repo.url.is_empty() {
                repo.id.as_str()
            } else {
                repo.url.as_str()
            })
            .build();
        match repo.kind {
            brim_core::RepoKind::FlatpakRemote => {
                let remove = Button::with_label("Remove");
                remove.add_css_class("destructive-action");
                remove.set_valign(Align::Center);
                let tx = request_tx.clone();
                let id = repo.id.clone();
                let window = window.clone();
                remove.connect_clicked(move |_| {
                    let tx = tx.clone();
                    let id = id.clone();
                    confirm_simple(
                        &window,
                        "Remove remote?",
                        &format!(
                            "This removes the flatpak remote '{id}'. Apps installed from it stay installed."
                        ),
                        "Remove",
                        move || {
                            let _ = tx.try_send(CoreRequest::RemoveFlatpakRemote(id.clone()));
                        },
                    );
                });
                row.add_suffix(&remove);
                groups.flatpak_list.append(&row);
            }
            brim_core::RepoKind::CoprRepo => {
                let disable = Button::with_label("Disable");
                disable.add_css_class("destructive-action");
                disable.set_valign(Align::Center);
                let tx = request_tx.clone();
                let id = repo.id.clone();
                let window = window.clone();
                disable.connect_clicked(move |_| {
                    let tx = tx.clone();
                    let id = id.clone();
                    confirm_simple(
                        &window,
                        "Disable COPR repo?",
                        &format!("This disables '{id}'. Packages installed from it are kept."),
                        "Disable",
                        move || {
                            let _ = tx.try_send(CoreRequest::SetCoprEnabled(id.clone(), false));
                        },
                    );
                });
                row.add_suffix(&disable);
                groups.copr_list.append(&row);
            }
        }
    }
}

/// Remove every row of a ListBox.
fn clear_list(list: &gtk4::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

/// Small shared confirmation dialog for destructive repo actions.
fn confirm_simple(
    parent: &impl IsA<gtk4::Widget>,
    heading: &str,
    body: &str,
    action_label: &str,
    on_confirm: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("ok", action_label)]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
        if response == "ok" {
            on_confirm();
        }
    });
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_latest_nonempty_query() {
        assert!(should_dispatch("htop", 3, 3));
    }

    #[test]
    fn no_dispatch_for_empty_or_blank_query() {
        assert!(!should_dispatch("", 1, 1));
        assert!(!should_dispatch("   ", 1, 1));
    }

    #[test]
    fn no_dispatch_for_stale_generation() {
        assert!(!should_dispatch("htop", 2, 3));
        assert!(!should_dispatch("htop", 0, 5));
    }

    #[test]
    fn refresh_query_reruns_last_search() {
        assert_eq!(refresh_query("htop"), Some("htop".to_string()));
    }

    #[test]
    fn refresh_query_skips_empty_last_query() {
        assert_eq!(refresh_query(""), None);
        assert_eq!(refresh_query("   "), None);
    }

    #[test]
    fn accepts_results_for_the_current_query() {
        assert!(accepts_results("htop", "htop"));
        // Trimmed compare: stray whitespace must not drop fresh results.
        assert!(accepts_results(" htop ", "htop"));
        assert!(accepts_results("htop", " htop"));
    }

    #[test]
    fn rejects_stale_results() {
        assert!(!accepts_results("firefox", "htop"));
        // A cleared page (empty last query) drops any in-flight result.
        assert!(!accepts_results("htop", ""));
    }

    #[test]
    fn dialog_meta_skips_unknown_fields() {
        let mut pkg = Package::new("htop.x86_64", "htop", SourceType::FedoraOfficial);
        assert_eq!(dialog_meta(&pkg), "Fedora");
        pkg.version = "3.4.1".to_string();
        pkg.license = Some("GPL-2.0-only".to_string());
        pkg.downloads = 1200;
        assert_eq!(
            dialog_meta(&pkg),
            "Fedora · 3.4.1 · GPL-2.0-only · 1.2k installs/mo"
        );
    }

    #[test]
    fn format_count_scales() {
        assert_eq!(format_count(42), "42");
        assert_eq!(format_count(12_300), "12.3k");
        assert_eq!(format_count(31_000_000), "31.0M");
    }
}

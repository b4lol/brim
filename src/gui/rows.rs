//! Package list rows and the virtualized list plumbing.
//!
//! Every store page is a `gtk4::ListView` over a `gio::ListStore` of
//! `BoxedAnyObject(Rc<Package>)`: only visible rows exist, so a 2000-hit
//! search costs ~20 widgets instead of 2000.
//!
//! Recycling constraint: a row's widgets are created once (`setup`) and
//! rebound many times (`bind`), so signal handlers must be connected in
//! setup and read the *current* package from per-row state. The state
//! lives in a registry indexed by the row root's widget name (`row-<n>`) —
//! safe Rust, no GObject data pointers.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use crate::core::{Package, PackageStatus};
use gtk4::gio;
use gtk4::glib::BoxedAnyObject;
use gtk4::pango::EllipsizeMode;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, Image, Label, ListItem, ListView, Orientation, SignalListItemFactory,
    SingleSelection,
};

use crate::gui::icons::{self, IconChoice};

/// Action callbacks shared by every list and the detail dialog.
pub type ActionFn = Rc<dyn Fn(&Package, &Button)>;
/// Activation callback (row clicked → detail dialog).
pub type ActivateFn = Rc<dyn Fn(&Package)>;

/// Per-row widget references plus the currently bound package.
struct RowWidgets {
    icon: Image,
    title: Label,
    subtitle: Label,
    badge: Label,
    button: Button,
    pkg: Rc<RefCell<Option<Rc<Package>>>>,
}

type Registry = Rc<RefCell<Vec<Rc<RowWidgets>>>>;

/// Build a virtualized package list and its store. `on_action` runs for
/// Install/Update/Remove clicks; `on_activate` for row activation. Rows
/// whose package id is in `pending_ids` (a transaction is in flight for
/// them) keep their button insensitive, even after recycling.
pub fn package_list(
    on_action: ActionFn,
    on_activate: ActivateFn,
    pending_ids: Rc<RefCell<HashSet<String>>>,
) -> (ListView, gio::ListStore) {
    let store = gio::ListStore::new::<BoxedAnyObject>();
    let selection = SingleSelection::new(Some(store.clone()));
    selection.set_autoselect(false);

    let registry: Registry = Rc::new(RefCell::new(Vec::new()));
    let factory = SignalListItemFactory::new();

    {
        let registry = registry.clone();
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<ListItem>().expect("setup item");
            let (root, widgets) = build_row(on_action.clone());
            let index = registry.borrow().len();
            root.set_widget_name(&format!("row-{index}"));
            registry.borrow_mut().push(Rc::new(widgets));
            item.set_child(Some(&root));
        });
    }

    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<ListItem>().expect("bind item").clone();
        let Some(obj) = item.item().and_downcast::<BoxedAnyObject>() else {
            return;
        };
        let pkg: Rc<Package> = obj.borrow::<Rc<Package>>().clone();
        let Some(root) = item.child() else {
            return;
        };
        let Some(index) = root
            .widget_name()
            .strip_prefix("row-")
            .and_then(|n| n.parse::<usize>().ok())
        else {
            return;
        };
        let Some(widgets) = registry.borrow().get(index).cloned() else {
            return;
        };
        *widgets.pkg.borrow_mut() = Some(pkg.clone());
        bind_row(&widgets, &pkg, &pending_ids.borrow());
    });

    let view = ListView::new(Some(selection), Some(factory));
    view.set_single_click_activate(true);
    view.connect_activate(move |view, position| {
        let Some(model) = view.model() else {
            return;
        };
        let Some(obj) = model
            .item(position)
            .and_then(|o| o.downcast::<BoxedAnyObject>().ok())
        else {
            return;
        };
        let pkg: Rc<Package> = obj.borrow::<Rc<Package>>().clone();
        on_activate(&pkg);
    });
    (view, store)
}

/// Replace a store's contents with rows for `packages`.
pub fn fill(store: &gio::ListStore, packages: &[Package]) {
    store.remove_all();
    let items: Vec<BoxedAnyObject> = packages
        .iter()
        .map(|p| BoxedAnyObject::new(Rc::new(p.clone())))
        .collect();
    store.extend_from_slice(&items);
}

/// Rebind positions whose package matches `app_id` (icon arrived → the
/// rebind picks the freshly cached file). Cheap: stores are page-sized.
pub fn rebind_matching(store: &gio::ListStore, app_id: &str) {
    for position in 0..store.n_items() {
        let Some(obj) = store
            .item(position)
            .and_then(|o| o.downcast::<BoxedAnyObject>().ok())
        else {
            continue;
        };
        let matches = {
            let pkg = obj.borrow::<Rc<Package>>();
            pkg.flatpak_ref.as_deref() == Some(app_id) || pkg.id == app_id
        };
        if matches {
            store.splice(position, 1, &[obj]);
        }
    }
}

/// The empty row skeleton; widgets are filled on bind.
fn build_row(on_action: ActionFn) -> (Box, RowWidgets) {
    let root = Box::new(Orientation::Horizontal, 12);
    root.add_css_class("package-row");

    let icon = Image::new();
    icon.set_pixel_size(40);
    icon.set_margin_top(4);
    icon.set_margin_bottom(4);
    root.append(&icon);

    let texts = Box::new(Orientation::Vertical, 2);
    texts.set_hexpand(true);
    texts.set_valign(Align::Center);
    let title = Label::new(None);
    title.add_css_class("heading");
    title.set_xalign(0.0);
    title.set_ellipsize(EllipsizeMode::End);
    let subtitle = Label::new(None);
    subtitle.add_css_class("dim-label");
    subtitle.add_css_class("caption");
    subtitle.set_xalign(0.0);
    subtitle.set_ellipsize(EllipsizeMode::End);
    texts.append(&title);
    texts.append(&subtitle);
    root.append(&texts);

    let badge = Label::new(None);
    badge.set_valign(Align::Center);
    root.append(&badge);

    let button = Button::new();
    button.set_valign(Align::Center);
    root.append(&button);

    let widgets = RowWidgets {
        icon,
        title,
        subtitle,
        badge,
        button: button.clone(),
        pkg: Rc::new(RefCell::new(None)),
    };

    // Connected once (setup): reads the *currently bound* package.
    let cell = widgets.pkg.clone();
    button.connect_clicked(move |button| {
        if let Some(pkg) = cell.borrow().clone() {
            on_action(&pkg, button);
        }
    });

    (root, widgets)
}

/// Fill a recycled row with `pkg`'s data. The button stays insensitive
/// while a transaction for this package id is in flight.
fn bind_row(widgets: &RowWidgets, pkg: &Package, pending_ids: &HashSet<String>) {
    match icons::resolve_immediate(pkg) {
        IconChoice::File(path) => widgets.icon.set_from_file(Some(&path)),
        IconChoice::Theme(name) => widgets.icon.set_icon_name(Some(&name)),
    }
    widgets.title.set_text(&pkg.name);
    widgets.subtitle.set_text(&row_subtitle(pkg));

    widgets.badge.set_text(&pkg.source.to_string());
    for class in ["badge-fedora", "badge-copr", "badge-flatpak"] {
        widgets.badge.remove_css_class(class);
    }
    widgets.badge.add_css_class(pkg.source.badge_class());

    let (label, destructive) = match pkg.status {
        PackageStatus::Available => ("Install", false),
        PackageStatus::UpdateAvailable => ("Update", false),
        PackageStatus::Installed => ("Remove", true),
    };
    widgets.button.set_label(label);
    widgets.button.set_sensitive(!pending_ids.contains(&pkg.id));
    widgets.button.remove_css_class("suggested-action");
    widgets.button.remove_css_class("destructive-action");
    widgets.button.add_css_class(if destructive {
        "destructive-action"
    } else {
        "suggested-action"
    });
}

/// Supporting text: version plus summary when known.
fn row_subtitle(pkg: &Package) -> String {
    let summary = pkg.summary.trim();
    if pkg.version.is_empty() {
        summary.to_string()
    } else if summary.is_empty() {
        pkg.version.clone()
    } else {
        format!("{} · {}", pkg.version, summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SourceType;

    #[test]
    fn row_subtitle_combines_version_and_summary() {
        let mut pkg = Package::new("org.example.app", "App", SourceType::Flatpak);
        assert_eq!(row_subtitle(&pkg), "");
        pkg.summary = "Does things".to_string();
        assert_eq!(row_subtitle(&pkg), "Does things");
        pkg.version = "1.0".to_string();
        assert_eq!(row_subtitle(&pkg), "1.0 · Does things");
    }
}

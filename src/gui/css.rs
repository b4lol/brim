//! Application-wide styling: native libadwaita look. The app follows the
//! system light/dark style; only row layout and source badges need custom
//! rules, and those use libadwaita palette variables so they adapt to both.

use gtk4::gdk::Display;
use gtk4::{CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION};

const CSS: &str = r#"
/* — List rows — */

.package-row {
    min-height: 56px;
    padding: 8px 16px;
    border-bottom: 1px solid alpha(currentColor, 0.1);
}

.package-row:hover {
    background-color: alpha(currentColor, 0.06);
}

/* — Source badges (palette tints adapt to light and dark) — */

.badge-fedora,
.badge-copr,
.badge-flatpak {
    border-radius: 8px;
    padding: 2px 8px;
    font-size: 0.75rem;
    font-weight: 500;
}

.badge-fedora {
    background-color: alpha(@blue_2, 0.35);
}

.badge-copr {
    background-color: alpha(@purple_2, 0.35);
}

.badge-flatpak {
    background-color: alpha(@green_2, 0.35);
}
"#;

/// Load the Brim stylesheet onto the default display.
pub fn load_css() {
    let provider = CssProvider::new();
    provider.load_from_data(CSS);
    if let Some(display) = Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

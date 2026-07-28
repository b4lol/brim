//! Application-wide styling: Material Design 3 baseline (dark).

use gtk4::gdk::Display;
use gtk4::{CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION};

const CSS: &str = r#"
/* Material Design 3 baseline (dark) for Brim. */

window {
    background-color: #131314;
    color: #e3e3e3;
}

/* — List rows — */

.md3-list row,
.md3-list {
    background-color: transparent;
}

.md3-row {
    min-height: 56px;
    padding: 8px 16px;
    border-bottom: 1px solid #444746; /* outline-variant */
}

.md3-row:hover {
    background-color: alpha(#e3e3e3, 0.08);
}

.md3-row-icon {
    margin: 4px 0;
}

.md3-title {
    font-weight: 500;
    font-size: 1rem; /* title-medium */
    color: #e3e3e3; /* on-surface */
}

.md3-subtitle {
    font-size: 0.875rem; /* body-medium */
    color: #c4c7c7; /* on-surface-variant */
}

.md3-body {
    font-size: 0.875rem;
    color: #e3e3e3;
}

/* — Source badges — */

.badge-fedora,
.badge-copr,
.badge-flatpak {
    border-radius: 8px;
    padding: 4px 10px;
    font-size: 0.75rem;
    font-weight: 500;
}

.badge-fedora {
    background-color: #a8c7fa; /* primary-80 */
    color: #0b305f;
}

.badge-copr {
    background-color: #efb8c8; /* tertiary-80 */
    color: #492532;
}

.badge-flatpak {
    background-color: #8dd5c0; /* secondary-80 */
    color: #00382d;
}

/* — Dialog — */

.md3-dialog-content {
    padding: 24px;
}

.md3-dialog-title {
    font-size: 1.375rem; /* headline-small */
    font-weight: 500;
}

/* — Misc — */

.dim-label {
    color: #c4c7c7;
}
"#;

/// Load the Brim Material Design 3 stylesheet onto the default display.
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

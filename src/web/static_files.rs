//! Static SPA assets embedded into the binary at compile time.

/// The single-page application shell.
pub const INDEX_HTML: &str = include_str!("../../static/index.html");

/// Stylesheet for the SPA.
pub const STYLE_CSS: &str = include_str!("../../static/style.css");

/// Client-side application logic.
pub const APP_JS: &str = include_str!("../../static/app.js");

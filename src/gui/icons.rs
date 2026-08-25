//! Package icon resolution.
//!
//! brim-core gives every package a placeholder `icon_name`; real logos come
//! from four sources, tried in order:
//!
//! 1. Local flatpak exports (installed flatpaks — no network needed).
//! 2. Brim's icon cache (`~/.cache/brim/icons`), filled from the Flathub CDN.
//! 3. The system icon theme by package name (installed RPMs — hicolor).
//! 4. An Adwaita symbolic category icon as the themed fallback.

use std::path::PathBuf;

use crate::core::{Category, Package, SourceType};

/// How a card should render its icon right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconChoice {
    /// A real image file on disk (flatpak export or CDN cache).
    File(PathBuf),
    /// A themed icon name: the app's real logo from the icon theme when the
    /// package ships one (installed RPMs), else the category fallback.
    Theme(String),
}

/// Brim's icon cache directory (`~/.cache/brim/icons`), or `None` when no
/// per-user cache directory can be determined. Unlike a `/tmp` fallback,
/// `None` cannot be squatted by another local user.
pub fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("brim").join("icons"))
}

/// Flatpak app ids are reverse-DNS (`org.example.App`): ASCII letters,
/// digits, dots, underscores and dashes. The id comes from untrusted remote
/// metadata, so anything else (notably `/` or `..`) must never reach a file
/// path or URL.
fn is_safe_app_id(app_id: &str) -> bool {
    !app_id.is_empty()
        && app_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Flathub's AppStream CDN icon URL for an application id.
pub fn flathub_icon_url(app_id: &str) -> String {
    format!("https://dl.flathub.org/repo/appstream/x86_64/icons/128x128/{app_id}.png")
}

/// The cache file an icon for `app_id` would live at, if downloaded.
pub fn cached_icon(app_id: &str) -> Option<PathBuf> {
    if !is_safe_app_id(app_id) {
        return None;
    }
    let path = cache_dir()?.join(format!("{app_id}.png"));
    path.is_file().then_some(path)
}

/// Icon shipped by an installed flatpak, without any network access.
pub fn installed_flatpak_icon(app_id: &str) -> Option<PathBuf> {
    if !is_safe_app_id(app_id) {
        return None;
    }
    // No per-user cache directory means HOME is unset; the user-level
    // exports path below would then be relative to the CWD — skip disk
    // lookup entirely.
    cache_dir()?;
    let rel = format!("icons/hicolor/128x128/apps/{app_id}.png");
    let candidates = [
        PathBuf::from("/var/lib/flatpak/exports/share").join(&rel),
        std::env::var_os("HOME")
            .map(|home| {
                PathBuf::from(home)
                    .join(".local/share/flatpak/exports/share")
                    .join(&rel)
            })
            .unwrap_or_default(),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

/// Themed fallback per category (Adwaita symbolic icons).
pub fn category_icon(category: Category) -> &'static str {
    match category {
        Category::Development => "applications-engineering-symbolic",
        Category::Gaming => "applications-games-symbolic",
        Category::Productivity => "applications-utilities-symbolic",
        Category::System => "applications-system-symbolic",
        Category::Multimedia => "applications-multimedia-symbolic",
        Category::Graphics => "applications-graphics-symbolic",
        Category::Utilities => "applications-utilities-symbolic",
    }
}

/// Whether the current GTK icon theme ships an icon under `name` (hicolor
/// lookup). This is how installed RPMs get their real logo: the package name
/// usually matches the application icon name (firefox, vlc, htop, …).
/// Always `false` when there is no display (tests, headless).
fn theme_has_icon(name: &str) -> bool {
    let Some(display) = gtk4::gdk::Display::default() else {
        return false;
    };
    gtk4::IconTheme::for_display(&display).has_icon(name)
}

/// Resolve the best icon available *without* any network access.
pub fn resolve_immediate(pkg: &Package) -> IconChoice {
    resolve_with(pkg, &theme_has_icon)
}

/// [`resolve_immediate`] with the theme lookup injected, so tests stay
/// hermetic (a test host may have a display and a differently-populated
/// icon theme).
fn resolve_with(pkg: &Package, theme_has: &dyn Fn(&str) -> bool) -> IconChoice {
    if pkg.source == SourceType::Flatpak {
        let app_id = pkg.flatpak_ref.as_deref().unwrap_or(&pkg.id);
        if pkg.status == crate::core::PackageStatus::Installed {
            if let Some(path) = installed_flatpak_icon(app_id) {
                return IconChoice::File(path);
            }
        }
        if let Some(path) = cached_icon(app_id) {
            return IconChoice::File(path);
        }
    }
    if theme_has(&pkg.name) {
        return IconChoice::Theme(pkg.name.clone());
    }
    IconChoice::Theme(category_icon(pkg.category).to_string())
}

/// Whether a CDN fetch is worth attempting for this package: flatpaks from
/// Flathub (or an unknown remote) whose icon is not already local.
pub fn should_fetch(pkg: &Package) -> bool {
    if pkg.source != SourceType::Flatpak {
        return false;
    }
    if matches!(pkg.flatpak_remote.as_deref(), Some(remote) if remote != "flathub") {
        return false;
    }
    !matches!(resolve_immediate(pkg), IconChoice::File(_))
}

/// The app id worth a CDN fetch for this package, if any (see
/// [`should_fetch`]). Returns the flatpak ref when known, else the package id.
pub fn fetch_candidate(pkg: &Package) -> Option<String> {
    if !should_fetch(pkg) {
        return None;
    }
    Some(pkg.flatpak_ref.clone().unwrap_or_else(|| pkg.id.clone()))
}

/// Download a Flathub icon into the cache. Returns the cached path on
/// success; the themed fallback stays in place on failure.
pub async fn fetch_flathub_icon(client: &reqwest::Client, app_id: &str) -> Option<PathBuf> {
    if !is_safe_app_id(app_id) {
        return None;
    }
    if let Some(path) = cached_icon(app_id) {
        return Some(path);
    }
    // No per-user cache directory: skip the fetch — the bytes would have
    // no persistent home, and the row keeps its themed fallback.
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let bytes = crate::core::http::get_bytes(client, &flathub_icon_url(app_id))
        .await
        .ok()?;
    let tmp = dir.join(format!("{app_id}.part"));
    let target = dir.join(format!("{app_id}.png"));
    std::fs::write(&tmp, bytes).ok()?;
    std::fs::rename(&tmp, &target).ok()?;
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SourceType;

    #[test]
    fn unsafe_app_ids_are_rejected() {
        assert!(is_safe_app_id("org.kde.kcalc"));
        assert!(is_safe_app_id("org.example.App_2-x"));
        assert!(!is_safe_app_id(""));
        assert!(!is_safe_app_id("../evil"));
        assert!(!is_safe_app_id("org/example"));
        assert!(!is_safe_app_id("app/org.example.App/x86_64/stable"));
    }

    #[test]
    fn flathub_url_follows_cdn_layout() {
        assert_eq!(
            flathub_icon_url("org.kde.kcalc"),
            "https://dl.flathub.org/repo/appstream/x86_64/icons/128x128/org.kde.kcalc.png"
        );
    }

    #[test]
    fn category_icons_cover_every_category() {
        for category in [
            Category::Development,
            Category::Gaming,
            Category::Productivity,
            Category::System,
            Category::Multimedia,
            Category::Graphics,
            Category::Utilities,
        ] {
            assert!(category_icon(category).ends_with("-symbolic"));
        }
    }

    #[test]
    fn rpm_packages_use_the_category_fallback() {
        let pkg = Package::new("htop.x86_64", "htop", SourceType::FedoraOfficial);
        // Theme lookup stubbed out: no installed icon, so the category
        // fallback must win.
        assert_eq!(
            resolve_with(&pkg, &|_| false),
            IconChoice::Theme(category_icon(pkg.category).to_string())
        );
    }

    #[test]
    fn rpm_packages_use_the_theme_icon_when_the_theme_ships_one() {
        let pkg = Package::new("htop.x86_64", "htop", SourceType::FedoraOfficial);
        assert_eq!(
            resolve_with(&pkg, &|name| name == "htop"),
            IconChoice::Theme("htop".to_string())
        );
    }

    #[test]
    fn non_flathub_remotes_are_never_fetched() {
        let mut pkg = Package::new("org.example.app", "app", SourceType::Flatpak);
        pkg.flatpak_remote = Some("fedora".to_string());
        assert!(!should_fetch(&pkg));
        pkg.flatpak_remote = Some("flathub".to_string());
        assert!(should_fetch(&pkg));
    }

    #[test]
    fn fetch_candidate_prefers_the_flatpak_ref() {
        let mut pkg = Package::new("org.example.app", "app", SourceType::Flatpak);
        assert_eq!(fetch_candidate(&pkg).as_deref(), Some("org.example.app"));
        pkg.flatpak_ref = Some("org.example.app.desktop".to_string());
        assert_eq!(
            fetch_candidate(&pkg).as_deref(),
            Some("org.example.app.desktop")
        );
        pkg.flatpak_remote = Some("fedora".to_string());
        assert_eq!(fetch_candidate(&pkg), None);
        let rpm = Package::new("htop.x86_64", "htop", SourceType::FedoraOfficial);
        assert_eq!(fetch_candidate(&rpm), None);
    }
}

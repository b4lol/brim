//! Flathub trending apps (the "popular" collection) with a 24h disk cache.
//!
//! One request returns 250 fully-described hits — no per-app follow-ups.
//! The raw response is cached in `~/.cache/brim/trending.json`; freshness
//! is the file mtime, so the parser stays pure and the cache human-readable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::core::error::Result;
use crate::core::models::{Category, Package, SourceType};

/// Flathub's popular collection endpoint.
const POPULAR_URL: &str = "https://flathub.org/api/v2/collection/popular";

/// How long a cached response is considered fresh.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Fetch the trending list: fresh cache → network → stale cache → empty.
///
/// A fresh cache that parses to an empty list counts as a miss (treated as
/// corrupt): otherwise a truncated cache file would pin the UI to an empty
/// trending page for a full TTL. When no cache directory is available (no
/// `XDG_CACHE_HOME` and no `HOME`), caching is skipped entirely rather than
/// falling back to a shared location like `/tmp`.
pub async fn trending(client: &reqwest::Client) -> Vec<Package> {
    let path = cache_file();
    if let Some(path) = &path {
        if cache_is_fresh(path).await {
            if let Ok(text) = tokio::fs::read_to_string(path).await {
                let cached = parse_popular(&text);
                if !cached.is_empty() {
                    return cached;
                }
            }
        }
    }
    match crate::core::http::get_text(client, POPULAR_URL).await {
        Ok(text) => {
            let parsed = parse_popular(&text);
            if parsed.is_empty() {
                // An unparseable 2xx body (error page, endpoint shape
                // change) must not clobber a usable cache.
                return read_stale_cache(path.as_deref()).await;
            }
            if let Some(path) = &path {
                let _ = write_cache(path, &text).await;
            }
            parsed
        }
        Err(_) => read_stale_cache(path.as_deref()).await,
    }
}

/// Last resort: whatever the cache holds, parsed (empty when there is no
/// cache path, or the file is missing, unreadable, or unparseable).
async fn read_stale_cache(path: Option<&Path>) -> Vec<Package> {
    let Some(path) = path else {
        return Vec::new();
    };
    tokio::fs::read_to_string(path)
        .await
        .ok()
        .map(|text| parse_popular(&text))
        .unwrap_or_default()
}

/// Parse the popular-collection response into packages (pure, total).
pub fn parse_popular(text: &str) -> Vec<Package> {
    #[derive(Deserialize)]
    struct Hit {
        app_id: String,
        name: String,
        #[serde(default)]
        summary: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        installs_last_month: u64,
        #[serde(default)]
        project_license: Option<String>,
        // The endpoint is inconsistent: most hits send a string ("game"),
        // but some send an empty array ([]) — accept both shapes.
        #[serde(default)]
        main_categories: Option<MainCategories>,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum MainCategories {
        One(String),
        Many(Vec<String>),
    }
    #[derive(Deserialize)]
    struct Response {
        // Raw values: each hit is parsed individually below so that one
        // malformed hit cannot zero the whole list.
        hits: Vec<serde_json::Value>,
    }
    let Ok(response) = serde_json::from_str::<Response>(text) else {
        return Vec::new();
    };
    response
        .hits
        .into_iter()
        .filter_map(|value| serde_json::from_value::<Hit>(value).ok())
        .map(|hit| {
            let mut pkg = Package::new(hit.app_id.clone(), hit.name, SourceType::Flatpak);
            pkg.flatpak_ref = Some(hit.app_id);
            pkg.summary = hit.summary.split_whitespace().collect::<Vec<_>>().join(" ");
            pkg.description = hit
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            pkg.downloads = hit.installs_last_month;
            pkg.license = hit.project_license.filter(|l| !l.trim().is_empty());
            pkg.category = match &hit.main_categories {
                Some(MainCategories::One(category)) => map_category(category),
                Some(MainCategories::Many(categories)) => categories
                    .first()
                    .map(|c| map_category(c))
                    .unwrap_or(Category::Utilities),
                None => Category::Utilities,
            };
            pkg
        })
        .collect()
}

/// Map Flathub's category ids onto Brim categories.
fn map_category(flathub: &str) -> Category {
    match flathub {
        "development" => Category::Development,
        "game" | "games" => Category::Gaming,
        "office" => Category::Productivity,
        "system" => Category::System,
        "audio-video" | "audiovideo" | "audio" | "video" => Category::Multimedia,
        "graphics" => Category::Graphics,
        _ => Category::Utilities,
    }
}

/// Whether the cache file exists and is younger than the TTL.
pub async fn cache_is_fresh(path: &Path) -> bool {
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    modified
        .elapsed()
        .map(|age| age < CACHE_TTL)
        .unwrap_or(false)
}

/// Write the raw response to the cache file atomically (parent dirs
/// created).
pub async fn write_cache(path: &Path, text: &str) -> Result<()> {
    crate::core::fsutil::write_atomic(path, text).await
}

/// The trending cache path (`~/.cache/brim/trending.json`), or `None`
/// when no per-user cache directory can be determined. Unlike a `/tmp`
/// fallback, `None` cannot be squatted by another local user.
fn cache_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("brim").join("trending.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::SourceType;

    // POPULAR_OUT: 3-hit excerpt captured from the real endpoint on
    // 2026-07-27 (see plan Task 2 Step 1). The third hit (org.winehq.Wine)
    // is included because it is a real `"main_categories": []` hit.
    const POPULAR_OUT: &str = r#"{
 "hits": [
  {
   "name": "Sober",
   "keywords": [
    "roblox",
    "vinegar",
    "launcher"
   ],
   "localized_keywords": [
    "launcher",
    "roblox",
    "vinegar"
   ],
   "summary": "Play, chat & explore on Roblox",
   "description": "Not affiliated with Roblox. Research project, use at your own risk. Read the notice on our website before using.\n      We ported Roblox to Linux because they wouldn't.\n      \n        Enjoy millions of experiences on Roblox, chat with friends, and explore!\n        Seamlessly run Roblox on Linux, faster than on Windows.\n        No emulators. No virtual machines. No cruft. We disable ads and telemetry by default.\n        No strings attached - Sober is a community project. We made it closed source to reduce the potential for abuse (which would lead to Roblox blocking us again)\n      \n      Make Sober your own. See our documentation for a list of options.\n      \n        Turn on server location indicators, bring back the nostalgic oof sound, customize textures, try experimental settings, and more.\n      \n    ",
   "id": "org_vinegarhq_Sober",
   "type": "desktop-application",
   "translations": {},
   "project_license": "LicenseRef-proprietary=https://sober.vinegarhq.org/notice.txt",
   "is_free_license": false,
   "app_id": "org.vinegarhq.Sober",
   "icon": "https://dl.flathub.org/media/org/vinegarhq/Sober/53a35f34cedfbd06c961a0f84bfb076f/icons/128x128/org.vinegarhq.Sober.png",
   "main_categories": "game",
   "sub_categories": [
    "GNOME",
    "GTK"
   ],
   "developer_name": "VinegarHQ & Sober contributors",
   "verification_verified": true,
   "verification_method": "website",
   "verification_login_name": null,
   "verification_login_provider": null,
   "verification_login_is_organization": false,
   "verification_website": "vinegarhq.org",
   "verification_timestamp": "1743317849",
   "runtime": "org.gnome.Platform/x86_64/50",
   "updated_at": 1784683732,
   "arches": [
    "x86_64"
   ],
   "added_at": 1743316841,
   "trending": 18.37308360749495,
   "installs_last_month": 219415,
   "favorites_count": 305,
   "isMobileFriendly": false
  },
  {
   "name": "Firefox",
   "keywords": [
    "Browser",
    "Explorer",
    "Internet"
   ],
   "localized_keywords": [
    "Browser",
    "Explorer",
    "Internet"
   ],
   "summary": "Fast, Private & Safe Web Browser",
   "description": "When it comes to your life online, you have a choice: accept the factory settings or put your privacy first. When you choose Firefox as your default browser, you’re choosing to protect your data while supporting an independent tech company. Firefox is also the only major browser backed by a non-profit fighting to give you more openness, transparency and control of your life online. Join hundreds of millions of people who choose to protect what's important by choosing Firefox - a web browser designed to be fast, easy to use, customizable and private.",
   "id": "org_mozilla_firefox",
   "type": "desktop-application",
   "project_license": "MPL-2.0",
   "is_free_license": true,
   "app_id": "org.mozilla.firefox",
   "icon": "https://dl.flathub.org/media/icons/128x128/org.mozilla.firefox.png",
   "main_categories": "network",
   "sub_categories": [
    "WebBrowser"
   ],
   "developer_name": "Mozilla",
   "verification_verified": true,
   "verification_method": "manual",
   "verification_login_name": null,
   "verification_login_provider": null,
   "verification_login_is_organization": false,
   "verification_website": null,
   "verification_timestamp": "1675948428",
   "runtime": "org.freedesktop.Platform/x86_64/25.08",
   "updated_at": 1784637787,
   "arches": [
    "aarch64",
    "x86_64"
   ],
   "added_at": 1689081703,
   "trending": 13.035758381190332,
   "installs_last_month": 210158,
   "favorites_count": 425,
   "isMobileFriendly": false
  },
  {
   "name": "Wine",
   "keywords": null,
   "localized_keywords": null,
   "summary": "Run Windows applications on Linux",
   "description": "\n      Wine (originally an acronym for \"Wine Is Not an Emulator\") is a compatibility\n      layer capable of running Windows applications on several POSIX-compliant\n      operating systems, such as Linux, Mac OSX, and BSD. Instead of simulating\n      internal Windows logic like a virtual machine or emulator, Wine translates\n      Windows API calls into POSIX calls on-the-fly, eliminating the performance\n      and memory penalties of other methods and allowing you to cleanly integrate\n      Windows applications into your desktop.\n    \n      This Flatpak also provides Winetricks. To use it, run:\n      \n        flatpak run --command=winetricks org.winehq.Wine\n      \n    ",
   "id": "org_winehq_Wine",
   "type": "console-application",
   "translations": {},
   "project_license": "LGPL-2.1+",
   "is_free_license": true,
   "app_id": "org.winehq.Wine",
   "icon": "https://dl.flathub.org/media/org/winehq/Wine/b1c2eabe6cab85ae5a45d9062f7ca208/icons/128x128/org.winehq.Wine.png",
   "main_categories": [],
   "sub_categories": [],
   "developer_name": "Wine Project",
   "verification_verified": false,
   "verification_method": "none",
   "verification_login_name": null,
   "verification_login_provider": null,
   "verification_login_is_organization": null,
   "verification_website": null,
   "verification_timestamp": null,
   "runtime": "org.freedesktop.Platform/x86_64/25.08",
   "updated_at": 1783085542,
   "arches": [
    "x86_64"
   ],
   "added_at": 1646075272,
   "trending": 10.381137359588651,
   "installs_last_month": 24888,
   "favorites_count": 96,
   "isMobileFriendly": false
  }
 ],
 "query": "",
 "processingTimeMs": 14,
 "hitsPerPage": 250,
 "page": 1,
 "totalPages": 14,
 "totalHits": 3282,
 "facetDistribution": null,
 "facetStats": null
}"#;

    #[test]
    fn parse_popular_maps_hits_to_packages() {
        let pkgs = parse_popular(POPULAR_OUT);
        assert_eq!(pkgs.len(), 3);
        let first = &pkgs[0];
        assert_eq!(first.source, SourceType::Flatpak);
        assert_eq!(first.flatpak_ref.as_deref(), Some(first.id.as_str()));
        assert!(!first.id.is_empty());
        assert!(!first.name.is_empty());
        assert!(first.downloads > 0);
    }

    #[test]
    fn parse_popular_tolerates_string_and_array_main_categories() {
        // Regression: the live endpoint mixes "main_categories": "game"
        // with "main_categories": [] (org.winehq.Wine); an array hit must
        // not zero the whole response.
        let pkgs = parse_popular(POPULAR_OUT);
        assert_eq!(pkgs.len(), 3);
        let sober = pkgs.iter().find(|p| p.id == "org.vinegarhq.Sober").unwrap();
        assert_eq!(sober.category, Category::Gaming);
        let wine = pkgs.iter().find(|p| p.id == "org.winehq.Wine").unwrap();
        assert_eq!(wine.category, Category::Utilities);
        assert_eq!(wine.downloads, 24888);
    }

    #[test]
    fn parse_popular_skips_a_malformed_hit() {
        // One structurally broken hit must not void the good ones.
        let text = r#"{"hits": [
            {"app_id": "org.good.App", "name": "Good", "main_categories": "audiovideo"},
            {"app_id": 42, "name": null}
        ]}"#;
        let pkgs = parse_popular(text);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].id, "org.good.App");
        assert_eq!(pkgs[0].category, Category::Multimedia);
    }

    #[test]
    fn map_category_covers_live_endpoint_ids() {
        // Ids observed on the live endpoint (2026-07-27): audiovideo,
        // development, education, game, graphics, network, office,
        // system, utility.
        assert_eq!(map_category("audiovideo"), Category::Multimedia);
        assert_eq!(map_category("development"), Category::Development);
        assert_eq!(map_category("game"), Category::Gaming);
        assert_eq!(map_category("graphics"), Category::Graphics);
        assert_eq!(map_category("office"), Category::Productivity);
        assert_eq!(map_category("system"), Category::System);
        assert_eq!(map_category("network"), Category::Utilities);
        assert_eq!(map_category("utility"), Category::Utilities);
        assert_eq!(map_category("education"), Category::Utilities);
    }

    #[test]
    fn parse_popular_is_total_on_garbage() {
        assert!(parse_popular("").is_empty());
        assert!(parse_popular("{not json").is_empty());
        assert!(parse_popular(r#"{"hits": "nope"}"#).is_empty());
    }

    #[tokio::test]
    async fn cache_round_trip_and_freshness() {
        let path =
            std::env::temp_dir().join(format!("brim-test-{}-trending.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(!cache_is_fresh(&path).await);
        write_cache(&path, POPULAR_OUT).await.unwrap();
        assert!(cache_is_fresh(&path).await);
        assert_eq!(
            parse_popular(&std::fs::read_to_string(&path).unwrap()).len(),
            3
        );
        let _ = std::fs::remove_file(&path);
    }
}

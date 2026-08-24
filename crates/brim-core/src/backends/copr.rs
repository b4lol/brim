//! COPR backend: discovers projects via the COPR API and manages the
//! repositories through the `dnf copr` plugin (dnf5 era).
//!
//! On dnf5-era systems `dnf copr` has no `search` subcommand (only
//! list/enable/disable/remove), so search and info use the read-only
//! COPR API (`https://copr.fedorainfracloud.org/api_3/...`) via reqwest
//! (native HTTP). All output parsers are pure functions so they can be
//! unit tested without ever executing `dnf` or `dnf5`. Every process spawn
//! forces `LC_ALL=C` so parsers can rely on English output.

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

use crate::backend::Backend;
use crate::backends::{probe, validate_arg, QUERY_TIMEOUT};
use crate::error::{BrimError, Result};
use crate::models::{
    Package, RepoInfo, RepoKind, SourceType, TransactionAction, TransactionResult,
};

/// COPR API endpoint returning matching projects as `{"items": [...]}`.
const COPR_API_SEARCH: &str = "https://copr.fedorainfracloud.org/api_3/project/search";

/// COPR API root; a cheap, always-2xx endpoint used to probe reachability.
/// (The search endpoint itself is too slow for a probe — even successful
/// searches take ~10 s — and a bare GET of it 400s.)
const COPR_API_ROOT: &str = "https://copr.fedorainfracloud.org/api_3/";

/// Backend for packages from COPR (Fedora's community build service).
#[derive(Debug, Clone)]
pub struct CoprBackend {
    /// Shared HTTP client for COPR API requests.
    http: reqwest::Client,
    /// Cached result of the first availability probe.
    available: OnceLock<bool>,
}

impl CoprBackend {
    /// Create a new COPR backend instance (with a fresh availability cache).
    pub fn new() -> Self {
        Self {
            http: crate::http::client(),
            available: OnceLock::new(),
        }
    }

    /// Run `program` with the given arguments and return stdout on success.
    ///
    /// Any non-zero exit yields [`BrimError::CommandFailed`] carrying stderr.
    /// `timeout` bounds the wait (queries); pass `None` for transactions,
    /// which may take long.
    async fn run(&self, program: &str, args: &[&str], timeout: Option<Duration>) -> Result<String> {
        let future = Command::new(program)
            .args(args)
            // Force English output so the parsers see stable messages.
            .env("LC_ALL", "C")
            // A timed-out query must not leave the child running.
            .kill_on_drop(true)
            .output();
        let output = match timeout {
            Some(limit) => match tokio::time::timeout(limit, future).await {
                Ok(result) => result.map_err(|e| super::spawn_error(program, e))?,
                Err(_) => {
                    return Err(BrimError::CommandFailed(format!("{program} timed out")));
                }
            },
            None => future.await.map_err(|e| super::spawn_error(program, e))?,
        };

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let code = output.status.code().unwrap_or(-1);
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                format!("{program} exited with code {code}")
            } else {
                stderr
            };
            Err(BrimError::CommandFailed(detail))
        }
    }

    /// Transactions need the `dnf copr` plugin (reads use the COPR API).
    async fn ensure_copr_plugin(&self) -> Result<()> {
        if probe("dnf", &["copr", "--help"]).await {
            Ok(())
        } else {
            Err(BrimError::BackendUnavailable("dnf copr plugin".to_string()))
        }
    }
}

impl Default for CoprBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Backend for CoprBackend {
    fn source(&self) -> SourceType {
        SourceType::Copr
    }

    async fn is_available(&self) -> bool {
        if let Some(&cached) = self.available.get() {
            return cached;
        }
        // Reads need only the COPR API: availability is a short probe of
        // the API host itself (network reachability). Probe the API root:
        // a bare GET of the search endpoint 400s, and real searches take
        // ~10 s, which would outrun the 5 s probe window.
        let probe = crate::http::get_text(&self.http, COPR_API_ROOT);
        let available = tokio::time::timeout(std::time::Duration::from_secs(5), probe)
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false);
        let _ = self.available.set(available);
        available
    }

    async fn search(&self, query: &str) -> Result<Vec<Package>> {
        validate_arg(query)?;
        let out =
            crate::http::get_text_query(&self.http, COPR_API_SEARCH, &[("query", query)]).await?;
        Ok(parse_search(&out))
    }

    async fn list_installed(&self) -> Result<Vec<Package>> {
        // COPR has no per-repo package inventory of its own: installed RPMs
        // (including ones from COPR repos) are owned by the dnf5 backend.
        Ok(vec![])
    }

    async fn info(&self, id: &str) -> Result<Package> {
        validate_arg(id)?;
        // Reuse the search endpoint: query the project name and keep the
        // exact `owner/project` match.
        let Some((_, project)) = id.split_once('/') else {
            return Err(BrimError::NotFound(id.to_string()));
        };
        let pkgs = self.search(project).await?;
        pkgs.into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| BrimError::NotFound(id.to_string()))
    }

    /// Install from a COPR project — deliberately **best effort**.
    ///
    /// Semantics (kept stable since v0.1.0, success flag corrected in v0.2.0):
    ///
    /// 1. The repository is enabled first via `dnf copr enable -y owner/project`.
    ///    If that fails, the whole transaction is an `Err` — nothing changed
    ///    on the system that the user did not ask for.
    /// 2. The package is then installed with `dnf5 install -y <project>`.
    ///    COPR project names usually match the package name, but not always;
    ///    when they differ, this step fails while the repo stays enabled.
    /// 3. That partial outcome is reported as `success: false` with a message
    ///    that discloses the repo WAS enabled — the user asked for a package
    ///    that is not installed, so the transaction is not a success, but the
    ///    enabled repo is left in place deliberately (it is the standard COPR
    ///    workflow and is what `remove` mirrors by disabling again).
    async fn install(&self, pkg: &Package) -> Result<TransactionResult> {
        self.ensure_copr_plugin().await?;
        validate_arg(&pkg.id)?;
        // Enable the repository first, then install the package. The repo
        // enable succeeding while the package step fails is reported as a
        // failed transaction (the message discloses that the repo was
        // enabled), because the package the user asked for is not there.
        let mut out = self
            .run("dnf", &["copr", "enable", "-y", &pkg.id], None)
            .await?;
        let project = pkg.copr_project.as_deref().unwrap_or(&pkg.name);
        validate_arg(project)?;
        let install_err = match self.run("dnf5", &["install", "-y", project], None).await {
            Ok(install_out) => {
                out.push_str(&install_out);
                None
            }
            Err(e) => Some(e.to_string()),
        };
        let (success, message) = install_message(&pkg.id, project, install_err.as_deref());
        Ok(TransactionResult {
            success,
            action: TransactionAction::Install,
            package_id: pkg.id.clone(),
            message,
            output: out,
        })
    }

    async fn remove(&self, pkg: &Package) -> Result<TransactionResult> {
        self.ensure_copr_plugin().await?;
        validate_arg(&pkg.id)?;
        // Even when the package name does not match the project name (so
        // `dnf5 remove` fails), the repo must still be disabled — otherwise
        // it stays enabled with no recovery path through this backend. A
        // failed package step makes the transaction unsuccessful (the
        // message discloses that the repo was disabled).
        let name = pkg.copr_project.as_deref().unwrap_or(&pkg.name);
        validate_arg(name)?;
        let remove_result = self.run("dnf5", &["remove", "-y", name], None).await;
        let remove_err = remove_result.as_ref().err().map(|e| e.to_string());
        let mut out = remove_result.unwrap_or_default();
        out.push_str(
            &self
                .run("dnf", &["copr", "disable", "-y", &pkg.id], None)
                .await?,
        );
        let (success, message) = remove_message(&pkg.id, name, remove_err.as_deref());
        Ok(TransactionResult {
            success,
            action: TransactionAction::Remove,
            package_id: pkg.id.clone(),
            message,
            output: out,
        })
    }

    async fn updates(&self) -> Result<Vec<Package>> {
        // Update detection for COPR packages is handled by the dnf5 backend
        // (`dnf5 check-update` already covers enabled COPR repos).
        Ok(vec![])
    }

    async fn upgrade(&self) -> Result<TransactionResult> {
        // Deliberately a no-op: upgrading COPR packages is delegated to the
        // dnf5 backend to avoid running two system-wide upgrades.
        Ok(TransactionResult::ok(
            TransactionAction::Upgrade,
            "copr",
            "COPR packages are upgraded by the dnf5 backend",
            "",
        ))
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>> {
        let out = self
            .run("dnf", &["copr", "list"], Some(QUERY_TIMEOUT))
            .await?;
        Ok(parse_copr_list(&out))
    }

    async fn add_repo(&self, id: &str, _url: &str) -> Result<TransactionResult> {
        self.set_repo_enabled(id, true).await
    }

    async fn set_repo_enabled(&self, id: &str, enabled: bool) -> Result<TransactionResult> {
        validate_arg(id)?;
        let action = if enabled { "enable" } else { "disable" };
        let out = self.run("dnf", &["copr", action, "-y", id], None).await?;
        Ok(TransactionResult::ok(
            TransactionAction::RepoChange,
            id,
            format!("COPR repo '{id}' {action}d"),
            out,
        ))
    }
}

/// Parse `dnf copr list` output: one `host/owner/project [flags]` per
/// line (enabled repos only). Pure and total.
fn parse_copr_list(text: &str) -> Vec<RepoInfo> {
    text.lines()
        .filter_map(|line| {
            let path = line.split_whitespace().next()?;
            let (_, owner_project) = path.split_once('/')?;
            if owner_project.is_empty() {
                return None;
            }
            Some(RepoInfo {
                id: owner_project.to_string(),
                title: owner_project.to_string(),
                url: String::new(),
                kind: RepoKind::CoprRepo,
                // `dnf copr list` enumerates enabled repos only.
                enabled: true,
            })
        })
        .collect()
}

/// Parse the JSON body of the COPR API `project/search` endpoint.
///
/// The response is `{"items": [...]}` where each item carries `full_name`
/// (`owner/project`, owner may be an `@group`), `ownername`, `name`, a
/// free-form markdown `description`, and an optional `homepage`. Malformed
/// JSON or items without a usable `full_name` are skipped, never panicked on.
pub fn parse_search(output: &str) -> Vec<Package> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return Vec::new();
    };
    let Some(items) = value.get("items").and_then(|i| i.as_array()) else {
        return Vec::new();
    };
    items.iter().filter_map(project_to_package).collect()
}

/// Convert one API `items` entry into a [`Package`]; `None` when the entry
/// has no usable `owner/project` full name.
fn project_to_package(item: &serde_json::Value) -> Option<Package> {
    let full_name = item.get("full_name")?.as_str()?;
    let (owner, project) = full_name.split_once('/')?;
    if owner.is_empty() || project.is_empty() {
        return None;
    }
    let mut pkg = Package::new(full_name, project, SourceType::Copr);
    pkg.summary = item
        .get("description")
        .and_then(|d| d.as_str())
        .map(first_meaningful_line)
        .unwrap_or_default();
    pkg.refresh_category();
    pkg.homepage = item
        .get("homepage")
        .and_then(|h| h.as_str())
        .filter(|h| !h.is_empty())
        .map(str::to_string);
    pkg.copr_owner = Some(owner.to_string());
    pkg.copr_project = Some(project.to_string());
    Some(pkg)
}

/// Build the outcome of the package step of an `install` transaction:
/// `(success, user-facing message)`. The repo enable step has already
/// succeeded at this point, so a failed package install is a failed
/// transaction whose message discloses that the repo was enabled.
fn install_message(id: &str, project: &str, install_err: Option<&str>) -> (bool, String) {
    match install_err {
        None => (true, format!("enabled {id} and installed {project}")),
        Some(e) => (false, format!("enabled {id}; package install failed: {e}")),
    }
}

/// Build the outcome of the package step of a `remove` transaction:
/// `(success, user-facing message)`. The repo disable step has already
/// succeeded at this point, so a failed package remove is a failed
/// transaction whose message discloses that the repo was disabled.
fn remove_message(id: &str, name: &str, remove_err: Option<&str>) -> (bool, String) {
    match remove_err {
        None => (true, format!("removed {name} and disabled {id}")),
        Some(e) => (false, format!("disabled {id}; package remove failed: {e}")),
    }
}

/// First non-empty line of a free-form description, with common markdown
/// decorations (`#`, `*`, `` ` ``) stripped from the start.
fn first_meaningful_line(description: &str) -> String {
    for line in description.lines() {
        let trimmed = line.trim().trim_start_matches(['#', '*', '`']).trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SourceType;

    // Shape of the real COPR API response (verified on Fedora 44,
    // https://copr.fedorainfracloud.org/api_3/project/search?query=ghostty):
    // `full_name` is `owner/project`, group owners keep their `@` prefix,
    // descriptions are free-form markdown and may be empty.
    const SEARCH_OUT: &str = r##"{"items": [
        {"name": "ghostty", "ownername": "@ghostty", "full_name": "@ghostty/ghostty",
         "description": "Ghostty terminal emulator", "homepage": "https://ghostty.org"},
        {"name": "ghostty", "ownername": "cube1ber", "full_name": "cube1ber/ghostty",
         "description": "# Ghostty\r\n\r\n[![badge](https://img.shields.io/badge)]",
         "homepage": null},
        {"name": "ghostty", "ownername": "guillermodotn", "full_name": "guillermodotn/ghostty",
         "description": "", "homepage": null}
    ]}"##;

    // Captured 2026-07-27 from `dnf copr list` (enabled repos only).
    const COPR_LIST_OUT: &str = "copr.fedorainfracloud.org/che/nerd-fonts\ncopr.fedorainfracloud.org/erikreider/SwayNotificationCenter\ncopr.fedorainfracloud.org/errornointernet/quickshell\ncopr.fedorainfracloud.org/mineiro/hyprland\ncopr.fedorainfracloud.org/peterwu/rendezvous\ncopr.fedorainfracloud.org/tofik/nwg-shell [eternal_deps]\n";

    #[test]
    fn parse_copr_list_strips_host_and_flags() {
        let repos = parse_copr_list(COPR_LIST_OUT);
        assert_eq!(repos.len(), 6);
        assert_eq!(repos[0].id, "che/nerd-fonts");
        assert_eq!(repos[5].id, "tofik/nwg-shell");
        assert!(repos
            .iter()
            .all(|r| r.enabled && r.kind == RepoKind::CoprRepo));
    }

    #[test]
    fn parse_copr_list_is_total_on_garbage() {
        assert!(parse_copr_list("").is_empty());
        assert!(parse_copr_list("no-slash-here\n").is_empty());
    }

    #[test]
    fn parses_copr_search() {
        let pkgs = parse_search(SEARCH_OUT);
        assert_eq!(pkgs.len(), 3);
        assert_eq!(pkgs[0].id, "@ghostty/ghostty");
        assert_eq!(pkgs[0].name, "ghostty");
        assert_eq!(pkgs[0].copr_owner.as_deref(), Some("@ghostty"));
        assert_eq!(pkgs[0].copr_project.as_deref(), Some("ghostty"));
        assert_eq!(pkgs[0].source, SourceType::Copr);
        assert_eq!(pkgs[0].summary, "Ghostty terminal emulator");
        assert_eq!(pkgs[0].homepage.as_deref(), Some("https://ghostty.org"));
    }

    #[test]
    fn search_uses_first_description_line_as_summary() {
        let pkgs = parse_search(SEARCH_OUT);
        assert_eq!(pkgs[1].summary, "Ghostty");
        assert_eq!(pkgs[1].copr_owner.as_deref(), Some("cube1ber"));
        assert!(pkgs[1].homepage.is_none());
        assert_eq!(pkgs[2].summary, "");
    }

    #[test]
    fn search_skips_items_without_full_name() {
        let out = r#"{"items": [
            {"name": "no-full-name", "ownername": "x", "description": "d"},
            {"full_name": "missing-slash", "description": "d"},
            {"full_name": "/empty-owner", "description": "d"},
            {"full_name": "ok/project", "description": "fine"}
        ]}"#;
        let pkgs = parse_search(out);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].id, "ok/project");
        assert_eq!(pkgs[0].copr_owner.as_deref(), Some("ok"));
        assert_eq!(pkgs[0].copr_project.as_deref(), Some("project"));
    }

    #[test]
    fn parser_never_panics_on_empty_or_malformed() {
        assert!(parse_search("").is_empty());
        assert!(parse_search("not json at all").is_empty());
        assert!(parse_search("{}").is_empty());
        assert!(parse_search(r#"{"items": "oops"}"#).is_empty());
        assert!(parse_search(r#"{"items": []}"#).is_empty());
        assert!(parse_search(r#"{"items": [42, null, "x"]}"#).is_empty());
    }

    #[test]
    fn install_message_reports_full_success() {
        let (success, msg) = install_message("owner/project", "project", None);
        assert!(success);
        assert_eq!(msg, "enabled owner/project and installed project");
    }

    #[test]
    fn install_message_reports_package_step_failure() {
        let (success, msg) = install_message(
            "owner/project",
            "project",
            Some("No match for argument: project"),
        );
        assert!(!success);
        assert!(msg.starts_with("enabled owner/project; package install failed: "));
        assert!(msg.contains("No match for argument: project"));
    }

    #[test]
    fn remove_message_reports_full_success() {
        let (success, msg) = remove_message("owner/project", "project", None);
        assert!(success);
        assert_eq!(msg, "removed project and disabled owner/project");
    }

    #[test]
    fn remove_message_reports_package_step_failure() {
        let (success, msg) = remove_message(
            "owner/project",
            "project",
            Some("No match for argument: project"),
        );
        assert!(!success);
        assert!(msg.starts_with("disabled owner/project; package remove failed: "));
        assert!(msg.contains("No match for argument: project"));
    }
}

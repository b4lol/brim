//! Flatpak backend: wraps the `flatpak` CLI (Flathub and other remotes).
//!
//! All output parsers in this module are pure functions so they can be unit
//! tested without ever executing `flatpak`. Every process spawn forces
//! `LC_ALL=C` so parsers can rely on English headers and field names.

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

use crate::backend::Backend;
use crate::backends::{probe, validate_arg, QUERY_TIMEOUT};
use crate::error::{BrimError, Result};
use crate::models::{
    Package, PackageStatus, RepoInfo, RepoKind, SourceType, TransactionAction, TransactionResult,
};

/// Backend for packages managed by Flatpak (Flathub and other remotes).
#[derive(Debug, Default, Clone)]
pub struct FlatpakBackend {
    /// Cached result of the first availability probe.
    available: OnceLock<bool>,
}

impl FlatpakBackend {
    /// Create a new Flatpak backend instance (with a fresh availability cache).
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `flatpak` with the given arguments and return stdout on success.
    ///
    /// Any non-zero exit yields [`BrimError::CommandFailed`] carrying stderr.
    /// `timeout` bounds the wait (queries); pass `None` for transactions,
    /// which may take long.
    async fn run(&self, args: &[&str], timeout: Option<Duration>) -> Result<String> {
        let future = Command::new("flatpak")
            .args(args)
            // Force English output so the parsers see stable headers/fields.
            .env("LC_ALL", "C")
            // A timed-out query must not leave the flatpak child running.
            .kill_on_drop(true)
            .output();
        let output = match timeout {
            Some(limit) => match tokio::time::timeout(limit, future).await {
                Ok(result) => result.map_err(|e| super::spawn_error("flatpak", e))?,
                Err(_) => {
                    return Err(BrimError::CommandFailed("flatpak timed out".to_string()));
                }
            },
            None => future.await.map_err(|e| super::spawn_error("flatpak", e))?,
        };

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let code = output.status.code().unwrap_or(-1);
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                format!("flatpak exited with code {code}")
            } else {
                stderr
            };
            Err(BrimError::CommandFailed(detail))
        }
    }
}

#[async_trait]
impl Backend for FlatpakBackend {
    fn source(&self) -> SourceType {
        SourceType::Flatpak
    }

    async fn is_available(&self) -> bool {
        if let Some(&cached) = self.available.get() {
            return cached;
        }
        let available = probe("flatpak", &["--version"]).await;
        let _ = self.available.set(available);
        available
    }

    async fn search(&self, query: &str) -> Result<Vec<Package>> {
        validate_arg(query)?;
        let out = self
            .run(
                &[
                    "search",
                    "--columns=name,description,application,version,branch,remotes",
                    // `--` keeps user input from being parsed as a flag.
                    "--",
                    query,
                ],
                Some(QUERY_TIMEOUT),
            )
            .await?;
        Ok(parse_search(&out))
    }

    async fn list_installed(&self) -> Result<Vec<Package>> {
        let out = self
            .run(
                &[
                    "list",
                    "--app",
                    "--columns=name,description,application,version",
                ],
                Some(QUERY_TIMEOUT),
            )
            .await?;
        Ok(parse_list(&out))
    }

    async fn info(&self, id: &str) -> Result<Package> {
        validate_arg(id)?;
        let out = self.run(&["info", "--", id], Some(QUERY_TIMEOUT)).await?;
        parse_info(&out).ok_or_else(|| BrimError::NotFound(id.to_string()))
    }

    async fn install(&self, pkg: &Package) -> Result<TransactionResult> {
        let app_id = pkg.flatpak_ref.as_deref().unwrap_or(&pkg.id);
        validate_arg(app_id)?;
        let args = install_args(pkg);
        let verb = if args[0] == "update" {
            "updated"
        } else {
            "installed"
        };
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.run(&argv, None).await?;
        Ok(TransactionResult::ok(
            TransactionAction::Install,
            app_id,
            format!("{verb} {app_id}"),
            out,
        ))
    }

    async fn remove(&self, pkg: &Package) -> Result<TransactionResult> {
        let app_id = pkg.flatpak_ref.as_deref().unwrap_or(&pkg.id);
        validate_arg(app_id)?;
        let out = self.run(&["uninstall", "-y", "--", app_id], None).await?;
        Ok(TransactionResult::ok(
            TransactionAction::Remove,
            app_id,
            format!("removed {app_id}"),
            out,
        ))
    }

    async fn updates(&self) -> Result<Vec<Package>> {
        let out = self
            .run(
                &["remote-ls", "--updates", "--columns=application,version"],
                Some(QUERY_TIMEOUT),
            )
            .await?;
        Ok(parse_updates(&out))
    }

    async fn upgrade(&self) -> Result<TransactionResult> {
        let out = self.run(&["update", "-y"], None).await?;
        Ok(TransactionResult::ok(
            TransactionAction::Upgrade,
            "system",
            "upgraded all flatpaks",
            out,
        ))
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>> {
        // `--show-disabled` is required: flatpak 1.18 hides disabled remotes
        // from `flatpak remotes` by default.
        let out = self
            .run(
                &[
                    "remotes",
                    "--show-disabled",
                    "--columns=name,title,url,options",
                ],
                Some(QUERY_TIMEOUT),
            )
            .await?;
        Ok(parse_remotes(&out))
    }

    async fn add_repo(&self, id: &str, url: &str) -> Result<TransactionResult> {
        validate_arg(id)?;
        let out = self
            .run(
                &["remote-add", "--user", "--if-not-exists", "--", id, url],
                None,
            )
            .await?;
        Ok(TransactionResult::ok(
            TransactionAction::RepoChange,
            id,
            format!("Remote '{id}' added"),
            out,
        ))
    }

    async fn remove_repo(&self, id: &str) -> Result<TransactionResult> {
        validate_arg(id)?;
        // remote-delete defaults to --system; user-level remotes (like the
        // ones add_repo creates) need an explicit --user retry.
        let out = match self.run(&["remote-delete", "--", id], None).await {
            Ok(out) => out,
            Err(first) => {
                self.run(&["remote-delete", "--user", "--", id], None)
                    .await
                    // Keep the original error visible: the --user retry
                    // failing must not mask why the system-level delete
                    // failed (e.g. a permission problem).
                    .map_err(|second| {
                        BrimError::CommandFailed(format!(
                            "{first}; --user retry also failed: {second}"
                        ))
                    })?
            }
        };
        Ok(TransactionResult::ok(
            TransactionAction::RepoChange,
            id,
            format!("Remote '{id}' removed"),
            out,
        ))
    }
}

/// Parse `flatpak search --columns=name,description,application,version,branch,remotes`.
///
/// Rows are TAB-separated with no header; the remotes column may hold a
/// comma-separated list (e.g. `fedora,flathub`). Rows without an application
/// id are skipped.
pub fn parse_search(output: &str) -> Vec<Package> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.trim();
            let summary = fields.next().unwrap_or("").trim();
            let app_id = fields.next().unwrap_or("").trim();
            if app_id.is_empty() {
                return None;
            }
            let version = fields.next().unwrap_or("").trim();
            let _branch = fields.next();
            let remotes = fields.next().unwrap_or("").trim();
            let mut pkg = Package::new(app_id, display_name(name, app_id), SourceType::Flatpak);
            pkg.summary = summary.to_string();
            pkg.refresh_category();
            pkg.version = version.to_string();
            pkg.flatpak_ref = Some(app_id.to_string());
            pkg.flatpak_remote = pick_remote(remotes);
            Some(pkg)
        })
        .collect()
}

/// Pick the remote to install from: prefer `flathub` when it is one of the
/// comma-separated remotes, otherwise take the first one. `None` when the
/// remotes column is empty.
fn pick_remote(remotes: &str) -> Option<String> {
    let remotes: Vec<&str> = remotes
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .collect();
    if remotes.is_empty() {
        None
    } else if remotes.contains(&"flathub") {
        Some("flathub".to_string())
    } else {
        Some(remotes[0].to_string())
    }
}

/// Parse `flatpak list --app --columns=name,description,application,version`.
///
/// Same TAB-separated shape as search; parsed packages are marked installed.
/// The list output carries no remote information, so `flatpak_remote`
/// stays `None`.
pub fn parse_list(output: &str) -> Vec<Package> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.trim();
            let summary = fields.next().unwrap_or("").trim();
            let app_id = fields.next().unwrap_or("").trim();
            if app_id.is_empty() {
                return None;
            }
            let version = fields.next().unwrap_or("").trim();
            let mut pkg = Package::new(app_id, display_name(name, app_id), SourceType::Flatpak);
            pkg.summary = summary.to_string();
            pkg.refresh_category();
            pkg.version = version.to_string();
            pkg.installed_version = Some(version.to_string());
            pkg.status = PackageStatus::Installed;
            pkg.flatpak_ref = Some(app_id.to_string());
            Some(pkg)
        })
        .collect()
}

/// Parse `flatpak remote-ls --updates --columns=application,version`.
///
/// Only the application id and the available version are known, so the id
/// doubles as the display name. Lines without a TAB-separated version
/// column are not table rows (noise on stdout) and are skipped, matching
/// the sibling parsers.
pub fn parse_updates(output: &str) -> Vec<Package> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let app_id = fields.next()?.trim();
            if app_id.is_empty() {
                return None;
            }
            let version = fields.next()?.trim();
            let mut pkg = Package::new(app_id, app_id, SourceType::Flatpak);
            pkg.version = version.to_string();
            pkg.status = PackageStatus::UpdateAvailable;
            pkg.flatpak_ref = Some(app_id.to_string());
            Some(pkg)
        })
        .collect()
}

/// Parse `flatpak info <ref>` output into a single package.
///
/// The first non-empty line is `Name - Summary`; the remaining lines are
/// right-aligned `Key: Value` fields. Returns `None` when no `ID` field is
/// present (unknown ref or empty output).
pub fn parse_info(output: &str) -> Option<Package> {
    let mut name: Option<String> = None;
    let mut summary = String::new();
    let mut app_id: Option<String> = None;
    let mut version = String::new();
    let mut license: Option<String> = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // The first non-empty line is the `Name - Summary` header.
        if name.is_none() && !trimmed.contains(':') {
            let (n, s) = trimmed.split_once(" - ").unwrap_or((trimmed, ""));
            name = Some(n.trim().to_string());
            summary = s.trim().to_string();
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        match key.trim() {
            "ID" => app_id = Some(value.trim().to_string()),
            "Version" => version = value.trim().to_string(),
            "License" => license = Some(value.trim().to_string()),
            _ => {}
        }
    }

    let app_id = app_id?;
    let mut pkg = Package::new(
        &app_id,
        display_name(name.as_deref().unwrap_or(""), &app_id),
        SourceType::Flatpak,
    );
    pkg.summary = summary;
    pkg.refresh_category();
    // `flatpak info` only covers installed refs, so the reported version is
    // the installed one (consistent with `parse_list`).
    pkg.installed_version = Some(version.clone());
    pkg.version = version;
    pkg.license = license;
    pkg.status = PackageStatus::Installed;
    pkg.flatpak_ref = Some(app_id);
    Some(pkg)
}

/// Parse `flatpak remotes --show-disabled --columns=name,title,url,options`.
/// Pure and total: malformed lines are skipped.
fn parse_remotes(text: &str) -> Vec<RepoInfo> {
    text.lines()
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let name = cols.next()?.trim();
            if name.is_empty() || !line.contains('\t') {
                // Real output always has TABs; TAB-less lines are malformed.
                return None;
            }
            let title = cols.next().unwrap_or("").trim();
            let url = cols.next().unwrap_or("").trim().to_string();
            let options = cols.next().unwrap_or("");
            Some(RepoInfo {
                id: name.to_string(),
                title: if title.is_empty() || title == "-" {
                    name.to_string()
                } else {
                    title.to_string()
                },
                url,
                kind: RepoKind::FlatpakRemote,
                enabled: !options.split(',').any(|o| o.trim() == "disabled"),
            })
        })
        .collect()
}

/// Use the Flatpak-provided name, falling back to the application id when
/// the name column is empty.
fn display_name<'a>(name: &'a str, app_id: &'a str) -> &'a str {
    if name.is_empty() {
        app_id
    } else {
        name
    }
}

/// Build the flatpak command line for installing `pkg`.
///
/// Unlike `dnf5 install`, `flatpak install` on an already-installed ref is
/// a no-op and does NOT upgrade it, so installed or update-available
/// packages go through `flatpak update` instead — which is a harmless
/// "Nothing to do" when the app is already up to date. Available packages
/// install from their remote, falling back to flathub.
fn install_args(pkg: &Package) -> Vec<String> {
    let app_id = pkg.flatpak_ref.as_deref().unwrap_or(&pkg.id);
    match pkg.status {
        PackageStatus::Installed | PackageStatus::UpdateAvailable => {
            vec![
                "update".to_string(),
                "-y".to_string(),
                "--".to_string(),
                app_id.to_string(),
            ]
        }
        PackageStatus::Available => {
            let remote = pkg.flatpak_remote.as_deref().unwrap_or("flathub");
            vec![
                "install".to_string(),
                "-y".to_string(),
                "--".to_string(),
                remote.to_string(),
                app_id.to_string(),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{PackageStatus, RepoKind, SourceType};

    // Real flatpak 1.16 output (LC_ALL=C): TAB-separated, no header, remotes
    // column may contain a comma-separated list.
    const SEARCH_OUT: &str =
        "Firefox\tFast, Private & Safe Web Browser\torg.mozilla.firefox\t140.0\tstable\tflathub\n";

    #[test]
    fn parses_flatpak_search() {
        let pkgs = parse_search(SEARCH_OUT);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "Firefox");
        assert_eq!(pkgs[0].id, "org.mozilla.firefox");
        assert_eq!(pkgs[0].flatpak_ref.as_deref(), Some("org.mozilla.firefox"));
        assert_eq!(pkgs[0].flatpak_remote.as_deref(), Some("flathub"));
        assert_eq!(pkgs[0].source, SourceType::Flatpak);
        assert_eq!(pkgs[0].summary, "Fast, Private & Safe Web Browser");
        assert_eq!(pkgs[0].version, "140.0");
        assert_eq!(pkgs[0].status, PackageStatus::Available);
    }

    #[test]
    fn search_prefers_flathub_among_multiple_remotes() {
        let out = "Firefox\tWeb Browser\torg.mozilla.firefox\t152.0.6\tstable\tfedora,flathub\n";
        let pkgs = parse_search(out);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].id, "org.mozilla.firefox");
        assert_eq!(pkgs[0].flatpak_remote.as_deref(), Some("flathub"));
    }

    #[test]
    fn search_uses_first_remote_when_flathub_is_absent() {
        let out = "SomeApp\tAn app\torg.example.App\t1.0\tstable\tfedora\n";
        let pkgs = parse_search(out);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].flatpak_remote.as_deref(), Some("fedora"));
    }

    #[test]
    fn search_skips_garbage() {
        let out = "\nno-tabs-here\n\tmissing-id\t\t1.0\tstable\tflathub\n";
        assert!(parse_search(out).is_empty());
    }

    const LIST_OUT: &str = "Whatsie\tFeature-rich WhatsApp Web client for the Linux desktop\tcom.ktechpit.whatsie\t5.1.0\n";

    #[test]
    fn parses_flatpak_list() {
        let pkgs = parse_list(LIST_OUT);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "Whatsie");
        assert_eq!(pkgs[0].id, "com.ktechpit.whatsie");
        assert_eq!(pkgs[0].flatpak_ref.as_deref(), Some("com.ktechpit.whatsie"));
        // List output has no remote column.
        assert_eq!(pkgs[0].flatpak_remote, None);
        assert_eq!(pkgs[0].version, "5.1.0");
        assert_eq!(pkgs[0].installed_version.as_deref(), Some("5.1.0"));
        assert_eq!(pkgs[0].status, PackageStatus::Installed);
        assert_eq!(pkgs[0].source, SourceType::Flatpak);
    }

    const UPDATES_OUT: &str = "com.ktechpit.whatsie\t5.2.0\n";

    #[test]
    fn parses_updates() {
        let pkgs = parse_updates(UPDATES_OUT);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].id, "com.ktechpit.whatsie");
        assert_eq!(pkgs[0].version, "5.2.0");
        assert_eq!(pkgs[0].status, PackageStatus::UpdateAvailable);
        assert_eq!(pkgs[0].flatpak_ref.as_deref(), Some("com.ktechpit.whatsie"));
    }

    const INFO_OUT: &str = "\nWhatsie - Feature-rich WhatsApp Web client for the Linux desktop\n\n            ID: com.ktechpit.whatsie\n           Ref: app/com.ktechpit.whatsie/x86_64/stable\n          Arch: x86_64\n        Branch: stable\n       Version: 5.1.0\n       License: MIT\n        Origin: flathub\n";

    #[test]
    fn parses_info_output() {
        let pkg = parse_info(INFO_OUT).expect("should parse info");
        assert_eq!(pkg.id, "com.ktechpit.whatsie");
        assert_eq!(pkg.name, "Whatsie");
        assert_eq!(
            pkg.summary,
            "Feature-rich WhatsApp Web client for the Linux desktop"
        );
        assert_eq!(pkg.version, "5.1.0");
        // `flatpak info` covers installed refs, so the version is installed.
        assert_eq!(pkg.installed_version.as_deref(), Some("5.1.0"));
        assert_eq!(pkg.license.as_deref(), Some("MIT"));
        assert_eq!(pkg.flatpak_ref.as_deref(), Some("com.ktechpit.whatsie"));
        assert_eq!(pkg.status, PackageStatus::Installed);
        assert_eq!(pkg.source, SourceType::Flatpak);
    }

    #[test]
    fn info_returns_none_without_id() {
        assert!(parse_info("").is_none());
        assert!(parse_info("error: No such ref\n").is_none());
    }

    #[test]
    fn parsers_never_panic_on_empty_or_malformed() {
        assert!(parse_search("").is_empty());
        assert!(parse_list("").is_empty());
        assert!(parse_updates("").is_empty());
        assert!(parse_search("\t\t\t\t\t\n").is_empty());
        assert!(parse_list("only-one-column\n").is_empty());
        assert!(parse_updates("\t\n").is_empty());
    }

    fn pkg_with(status: PackageStatus, remote: Option<&str>) -> Package {
        let mut pkg = Package::new("org.example.App", "Example", SourceType::Flatpak);
        pkg.status = status;
        pkg.flatpak_ref = Some("org.example.App".to_string());
        pkg.flatpak_remote = remote.map(str::to_string);
        pkg
    }

    #[test]
    fn install_args_installs_available_package_from_its_remote() {
        let pkg = pkg_with(PackageStatus::Available, Some("fedora"));
        assert_eq!(
            install_args(&pkg),
            vec!["install", "-y", "--", "fedora", "org.example.App"]
        );
    }

    #[test]
    fn install_args_falls_back_to_flathub_without_remote() {
        let pkg = pkg_with(PackageStatus::Available, None);
        assert_eq!(
            install_args(&pkg),
            vec!["install", "-y", "--", "flathub", "org.example.App"]
        );
    }

    #[test]
    fn install_args_updates_installed_package() {
        let pkg = pkg_with(PackageStatus::Installed, None);
        assert_eq!(
            install_args(&pkg),
            vec!["update", "-y", "--", "org.example.App"]
        );
    }

    #[test]
    fn install_args_updates_update_available_package() {
        let pkg = pkg_with(PackageStatus::UpdateAvailable, Some("flathub"));
        assert_eq!(
            install_args(&pkg),
            vec!["update", "-y", "--", "org.example.App"]
        );
    }

    // Captured 2026-07-27 from `flatpak remotes --show-disabled
    // --columns=name,title,url,options` (flatpak 1.18.0); the last line was a
    // temporary --user remote, disabled, then deleted. Disabled remotes are
    // hidden unless `--show-disabled` is passed.
    const REMOTES_OUT: &str = "fedora\tFedora Flatpaks\toci+https://registry.fedoraproject.org\tsystem,oci\nfedora-testing\tFedora Flatpaks (testing)\toci+https://registry.fedoraproject.org#testing\tsystem,disabled,oci\nflathub\tFlathub\thttps://dl.flathub.org/repo/\tsystem\nbrim-fixture\t-\thttps://example.com/repo\tuser,disabled\n";

    #[test]
    fn parse_remotes_reads_columns_and_disabled_flag() {
        let repos = parse_remotes(REMOTES_OUT);
        assert!(repos.len() >= 3);
        let flathub = repos.iter().find(|r| r.id == "flathub").unwrap();
        assert_eq!(flathub.kind, RepoKind::FlatpakRemote);
        assert!(flathub.enabled);
        assert_eq!(flathub.title, "Flathub");
        assert_eq!(flathub.url, "https://dl.flathub.org/repo/");
        let fixture = repos.iter().find(|r| r.id == "brim-fixture").unwrap();
        assert!(!fixture.enabled);
        // Real flatpak prints "-" for untitled remotes (e.g. ones added via
        // `flatpak remote-add` without a title); fall back to the name.
        assert_eq!(fixture.title, "brim-fixture");
    }

    #[test]
    fn parse_remotes_is_total_on_garbage() {
        assert!(parse_remotes("").is_empty());
        assert!(parse_remotes("no-tabs-here").is_empty());
    }
}

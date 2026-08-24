//! DNF5 backend: wraps the `dnf5` CLI on Fedora systems.
//!
//! All output parsers in this module are pure functions so they can be unit
//! tested without ever executing `dnf5`. Every process spawn forces
//! `LC_ALL=C` so parsers can rely on English headers and field names.

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

use crate::backend::Backend;
use crate::backends::{probe, validate_arg, QUERY_TIMEOUT};
use crate::error::{BrimError, Result};
use crate::models::{Package, PackageStatus, SourceType, TransactionAction, TransactionResult};

/// `dnf5 check-update` exits with this code when updates are available;
/// it signals success, not failure.
const CHECK_UPDATE_EXIT_UPDATES_AVAILABLE: i32 = 100;

/// Backend for packages managed by DNF5 (Fedora's default package manager).
#[derive(Debug, Default, Clone)]
pub struct Dnf5Backend {
    /// Cached result of the first availability probe.
    available: OnceLock<bool>,
}

impl Dnf5Backend {
    /// Create a new DNF5 backend instance (with a fresh availability cache).
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `dnf5` with the given arguments and return stdout on success.
    ///
    /// `allow_update_exit_code` treats exit code 100 (check-update found
    /// updates) as success. Any other non-zero exit yields
    /// [`BrimError::CommandFailed`] carrying stderr. `timeout` bounds the
    /// wait (queries); pass `None` for transactions, which may take long.
    async fn run(
        &self,
        args: &[&str],
        allow_update_exit_code: bool,
        timeout: Option<Duration>,
    ) -> Result<String> {
        let future = Command::new("dnf5")
            .args(args)
            // Force English output so the parsers see stable headers/fields.
            .env("LC_ALL", "C")
            // A timed-out query must not leave the dnf5 child running.
            .kill_on_drop(true)
            .output();
        let output = match timeout {
            Some(limit) => match tokio::time::timeout(limit, future).await {
                Ok(result) => result.map_err(|e| super::spawn_error("dnf5", e))?,
                Err(_) => return Err(BrimError::CommandFailed("dnf5 timed out".to_string())),
            },
            None => future.await.map_err(|e| super::spawn_error("dnf5", e))?,
        };

        let code = output.status.code().unwrap_or(-1);
        let ok = output.status.success()
            || (allow_update_exit_code && code == CHECK_UPDATE_EXIT_UPDATES_AVAILABLE);
        if ok {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                format!("dnf5 exited with code {code}")
            } else {
                stderr
            };
            Err(BrimError::CommandFailed(detail))
        }
    }
}

#[async_trait]
impl Backend for Dnf5Backend {
    fn source(&self) -> SourceType {
        SourceType::FedoraOfficial
    }

    async fn is_available(&self) -> bool {
        if let Some(&cached) = self.available.get() {
            return cached;
        }
        let available = probe("dnf5", &["--version"]).await;
        // A concurrent probe setting the cache first is fine; the result
        // only changes when the system changes.
        let _ = self.available.set(available);
        available
    }

    async fn search(&self, query: &str) -> Result<Vec<Package>> {
        validate_arg(query)?;
        let out = self
            .run(&["search", "-q", query], false, Some(QUERY_TIMEOUT))
            .await?;
        Ok(parse_search(&out))
    }

    async fn list_installed(&self) -> Result<Vec<Package>> {
        let out = self
            .run(&["list", "--installed", "-q"], false, Some(QUERY_TIMEOUT))
            .await?;
        Ok(parse_list_installed(&out))
    }

    async fn info(&self, id: &str) -> Result<Package> {
        validate_arg(id)?;
        let out = self.run(&["info", id], false, Some(QUERY_TIMEOUT)).await?;
        parse_info(&out).ok_or_else(|| BrimError::NotFound(id.to_string()))
    }

    async fn install(&self, pkg: &Package) -> Result<TransactionResult> {
        validate_arg(&pkg.id)?;
        // Real transaction: no timeout (dnf5 upgrades installed packages).
        let out = self.run(&["install", "-y", &pkg.id], false, None).await?;
        Ok(TransactionResult::ok(
            TransactionAction::Install,
            &pkg.id,
            format!("installed {}", pkg.id),
            out,
        ))
    }

    async fn remove(&self, pkg: &Package) -> Result<TransactionResult> {
        validate_arg(&pkg.id)?;
        let out = self.run(&["remove", "-y", &pkg.id], false, None).await?;
        Ok(TransactionResult::ok(
            TransactionAction::Remove,
            &pkg.id,
            format!("removed {}", pkg.id),
            out,
        ))
    }

    async fn updates(&self) -> Result<Vec<Package>> {
        let out = self
            .run(&["check-update", "-q"], true, Some(QUERY_TIMEOUT))
            .await?;
        Ok(parse_check_update(&out))
    }

    async fn upgrade(&self) -> Result<TransactionResult> {
        let out = self.run(&["upgrade", "-y"], false, None).await?;
        Ok(TransactionResult::ok(
            TransactionAction::Upgrade,
            "system",
            "upgraded all packages",
            out,
        ))
    }
}

/// Split `name.arch` into `(name, arch)`; returns `None` when there is no
/// usable arch suffix (used to filter out headers and garbage lines).
fn split_name_arch(nv: &str) -> Option<(&str, &str)> {
    let (name, arch) = nv.rsplit_once('.')?;
    if name.is_empty() || arch.is_empty() {
        return None;
    }
    Some((name, arch))
}

/// Parse `dnf5 search -q` output.
///
/// Real dnf5 emits section headers (`Matched fields: name (exact)`) and
/// package lines shaped ` name.arch<TAB>summary` (older versions used
/// ` name.arch : summary`). A package matching several sections is listed
/// once per section, so results are deduplicated by `name.arch` (first
/// occurrence wins). Lines without a separator or arch suffix are skipped.
pub fn parse_search(output: &str) -> Vec<Package> {
    let mut seen = std::collections::HashSet::new();
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (left, summary) = line.split_once('\t').or_else(|| line.split_once(" : "))?;
            let nv = left.split_whitespace().next()?;
            let (name, _) = split_name_arch(nv)?;
            if !seen.insert(nv.to_string()) {
                return None;
            }
            let mut pkg = Package::new(nv, name, SourceType::FedoraOfficial);
            pkg.summary = summary.trim().to_string();
            pkg.refresh_category();
            Some(pkg)
        })
        .collect()
}

/// Parse `dnf5 list --installed -q` output.
///
/// Package lines are column-aligned: `name.arch  version-release  repo`.
/// The `Installed packages` header and malformed lines are skipped.
pub fn parse_list_installed(output: &str) -> Vec<Package> {
    parse_list_lines(output, PackageStatus::Installed)
}

/// Parse `dnf5 check-update -q` output (same column shape as `list`).
pub fn parse_check_update(output: &str) -> Vec<Package> {
    parse_list_lines(output, PackageStatus::UpdateAvailable)
}

/// Shared parser for column-shaped `list`/`check-update` output.
fn parse_list_lines(output: &str, status: PackageStatus) -> Vec<Package> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let nv = fields.next()?;
            let version = fields.next()?;
            let (name, _) = split_name_arch(nv)?;
            let mut pkg = Package::new(nv, name, SourceType::FedoraOfficial);
            pkg.version = version.to_string();
            if status == PackageStatus::Installed {
                // For installed entries the listed version is the installed
                // one; for updates it is the *new* version and the installed
                // version is unknown here.
                pkg.installed_version = Some(version.to_string());
            }
            pkg.status = status;
            Some(pkg)
        })
        .collect()
}

/// Parse `dnf5 info <id>` output into a single package.
///
/// When a package is both installed and available in a repo, dnf5 prints
/// two sections (`Installed packages` then `Available packages`) whose
/// fields must not mix: the installed section wins for an installed
/// package. Returns `None` when no `Name` field is present (unknown
/// package or empty output). Field lines look like `Name            : htop`;
/// description continuations repeat with an empty key.
pub fn parse_info(output: &str) -> Option<Package> {
    let mut blocks: Vec<InfoBlock> = Vec::new();
    let mut current = InfoBlock::default();
    let mut in_description = false;
    let mut started = false;

    for line in output.lines() {
        let trimmed = line.trim();
        let header = match trimmed {
            "Installed packages" => Some(PackageStatus::Installed),
            "Available packages" => Some(PackageStatus::Available),
            _ => None,
        };
        if let Some(status) = header {
            if started {
                blocks.push(std::mem::take(&mut current));
            }
            current.status = status;
            started = true;
            in_description = false;
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            // Continuation of the previous multi-line field.
            if in_description && !value.is_empty() {
                if !current.description.is_empty() {
                    current.description.push(' ');
                }
                current.description.push_str(value);
            }
            continue;
        }
        in_description = false;
        match key {
            "Name" => {
                current.name = Some(value.to_string());
                started = true;
            }
            "Architecture" => current.arch = value.to_string(),
            "Version" => current.version = value.to_string(),
            "Release" => current.release = value.to_string(),
            "Summary" => current.summary = value.to_string(),
            "Description" => {
                current.description = value.to_string();
                in_description = true;
            }
            "License" => current.license = Some(value.to_string()),
            "URL" => current.homepage = Some(value.to_string()),
            _ => {}
        }
    }
    if started {
        blocks.push(current);
    }

    // The installed section describes what is actually on the system; only
    // fall back to an available section when the package is not installed.
    let block = blocks
        .iter()
        .find(|b| b.status == PackageStatus::Installed && b.name.is_some())
        .or_else(|| blocks.iter().find(|b| b.name.is_some()))?;
    Some(block.to_package())
}

/// One `dnf5 info` section (`Installed packages` / `Available packages`).
#[derive(Default)]
struct InfoBlock {
    status: PackageStatus,

    name: Option<String>,
    arch: String,
    version: String,
    release: String,
    summary: String,
    description: String,
    license: Option<String>,
    homepage: Option<String>,
}

impl InfoBlock {
    /// Turn a parsed section into a package; requires a `Name` field.
    fn to_package(&self) -> Package {
        let name = self.name.clone().unwrap_or_default();
        let id = if self.arch.is_empty() {
            name.clone()
        } else {
            format!("{name}.{}", self.arch)
        };
        let full_version = if self.release.is_empty() {
            self.version.clone()
        } else {
            format!("{}-{}", self.version, self.release)
        };
        let mut pkg = Package::new(id, &name, SourceType::FedoraOfficial);
        // For installed packages the reported version IS the installed one —
        // keep both fields in sync, like the flatpak backend does.
        if self.status == PackageStatus::Installed {
            pkg.installed_version = Some(full_version.clone());
        }
        pkg.version = full_version;
        pkg.summary = self.summary.clone();
        pkg.refresh_category();
        pkg.description = self.description.clone();
        pkg.license = self.license.clone();
        pkg.homepage = self.homepage.clone();
        pkg.status = self.status;
        pkg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Category, PackageStatus, SourceType};

    // Real dnf5 (5.4.x, LC_ALL=C) separates name.arch and summary with a TAB
    // and prints localized-unaware section headers.
    const SEARCH_OUT: &str = "Matched fields: name (exact)\n htop.x86_64\tInteractive process viewer\n vim-enhanced.x86_64\tVi IMproved, version 8\n\nMatched fields: summary\n screen.x86_64\tA screen manager that supports multiple logins\n";

    #[test]
    fn parses_search_lines() {
        let pkgs = parse_search(SEARCH_OUT);
        assert_eq!(pkgs.len(), 3);
        assert_eq!(pkgs[0].id, "htop.x86_64");
        assert_eq!(pkgs[0].name, "htop");
        assert_eq!(pkgs[0].summary, "Interactive process viewer");
        assert_eq!(pkgs[0].source, SourceType::FedoraOfficial);
        assert_eq!(pkgs[0].status, PackageStatus::Available);
    }

    #[test]
    fn parses_search_colon_separator() {
        // Older/alternate dnf5 output uses " : " as separator.
        let out = "Matched fields: name\n htop.x86_64 : Interactive process viewer\n";
        let pkgs = parse_search(out);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "htop");
        assert_eq!(pkgs[0].summary, "Interactive process viewer");
    }

    #[test]
    fn search_deduplicates_packages_across_match_sections() {
        // Real dnf5 lists a package once per `Matched fields` section it
        // matches; the same package must appear only once in the results.
        let out = "Matched fields: name (exact)\n htop.x86_64\tInteractive process viewer\n\nMatched fields: summary\n htop.x86_64\tInteractive process viewer\n";
        let pkgs = parse_search(out);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].id, "htop.x86_64");
    }

    #[test]
    fn search_skips_headers_blank_and_garbage() {
        let out = "Matched fields: name\n\n???no separator here\n htop.x86_64\tInteractive process viewer\n";
        let pkgs = parse_search(out);
        assert_eq!(pkgs.len(), 1);
    }

    #[test]
    fn search_refreshes_category_from_summary() {
        let out = " godot.x86_64\tGame engine for 2D and 3D games\n";
        let pkgs = parse_search(out);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].category, Category::Gaming);
    }

    const LIST_OUT: &str = "Installed packages\n7zip.x86_64                                    26.02-1.fc44                        updates\nImageMagick-libs.x86_64                        1:7.1.2.27-1.fc44                   updates\n";

    #[test]
    fn parses_list_installed() {
        let pkgs = parse_list_installed(LIST_OUT);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].id, "7zip.x86_64");
        assert_eq!(pkgs[0].name, "7zip");
        assert_eq!(pkgs[0].version, "26.02-1.fc44");
        assert_eq!(pkgs[0].installed_version.as_deref(), Some("26.02-1.fc44"));
        assert_eq!(pkgs[0].status, PackageStatus::Installed);
        assert_eq!(pkgs[1].name, "ImageMagick-libs");
        assert_eq!(pkgs[1].version, "1:7.1.2.27-1.fc44");
        assert_eq!(
            pkgs[1].installed_version.as_deref(),
            Some("1:7.1.2.27-1.fc44")
        );
    }

    #[test]
    fn list_skips_headers_and_garbage() {
        let out = "Installed packages\nnoarch-line-without-arch\nsingle-token\nhtop.x86_64  3.4.1-3.fc44  fedora\n";
        let pkgs = parse_list_installed(out);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "htop");
    }

    const UPDATE_OUT: &str = "htop.x86_64  3.3.0-2.fc44  updates\n";

    #[test]
    fn parses_check_update() {
        let pkgs = parse_check_update(UPDATE_OUT);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].status, PackageStatus::UpdateAvailable);
        assert_eq!(pkgs[0].name, "htop");
        // The listed version is the new one; the installed version is
        // unknown from check-update output.
        assert_eq!(pkgs[0].version, "3.3.0-2.fc44");
        assert_eq!(pkgs[0].installed_version, None);
        assert_eq!(pkgs[0].source, SourceType::FedoraOfficial);
    }

    #[test]
    fn parsers_never_panic_on_empty_or_malformed() {
        assert!(parse_search("").is_empty());
        assert!(parse_list_installed("").is_empty());
        assert!(parse_check_update("").is_empty());
        assert!(parse_check_update("\n\nObsoleting Packages\n").is_empty());
        assert!(parse_search(".").is_empty());
    }

    const INFO_OUT: &str = "Updating and loading repositories:\nRepositories loaded.\nInstalled packages\nName            : htop\nEpoch           : 0\nVersion         : 3.4.1\nRelease         : 3.fc44\nArchitecture    : x86_64\nInstalled size  : 464.3 KiB\nSource          : htop-3.4.1-3.fc44.src.rpm\nFrom repository : fedora\nSummary         : Interactive process viewer\nURL             : https://htop.dev/\nLicense         : GPL-2.0-or-later\nDescription     : htop is an interactive text-mode process viewer for Linux, similar to\n                : top(1).\nVendor          : Fedora Project\n";

    #[test]
    fn parses_info_output() {
        let pkg = parse_info(INFO_OUT).expect("should parse info");
        assert_eq!(pkg.name, "htop");
        assert_eq!(pkg.id, "htop.x86_64");
        assert_eq!(pkg.version, "3.4.1-3.fc44");
        assert_eq!(pkg.summary, "Interactive process viewer");
        assert!(pkg.description.contains("text-mode process viewer"));
        assert!(pkg.description.contains("top(1)."));
        assert_eq!(pkg.license.as_deref(), Some("GPL-2.0-or-later"));
        assert_eq!(pkg.homepage.as_deref(), Some("https://htop.dev/"));
        assert_eq!(pkg.status, PackageStatus::Installed);
        assert_eq!(pkg.installed_version.as_deref(), Some("3.4.1-3.fc44"));
        assert_eq!(pkg.source, SourceType::FedoraOfficial);
    }

    #[test]
    fn parses_info_available_package() {
        let out = "Available packages\nName         : nonexist\nVersion      : 1.0\nRelease      : 1.fc44\nArchitecture : src\nSummary      : Something\n";
        let pkg = parse_info(out).expect("should parse info");
        assert_eq!(pkg.name, "nonexist");
        assert_eq!(pkg.status, PackageStatus::Available);
        assert_eq!(pkg.installed_version, None);
    }

    // Real `dnf5 info htop` when htop is installed AND an update is
    // available: two sections, installed first. The installed section must
    // win and no field may leak across sections.
    const INFO_DOUBLE_BLOCK_OUT: &str = "Updating and loading repositories:\nRepositories loaded.\nInstalled packages\nName            : htop\nEpoch           : 0\nVersion         : 3.4.1\nRelease         : 3.fc44\nArchitecture    : x86_64\nInstalled size  : 464.3 KiB\nFrom repository : updates\nSummary         : Interactive process viewer\nURL             : https://htop.dev/\nLicense         : GPL-2.0-or-later\nDescription     : htop is an interactive text-mode process viewer for Linux, similar to\n                : top(1).\nVendor          : Fedora Project\n\nAvailable packages\nName            : htop\nEpoch           : 0\nVersion         : 3.5.0\nRelease         : 1.fc44\nArchitecture    : x86_64\nDownload size   : 200.0 KiB\nRepository      : updates\nSummary         : Interactive process viewer (bleeding edge)\nURL             : https://example.com/NEW\nLicense         : MIT\nDescription     : rewritten available-block description\n";

    #[test]
    fn info_prefers_installed_block_over_available() {
        let pkg = parse_info(INFO_DOUBLE_BLOCK_OUT).expect("should parse info");
        assert_eq!(pkg.name, "htop");
        assert_eq!(pkg.status, PackageStatus::Installed);
        // Version fields come from the installed block, not the newer
        // available block.
        assert_eq!(pkg.version, "3.4.1-3.fc44");
        assert_eq!(pkg.installed_version.as_deref(), Some("3.4.1-3.fc44"));
        // No field mixing across blocks.
        assert_eq!(pkg.summary, "Interactive process viewer");
        assert!(pkg.description.contains("top(1)."));
        assert!(!pkg.description.contains("rewritten"));
        assert_eq!(pkg.homepage.as_deref(), Some("https://htop.dev/"));
        assert_eq!(pkg.license.as_deref(), Some("GPL-2.0-or-later"));
    }

    #[test]
    fn info_returns_none_without_name() {
        assert!(parse_info("Repositories loaded.\n").is_none());
        assert!(parse_info("").is_none());
    }
}

//! APT backend: wraps `apt-get`/`apt-cache`/`dpkg-query` on Debian-based
//! systems (Debian, Ubuntu and derivatives).
//!
//! All output parsers in this module are pure functions so they can be unit
//! tested without ever executing apt. Every process spawn forces `LC_ALL=C`
//! so parsers can rely on English headers and field names.

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

use crate::core::backend::Backend;
use crate::core::backends::{probe, validate_arg, QUERY_TIMEOUT};
use crate::core::error::{BrimError, Result};
use crate::core::models::{
    Package, PackageStatus, SourceType, TransactionAction, TransactionResult,
};

/// Backend for packages managed by APT (Debian's package manager).
#[derive(Debug, Default, Clone)]
pub struct AptBackend {
    /// Cached result of the first availability probe.
    available: OnceLock<bool>,
}

impl AptBackend {
    /// Create a new APT backend instance (with a fresh availability cache).
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `program` (an apt-family tool) with the given arguments and
    /// return stdout on success.
    ///
    /// Mirrors the dnf5 backend's runner: `LC_ALL=C` for stable English
    /// output, `kill_on_drop` so timed-out queries leave no orphans, and
    /// non-zero exits surface stderr as [`BrimError::CommandFailed`].
    /// `timeout` bounds the wait (queries); pass `None` for transactions.
    async fn run(&self, program: &str, args: &[&str], timeout: Option<Duration>) -> Result<String> {
        let future = Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
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
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let code = output.status.code().unwrap_or(-1);
            let detail = if stderr.is_empty() {
                format!("{program} exited with code {code}")
            } else {
                stderr
            };
            Err(BrimError::CommandFailed(detail))
        }
    }
}

#[async_trait]
impl Backend for AptBackend {
    fn source(&self) -> SourceType {
        SourceType::Debian
    }

    async fn is_available(&self) -> bool {
        if let Some(&cached) = self.available.get() {
            return cached;
        }
        let available = probe("apt-get", &["--version"]).await;
        // A concurrent probe setting the cache first is fine; the result
        // only changes when the system changes.
        let _ = self.available.set(available);
        available
    }

    async fn search(&self, query: &str) -> Result<Vec<Package>> {
        validate_arg(query)?;
        // `apt-cache search` reads the local index only (no network), so
        // it is fast even without a cache-mode fallback like dnf5's.
        let out = self
            .run("apt-cache", &["search", query], Some(QUERY_TIMEOUT))
            .await?;
        Ok(parse_search(&out))
    }

    async fn list_installed(&self) -> Result<Vec<Package>> {
        // dpkg-query is faster and more stable to parse than `apt list
        // --installed` (which warns about its unstable CLI on stderr).
        let out = self
            .run(
                "dpkg-query",
                &["-W", "-f=${Package}\t${Version}\n"],
                Some(QUERY_TIMEOUT),
            )
            .await?;
        Ok(parse_installed(&out))
    }

    async fn info(&self, id: &str) -> Result<Package> {
        validate_arg(id)?;
        let show = self
            .run("apt-cache", &["show", id], Some(QUERY_TIMEOUT))
            .await?;
        let mut pkg = parse_show(&show).ok_or_else(|| BrimError::NotFound(id.to_string()))?;
        // `apt-cache show` says nothing about the install state; policy
        // knows the installed and candidate versions.
        if let Ok(policy) = self
            .run("apt-cache", &["policy", id], Some(QUERY_TIMEOUT))
            .await
        {
            let (installed, _candidate) = parse_policy(&policy);
            pkg.status = match &installed {
                Some(version) if version != &pkg.version => PackageStatus::UpdateAvailable,
                Some(_) => PackageStatus::Installed,
                None => PackageStatus::Available,
            };
            pkg.installed_version = installed;
        }
        Ok(pkg)
    }

    async fn install(&self, pkg: &Package) -> Result<TransactionResult> {
        validate_arg(&pkg.id)?;
        super::require_root("apt-get")?;
        // Real transaction: no timeout (apt-get upgrades installed packages).
        let out = self
            .run("apt-get", &["install", "-y", &pkg.id], None)
            .await?;
        Ok(TransactionResult::ok(
            TransactionAction::Install,
            &pkg.id,
            format!("installed {}", pkg.id),
            out,
        ))
    }

    async fn remove(&self, pkg: &Package) -> Result<TransactionResult> {
        validate_arg(&pkg.id)?;
        super::require_root("apt-get")?;
        let out = self
            .run("apt-get", &["remove", "-y", &pkg.id], None)
            .await?;
        Ok(TransactionResult::ok(
            TransactionAction::Remove,
            &pkg.id,
            format!("removed {}", pkg.id),
            out,
        ))
    }

    async fn updates(&self) -> Result<Vec<Package>> {
        // `apt list --upgradable` warns about its unstable CLI on stderr
        // but exits 0; only stdout is parsed.
        let out = self
            .run("apt", &["list", "--upgradable"], Some(QUERY_TIMEOUT))
            .await?;
        Ok(parse_upgradable(&out))
    }

    async fn upgrade(&self) -> Result<TransactionResult> {
        super::require_root("apt-get")?;
        // Unlike dnf5, apt-get does not refresh its index as part of an
        // upgrade: run `apt-get update` first or upgrades silently apply
        // stale metadata.
        let mut out = self.run("apt-get", &["update"], None).await?;
        out.push_str(&self.run("apt-get", &["upgrade", "-y"], None).await?);
        Ok(TransactionResult::ok(
            TransactionAction::Upgrade,
            "system",
            "upgraded all packages",
            out,
        ))
    }
}

/// Parse `apt-cache search <query>` output: one `name - summary` per line.
pub fn parse_search(output: &str) -> Vec<Package> {
    output
        .lines()
        .filter_map(|line| {
            let (name, summary) = line.split_once(" - ")?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            let mut pkg = Package::new(name, name, SourceType::Debian);
            pkg.summary = summary.trim().to_string();
            pkg.refresh_category();
            Some(pkg)
        })
        .collect()
}

/// Parse `dpkg-query -W -f='${Package}\t${Version}\n'` output.
pub fn parse_installed(output: &str) -> Vec<Package> {
    output
        .lines()
        .filter_map(|line| {
            let (name, version) = line.split_once('\t')?;
            let name = name.trim();
            let version = version.trim();
            if name.is_empty() || version.is_empty() {
                return None;
            }
            let mut pkg = Package::new(name, name, SourceType::Debian);
            pkg.version = version.to_string();
            pkg.installed_version = Some(version.to_string());
            pkg.status = PackageStatus::Installed;
            Some(pkg)
        })
        .collect()
}

/// Parse the first paragraph of `apt-cache show <id>` output (the
/// candidate's record). Returns `None` when no `Package` field is present
/// (unknown package or empty output).
pub fn parse_show(output: &str) -> Option<Package> {
    let paragraph = output.split("\n\n").next()?;
    let mut name: Option<&str> = None;
    let mut version = "";
    let mut size_kb: Option<f64> = None;
    let mut homepage: Option<String> = None;
    let mut summary = String::new();
    let mut description = String::new();
    let mut in_description = false;

    for line in paragraph.lines() {
        // Description continuation lines start with a space.
        if in_description && line.starts_with(' ') {
            let text = line.trim();
            // A lone "." is Debian's encoded empty line.
            if text != "." {
                if !description.is_empty() {
                    description.push('\n');
                }
                description.push_str(text);
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        in_description = false;
        match key {
            "Package" => name = Some(value),
            "Version" => version = value,
            "Installed-Size" => size_kb = value.parse::<f64>().ok(),
            "Homepage" => homepage = Some(value.to_string()),
            "Description" => {
                summary = value.to_string();
                in_description = true;
            }
            _ => {}
        }
    }

    let name = name?;
    let mut pkg = Package::new(name, name, SourceType::Debian);
    pkg.version = version.to_string();
    pkg.summary = summary;
    pkg.description = description;
    pkg.homepage = homepage;
    // dpkg's Installed-Size is KiB.
    if let Some(kb) = size_kb {
        pkg.size_mb = kb / 1024.0;
    }
    pkg.refresh_category();
    Some(pkg)
}

/// Parse `apt-cache policy <id>` output: the `Installed:` and `Candidate:`
/// versions. `(none)` means not installed.
pub fn parse_policy(output: &str) -> (Option<String>, Option<String>) {
    let field = |key: &str| {
        output
            .lines()
            .find_map(|line| line.trim().strip_prefix(key))
            .map(str::trim)
            .filter(|v| *v != "(none)")
            .map(str::to_string)
    };
    (field("Installed:"), field("Candidate:"))
}

/// Parse `apt list --upgradable` output:
/// `name/suite new-version arch [upgradable from: old-version]`, preceded
/// by a `Listing...` header line.
pub fn parse_upgradable(output: &str) -> Vec<Package> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next()?.split('/').next()?;
            if name.is_empty() || name == "Listing..." {
                return None;
            }
            let version = fields.next()?;
            let old = line
                .split_once("[upgradable from: ")
                .and_then(|(_, rest)| rest.strip_suffix(']'));
            let mut pkg = Package::new(name, name, SourceType::Debian);
            pkg.version = version.to_string();
            pkg.installed_version = old.map(str::to_string);
            pkg.status = PackageStatus::UpdateAvailable;
            Some(pkg)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from `apt-cache search htop` (Debian 12).
    const SEARCH_OUT: &str = "htop - interactive processes viewer\npcp-htop - Performance Co-Pilot interactive process viewer\n";

    // Captured from `dpkg-query -W -f='${Package}\t${Version}\n'` (Debian 12).
    const INSTALLED_OUT: &str = "adduser\t3.134\napt\t2.6.1\nhtop\t3.2.2-2\n";

    // Captured from `apt-cache show htop` (Debian 12), trimmed to the
    // fields Brim reads.
    const SHOW_OUT: &str = "Package: htop\n\
         Version: 3.2.2-2\n\
         Installed-Size: 372\n\
         Architecture: amd64\n\
         Description: interactive processes viewer\n\
         \x20Htop is an ncurses-based process viewer similar to top.\n\
         \x20.\n\
         \x20It can show the full command line of processes.\n\
         Homepage: https://htop.dev/\n\
         Section: utils\n";

    // Captured from `apt list --upgradable` (Debian 12).
    const UPGRADABLE_OUT: &str = "Listing... Done\nhtop/stable 3.2.2-2 amd64 [upgradable from: 3.2.1-1]\nvim/stable 2:9.0.1378-2 amd64 [upgradable from: 2:9.0.1378-1]\n";

    // Captured from `apt-cache policy htop` (Debian 12), header only.
    const POLICY_OUT: &str =
        "htop:\n  Installed: 3.2.1-1\n  Candidate: 3.2.2-2\n  Version table:\n     3.2.2-2 500\n";

    const POLICY_NOT_INSTALLED: &str = "htop:\n  Installed: (none)\n  Candidate: 3.2.2-2\n";

    #[test]
    fn parse_search_reads_name_and_summary() {
        let pkgs = parse_search(SEARCH_OUT);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "htop");
        assert_eq!(pkgs[0].summary, "interactive processes viewer");
        assert_eq!(pkgs[0].source, SourceType::Debian);
        assert!(parse_search("").is_empty());
    }

    #[test]
    fn parse_installed_marks_installed_versions() {
        let pkgs = parse_installed(INSTALLED_OUT);
        assert_eq!(pkgs.len(), 3);
        assert_eq!(pkgs[2].id, "htop");
        assert_eq!(pkgs[2].version, "3.2.2-2");
        assert_eq!(pkgs[2].installed_version.as_deref(), Some("3.2.2-2"));
        assert_eq!(pkgs[2].status, PackageStatus::Installed);
    }

    #[test]
    fn parse_show_reads_first_paragraph_fields() {
        let pkg = parse_show(SHOW_OUT).unwrap();
        assert_eq!(pkg.name, "htop");
        assert_eq!(pkg.version, "3.2.2-2");
        assert_eq!(pkg.summary, "interactive processes viewer");
        assert!(pkg.description.contains("ncurses-based"));
        // The "." empty-line marker is dropped, the following paragraph
        // survives: two lines joined with a newline.
        assert_eq!(
            pkg.description,
            "Htop is an ncurses-based process viewer similar to top.\nIt can show the full command line of processes."
        );
        assert_eq!(pkg.homepage.as_deref(), Some("https://htop.dev/"));
        // 372 KiB ≈ 0.36 MB.
        assert!((pkg.size_mb - 372.0 / 1024.0).abs() < f64::EPSILON);
        assert!(parse_show("").is_none());
    }

    #[test]
    fn parse_policy_handles_installed_and_not_installed() {
        let (installed, candidate) = parse_policy(POLICY_OUT);
        assert_eq!(installed.as_deref(), Some("3.2.1-1"));
        assert_eq!(candidate.as_deref(), Some("3.2.2-2"));
        let (installed, candidate) = parse_policy(POLICY_NOT_INSTALLED);
        assert_eq!(installed, None);
        assert_eq!(candidate.as_deref(), Some("3.2.2-2"));
    }

    #[test]
    fn parse_upgradable_reads_new_and_old_versions() {
        let pkgs = parse_upgradable(UPGRADABLE_OUT);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "htop");
        assert_eq!(pkgs[0].version, "3.2.2-2");
        assert_eq!(pkgs[0].installed_version.as_deref(), Some("3.2.1-1"));
        assert_eq!(pkgs[0].status, PackageStatus::UpdateAvailable);
        // Epoch versions (2:9.0…) parse as a single field.
        assert_eq!(pkgs[1].version, "2:9.0.1378-2");
        assert!(parse_upgradable("Listing... Done\n").is_empty());
    }
}

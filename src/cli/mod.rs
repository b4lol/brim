//! Brim terminal companion — CLI frontend over the brim-core engine.

mod banner;
mod lastsearch;
mod prompt;
mod sanitize;
mod table;

use crate::core::{
    config_path, BrimError, Config as BrimConfig, Package, PackageManager, Result, SourceType,
    TransactionResult,
};
use clap::{Parser, Subcommand};
use colored::Colorize;
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use sanitize::sanitize;
use std::io::IsTerminal;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "brim",
    version,
    about = "Brim — the Fedora app store & package manager"
)]
pub struct Cli {
    /// Print machine-readable JSON instead of tables (search, list,
    /// info, stats). Implies no banner.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Search for packages across all sources.
    Search {
        query: String,
        /// Restrict the search to one source.
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
    },
    /// Install a package by id or by row number (#) from the last search
    /// (asks for confirmation unless --yes).
    Install {
        id: String,
        /// Restrict the install to one source.
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
        /// Skip the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Remove an installed package (asks for confirmation unless --yes).
    Remove {
        id: String,
        /// Restrict the removal to one source.
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
        /// Skip the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Upgrade all packages (asks for confirmation unless --yes).
    Upgrade {
        /// Skip the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// List installed packages.
    List,
    /// List pending updates in detail (installed → new version).
    Updates,
    /// Show system statistics.
    Stats,
    /// Show details for a package.
    Info {
        id: String,
        /// Restrict the lookup to one source.
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
    },
    /// View and edit Brim configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Print a shell completion script to stdout (bash or zsh).
    Completions {
        /// The shell to generate completions for.
        #[arg(value_enum)]
        shell: ShellArg,
    },
    /// Launch the graphical app store.
    Gui,
    /// Run the web UI and REST API (bound to 127.0.0.1).
    Web {
        /// Port to listen on (bound to 127.0.0.1 only).
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// List all configuration keys and values.
    List,
    /// Print the value of one key.
    Get { key: String },
    /// Set a boolean key (true or false) and save.
    Set { key: String, value: String },
    /// Restore all settings to their defaults.
    Reset,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum SourceArg {
    Fedora,
    Copr,
    Flatpak,
    Debian,
}

impl From<SourceArg> for SourceType {
    fn from(arg: SourceArg) -> Self {
        match arg {
            SourceArg::Fedora => SourceType::FedoraOfficial,
            SourceArg::Copr => SourceType::Copr,
            SourceArg::Flatpak => SourceType::Flatpak,
            SourceArg::Debian => SourceType::Debian,
        }
    }
}

/// Shells Brim ships completions for.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum ShellArg {
    Bash,
    Zsh,
}

impl From<ShellArg> for clap_complete::Shell {
    fn from(arg: ShellArg) -> Self {
        match arg {
            ShellArg::Bash => clap_complete::Shell::Bash,
            ShellArg::Zsh => clap_complete::Shell::Zsh,
        }
    }
}

// Exit codes: 0 = success (or user abort), 1 = general/backend error,
// 2 = package not found, 3 = a transaction ran but reported failure.
const EXIT_ERROR: i32 = 1;
const EXIT_NOT_FOUND: i32 = 2;
const EXIT_TX_FAILED: i32 = 3;

/// Run the CLI: print the banner, execute the subcommand, and return the
/// process exit code. Gui/Web are dispatched by `main` and never reach
/// this function.
pub async fn run(cli: &Cli) -> i32 {
    // Machine-readable output stays clean; the banner itself is also
    // suppressed when stdout is not a terminal (see banner.rs).
    if !cli.json {
        banner::print_banner();
    }
    match dispatch(cli).await {
        // `false` = a transaction reported failure.
        Ok(false) => EXIT_TX_FAILED,
        Ok(true) => 0,
        Err(err) => {
            eprintln!("{}", format!("error: {err}").red().bold());
            error_exit_code(&err)
        }
    }
}

/// Map an error to its exit code: "package not found" gets its own code
/// so scripts can tell a miss apart from a broken backend.
fn error_exit_code(err: &BrimError) -> i32 {
    match err {
        BrimError::NotFound(_) => EXIT_NOT_FOUND,
        _ => EXIT_ERROR,
    }
}

/// Returns `Ok(false)` when a transaction itself reported failure, so the
/// single exit-code decision stays in `run`.
async fn dispatch(cli: &Cli) -> Result<bool> {
    // Config subcommands never touch the backends.
    if let Commands::Config { action } = &cli.command {
        return run_config(action);
    }
    // Completion scripts are pure clap metadata: no backends, no banner
    // concerns — the generated script goes to stdout verbatim.
    if let Commands::Completions { shell } = &cli.command {
        let mut command = <Cli as clap::CommandFactory>::command();
        let name = command.get_name().to_string();
        let shell: clap_complete::Shell = (*shell).into();
        clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
        return Ok(true);
    }
    // Async constructor: the sync one would block the tokio executor
    // while reading the config.
    let pm = PackageManager::new_async().await;
    match &cli.command {
        // Handled above, before any backend is constructed.
        Commands::Config { .. } | Commands::Completions { .. } => unreachable!(),
        // Dispatched by `main` before `run` is ever called.
        Commands::Gui | Commands::Web { .. } => unreachable!(),
        Commands::Search { query, source } => {
            let pb = spinner(&format!("Searching for '{query}'…"));
            if cli.json {
                // JSON consumers need one complete document: buffer all.
                let (pkgs, errors) = pm.search_with_errors(query, source.map(Into::into)).await;
                pb.finish_and_clear();
                warn_backends(&errors);
                print_json(&pkgs)?;
            } else {
                // Stream each backend's batch as soon as it completes, so
                // a slow source (COPR's endpoint takes ~9 s) no longer
                // hides the fast results behind a spinner. Rows are
                // numbered continuously and cached, so `brim install <#>`
                // installs exactly the row the user saw.
                let mut errors = Vec::new();
                let mut all: Vec<Package> = Vec::new();
                let mut printed = 0usize;
                let mut stream = pm.search_stream(query, source.map(Into::into)).await;
                while let Some((src, result)) = stream.next().await {
                    match result {
                        Ok(mut batch) if !batch.is_empty() => {
                            if printed == 0 {
                                pb.finish_and_clear();
                            }
                            crate::core::manager::sort_search_results(query, &mut batch);
                            printed += table::print_search_batch(&batch, printed + 1, printed == 0);
                            all.extend(batch);
                        }
                        Ok(_) => {}
                        Err(e) => errors.push((src, e)),
                    }
                }
                pb.finish_and_clear();
                warn_backends(&errors);
                if printed == 0 {
                    table::print_packages(&[]);
                } else {
                    lastsearch::save(&all).await;
                    // Summary and tip are decoration for humans; keep piped
                    // output parseable.
                    if std::io::stdout().is_terminal() {
                        println!(
                            "{}",
                            format!(
                                "{printed} results{} — install one with: brim install <#>",
                                source_counts(&all)
                            )
                            .dimmed()
                        );
                    }
                }
            }
        }
        Commands::Install { id, yes, source } => {
            let source = source.map(Into::into);
            // A pure-number id refers to a row of the last search (see
            // lastsearch.rs); anything else resolves by package id.
            let pkg = match result_number(id) {
                Some(number) => lastsearch::package_at(number).await?,
                None => resolve(&pm, id, source).await?,
            };
            if !yes && !prompt::confirm("Install", &pkg.name, Some(&pkg.source.to_string())) {
                println!("Aborted.");
                return Ok(true);
            }
            let pb = spinner(&format!("Installing '{}'…", pkg.name));
            // The resolved package goes straight to the backend: no
            // second resolve, and no room for the resolution to change
            // between the prompt and the transaction (TOCTOU).
            let result = pm.install_package(&pkg).await;
            pb.finish_and_clear();
            return Ok(report_transaction(result?));
        }
        Commands::Remove { id, yes, source } => {
            let source = source.map(Into::into);
            let pkg = resolve(&pm, id, source).await?;
            if !yes && !prompt::confirm("Remove", id, Some(&pkg.source.to_string())) {
                println!("Aborted.");
                return Ok(true);
            }
            let pb = spinner(&format!("Removing '{id}'…"));
            let result = pm.remove_package(&pkg).await;
            pb.finish_and_clear();
            return Ok(report_transaction(result?));
        }
        Commands::Upgrade { yes } => {
            if !yes && !prompt::confirm("Upgrade", "all packages", None) {
                println!("Aborted.");
                return Ok(true);
            }
            let pb = spinner("Upgrading all packages…");
            let result = pm.upgrade().await;
            pb.finish_and_clear();
            return Ok(report_transaction(result?));
        }
        Commands::List => {
            let pb = spinner("Listing installed packages…");
            let (pkgs, errors) = pm.list_installed_with_errors().await;
            pb.finish_and_clear();
            warn_backends(&errors);
            if cli.json {
                print_json(&pkgs)?;
            } else {
                table::print_packages(&pkgs);
                // The footer is decoration for humans; keep pipes clean.
                if !pkgs.is_empty() && std::io::stdout().is_terminal() {
                    println!(
                        "{}",
                        format!("{} packages{}", pkgs.len(), source_counts(&pkgs)).dimmed()
                    );
                }
            }
        }
        Commands::Updates => {
            // Updates and the installed list are fetched together: matching
            // the two fills in each update's installed version, so the table
            // can show old → new instead of just the new version.
            let pb = spinner("Checking for updates…");
            let (mut updates, update_errors) = pm.updates_with_errors().await;
            let (installed, list_errors) = pm.list_installed_with_errors().await;
            pb.finish_and_clear();
            warn_backends(&update_errors);
            warn_backends(&list_errors);
            fill_installed_versions(&mut updates, &installed);
            if cli.json {
                print_json(&updates)?;
            } else {
                table::print_updates(&updates);
                if !updates.is_empty() && std::io::stdout().is_terminal() {
                    println!();
                    println!(
                        "{}",
                        format!(
                            "{} updates pending{} — apply with: brim upgrade",
                            updates.len(),
                            source_counts(&updates)
                        )
                        .dimmed()
                    );
                }
            }
        }
        Commands::Stats => {
            let pb = spinner("Gathering system statistics…");
            let stats = pm.system_stats().await;
            pb.finish_and_clear();
            if cli.json {
                print_json(&stats)?;
            } else {
                table::print_stats(&stats);
            }
        }
        Commands::Info { id, source } => {
            let pb = spinner(&format!("Fetching info for '{id}'…"));
            let pkg = pm.info(id, source.map(Into::into)).await;
            pb.finish_and_clear();
            let pkg = pkg?;
            if cli.json {
                print_json(&pkg)?;
            } else {
                print_info(&pkg);
            }
        }
    }
    Ok(true)
}

/// Execute a `brim config` subcommand. Always returns `Ok(true)` on
/// success; unknown keys and bad values are `Err` so `main` exits 1.
fn run_config(action: &ConfigAction) -> Result<bool> {
    match action {
        ConfigAction::List => {
            if !BrimConfig::file_is_valid(&config_path()) {
                eprintln!(
                    "{}",
                    "warning: config file is not valid JSON; showing defaults".yellow()
                );
            }
            let config = BrimConfig::load();
            for key in BrimConfig::KEYS {
                println!("{} = {}", key, config.get(key).unwrap_or_default());
            }
        }
        ConfigAction::Get { key } => {
            let config = BrimConfig::load();
            match config.get(key) {
                Some(value) => println!("{value}"),
                None => return Err(unknown_key(key)),
            }
        }
        ConfigAction::Set { key, value } => {
            let Some(value) = parse_bool(value) else {
                return Err(BrimError::Parse(format!(
                    "invalid value '{value}' (expected true or false)"
                )));
            };
            let mut config = BrimConfig::load();
            if !config.set(key, value) {
                return Err(unknown_key(key));
            }
            if !BrimConfig::file_is_valid(&config_path()) {
                eprintln!(
                    "{}",
                    "warning: config file is not valid JSON; replacing it".yellow()
                );
            }
            config.save()?;
            println!("{key} = {value}");
        }
        ConfigAction::Reset => {
            BrimConfig::default().save()?;
            println!("Configuration reset to defaults.");
        }
    }
    Ok(true)
}

/// Parse a boolean argument, accepting `true` or `false` in any casing
/// (`true`, `True`, `TRUE`).
fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Interpret `id` as a 1-based search-result number: pure ASCII digits
/// only, so real package ids (even numeric-looking ones like `7zip`)
/// still resolve by name.
fn result_number(id: &str) -> Option<usize> {
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        id.parse().ok()
    } else {
        None
    }
}

/// The error for an unknown config key, listing valid keys.
fn unknown_key(key: &str) -> BrimError {
    BrimError::Parse(format!(
        "unknown config key '{key}' (keys: {})",
        BrimConfig::KEYS.join(", ")
    ))
}

/// Warn on stderr about backends that failed during a fan-out read;
/// partial results are still shown, so a "No packages found." after
/// warnings means a genuinely empty result, not a silent backend outage.
fn warn_backends(errors: &[(SourceType, BrimError)]) {
    for (source, err) in errors {
        eprintln!(
            "{}",
            format!("warning: {source} backend failed: {err}").yellow()
        );
    }
}

/// Print a value as pretty JSON. The in-house models always serialize,
/// so a failure here is defensive only; it still propagates as `Err` so
/// the process exits nonzero instead of reporting success.
fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|err| BrimError::Parse(format!("failed to encode JSON: {err}")))?;
    println!("{json}");
    Ok(())
}

/// Per-source counts for a result footer: " (Fedora: 2, COPR: 27)" —
/// empty when there is nothing to break down.
fn source_counts(pkgs: &[Package]) -> String {
    let counts: Vec<String> = [
        SourceType::FedoraOfficial,
        SourceType::Debian,
        SourceType::Copr,
        SourceType::Flatpak,
    ]
    .into_iter()
    .filter_map(|source| {
        let n = pkgs.iter().filter(|p| p.source == source).count();
        (n > 0).then(|| format!("{source}: {n}"))
    })
    .collect();
    if counts.is_empty() {
        String::new()
    } else {
        format!(" ({})", counts.join(", "))
    }
}

/// Fill each update's installed version from the installed list (matched
/// by package id), so the updates table can show old → new.
fn fill_installed_versions(updates: &mut [Package], installed: &[Package]) {
    let versions: std::collections::HashMap<&str, &str> = installed
        .iter()
        .filter_map(|p| p.installed_version.as_deref().map(|v| (p.id.as_str(), v)))
        .collect();
    for pkg in updates {
        if pkg.installed_version.is_none() {
            if let Some(version) = versions.get(pkg.id.as_str()) {
                pkg.installed_version = Some((*version).to_string());
            }
        }
    }
}

/// Resolve a package before a transaction so the confirmation prompt can
/// name the source it will come from. Uses `PackageManager::resolve`, whose
/// search fallback also finds not-yet-installed flatpaks that `info` alone
/// reports as missing. Errors (e.g. not found) propagate so the caller
/// exits before ever prompting.
async fn resolve(pm: &PackageManager, id: &str, source: Option<SourceType>) -> Result<Package> {
    let pb = spinner(&format!("Resolving '{id}'…"));
    let pkg = pm.resolve(id, source).await;
    pb.finish_and_clear();
    pkg
}

/// A spinner shown while a search or transaction is in flight.
fn spinner(message: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    if let Ok(style) = ProgressStyle::with_template("{spinner:.cyan} {msg}") {
        pb.set_style(style);
    }
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

/// Print package details for the `info` command. Untrusted fields are
/// sanitized so remote metadata cannot inject terminal escape sequences.
/// Only known fields are printed — backends leave unknowns empty.
fn print_info(pkg: &Package) {
    // Header: name with the colored source badge and status.
    println!(
        "{}  {}  {}",
        sanitize(&pkg.name).bold(),
        table::color_source(pkg.source, &pkg.source.to_string()),
        pkg.status.to_string().dimmed()
    );
    if !pkg.summary.trim().is_empty() {
        println!("{}", sanitize(&pkg.summary));
    }
    println!();

    let field = |label: &str, value: &str| {
        if !value.trim().is_empty() {
            println!("  {:<14}{}", label.dimmed(), sanitize(value));
        }
    };
    field("Version", &pkg.version);
    if let Some(installed) = &pkg.installed_version {
        field("Installed", installed);
    }
    field("ID", &pkg.id);
    field("Category", &pkg.category.to_string());
    if pkg.size_mb > 0.0 {
        field("Size", &format!("{:.1} MB", pkg.size_mb));
    }
    if let Some(license) = &pkg.license {
        field("License", license);
    }
    if let Some(homepage) = &pkg.homepage {
        field("Homepage", homepage);
    }
    if let Some(reference) = &pkg.flatpak_ref {
        field("Flatpak ref", reference);
    }
    if let Some(remote) = &pkg.flatpak_remote {
        field("Remote", remote);
    }
    if let (Some(owner), Some(project)) = (&pkg.copr_owner, &pkg.copr_project) {
        field("COPR", &format!("{owner}/{project}"));
    }
    if pkg.downloads > 0 {
        field("Downloads", &format!("{}/mo", pkg.downloads));
    }
    if !pkg.description.trim().is_empty() {
        println!();
        println!("{}", sanitize(pkg.description.trim()));
    }
}

/// Report the outcome of a transaction; returns `true` on success so the
/// caller can decide on the exit code.
fn report_transaction(result: TransactionResult) -> bool {
    let output = result.output.trim();
    if !output.is_empty() {
        // Backend output embeds remote metadata (package/repo names), so
        // it gets the same control-character stripping as other untrusted
        // strings.
        println!("{}", sanitize(output));
    }
    if result.success {
        println!("{}", result.message.green().bold());
    } else {
        eprintln!("{}", format!("error: {}", result.message).red().bold());
    }
    result.success
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_search_with_source() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["brim", "search", "htop", "--source", "fedora"]).unwrap();
        let Commands::Search { query, source } = cli.command else {
            panic!()
        };
        assert_eq!(query, "htop");
        assert!(matches!(source, Some(SourceArg::Fedora)));
        assert!(!cli.json);
    }

    #[test]
    fn cli_parses_global_json_flag() {
        use clap::Parser;
        for args in [
            vec!["brim", "--json", "list"],
            vec!["brim", "list", "--json"],
            vec!["brim", "--json", "search", "htop"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(cli.json);
        }
    }

    #[test]
    fn cli_rejects_missing_query() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["brim", "search"]).is_err());
    }

    #[test]
    fn cli_parses_install_with_source() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["brim", "install", "htop", "--source", "fedora"]).unwrap();
        let Commands::Install { id, source, yes } = cli.command else {
            panic!()
        };
        assert_eq!(id, "htop");
        assert!(matches!(source, Some(SourceArg::Fedora)));
        assert!(!yes);
    }

    #[test]
    fn cli_parses_remove_with_source() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["brim", "remove", "htop", "--source", "flatpak"]).unwrap();
        let Commands::Remove { id, source, .. } = cli.command else {
            panic!()
        };
        assert_eq!(id, "htop");
        assert!(matches!(source, Some(SourceArg::Flatpak)));
    }

    #[test]
    fn cli_parses_info_with_source() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["brim", "info", "bash", "--source", "copr"]).unwrap();
        let Commands::Info { id, source } = cli.command else {
            panic!()
        };
        assert_eq!(id, "bash");
        assert!(matches!(source, Some(SourceArg::Copr)));
    }

    #[test]
    fn cli_parses_install_without_source() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["brim", "install", "htop", "--yes"]).unwrap();
        let Commands::Install { source, yes, .. } = cli.command else {
            panic!()
        };
        assert!(source.is_none());
        assert!(yes);
    }

    #[test]
    fn cli_rejects_unknown_source() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["brim", "install", "htop", "--source", "snap"]).is_err());
    }

    #[test]
    fn cli_parses_config_subcommands() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["brim", "config", "set", "sources.copr", "false"]).unwrap();
        let Commands::Config { action } = cli.command else {
            panic!()
        };
        let ConfigAction::Set { key, value } = action else {
            panic!()
        };
        assert_eq!(key, "sources.copr");
        assert_eq!(value, "false");

        let cli = Cli::try_parse_from(["brim", "config", "list"]).unwrap();
        assert!(matches!(cli.command, Commands::Config { .. }));
    }

    #[test]
    fn cli_parses_completions_subcommand() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["brim", "completions", "bash"]).unwrap();
        let Commands::Completions { shell } = cli.command else {
            panic!()
        };
        assert!(matches!(shell, ShellArg::Bash));
        let cli = Cli::try_parse_from(["brim", "completions", "zsh"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Completions {
                shell: ShellArg::Zsh
            }
        ));
        assert!(Cli::try_parse_from(["brim", "completions", "fish"]).is_err());
    }

    #[test]
    fn completions_generate_for_both_shells() {
        use clap::CommandFactory;
        for shell in [clap_complete::Shell::Bash, clap_complete::Shell::Zsh] {
            let mut command = Cli::command();
            let mut out = Vec::new();
            clap_complete::generate(shell, &mut command, "brim", &mut out);
            let script = String::from_utf8(out).unwrap();
            assert!(
                script.contains("search"),
                "{shell} script lacks subcommands"
            );
            assert!(script.contains("completions"), "{shell} script is stale");
        }
    }

    #[test]
    fn parse_bool_accepts_true_false_any_case() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("True"), Some(true));
        assert_eq!(parse_bool("TRUE"), Some(true));
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("False"), Some(false));
        assert_eq!(parse_bool("yes"), None);
        assert_eq!(parse_bool("1"), None);
    }

    #[test]
    fn result_number_only_matches_pure_digits() {
        assert_eq!(result_number("1"), Some(1));
        assert_eq!(result_number("42"), Some(42));
        assert_eq!(result_number("7zip"), None);
        assert_eq!(result_number("htop"), None);
        assert_eq!(result_number(""), None);
        assert_eq!(result_number("1.5"), None);
        assert_eq!(result_number("+3"), None);
        assert_eq!(result_number(" 3"), None);
    }

    #[test]
    fn source_counts_breaks_down_by_source() {
        let pkgs = vec![
            Package::new("a", "a", SourceType::FedoraOfficial),
            Package::new("b", "b", SourceType::FedoraOfficial),
            Package::new("c", "c", SourceType::Flatpak),
        ];
        assert_eq!(source_counts(&pkgs), " (Fedora: 2, Flatpak: 1)");
        assert_eq!(source_counts(&[]), "");
    }

    #[test]
    fn fill_installed_versions_matches_by_id_without_overwriting() {
        let mut updates = vec![
            Package::new("htop.x86_64", "htop", SourceType::FedoraOfficial),
            Package::new("org.app", "App", SourceType::Flatpak),
            Package::new("gone.x86_64", "gone", SourceType::FedoraOfficial),
        ];
        updates[1].installed_version = Some("1.0".to_string());
        let mut installed_htop = Package::new("htop.x86_64", "htop", SourceType::FedoraOfficial);
        installed_htop.installed_version = Some("3.0".to_string());
        let installed = vec![installed_htop];
        fill_installed_versions(&mut updates, &installed);
        assert_eq!(updates[0].installed_version.as_deref(), Some("3.0"));
        // A version already set by the backend is never overwritten.
        assert_eq!(updates[1].installed_version.as_deref(), Some("1.0"));
        // Unknown ids stay empty (the table shows "—").
        assert_eq!(updates[2].installed_version, None);
    }

    #[test]
    fn exit_code_distinguishes_not_found() {
        assert_eq!(
            error_exit_code(&BrimError::NotFound("htop".into())),
            EXIT_NOT_FOUND
        );
        assert_eq!(
            error_exit_code(&BrimError::CommandFailed("boom".into())),
            EXIT_ERROR
        );
        assert_eq!(
            error_exit_code(&BrimError::BackendUnavailable("dnf5".into())),
            EXIT_ERROR
        );
    }

    #[test]
    fn report_transaction_returns_success_flag() {
        use crate::core::TransactionAction;
        let ok = TransactionResult::ok(TransactionAction::Install, "htop", "done", "");
        assert!(report_transaction(ok));
        let failed = TransactionResult::err(TransactionAction::Install, "htop", "boom", "");
        assert!(!report_transaction(failed));
    }

    #[test]
    fn print_json_failure_is_an_error() {
        use serde::ser::{Error as _, Serializer};

        // A value whose serialization always fails, so the defensive
        // error path in print_json is exercised without subprocesses.
        struct Failing;
        impl serde::Serialize for Failing {
            fn serialize<S: Serializer>(
                &self,
                _serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                Err(S::Error::custom("boom"))
            }
        }

        assert!(print_json(&Failing).is_err());
        assert!(print_json(&["htop"]).is_ok());
    }
}

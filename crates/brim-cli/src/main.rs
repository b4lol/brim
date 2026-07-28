//! Brim terminal companion — CLI frontend over the brim-core engine.

mod banner;
mod prompt;
mod table;

use brim_core::{
    config_path, BrimError, Config as BrimConfig, Package, PackageManager, Result, SourceType,
    TransactionResult,
};
use clap::{Parser, Subcommand};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "brim",
    version,
    about = "Brim — the Fedora app store & package manager"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Search for packages across all sources.
    Search {
        query: String,
        /// Restrict the search to one source.
        #[arg(long, value_enum)]
        source: Option<SourceArg>,
    },
    /// Install a package (asks for confirmation unless --yes).
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
}

#[derive(Subcommand)]
enum ConfigAction {
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
enum SourceArg {
    Fedora,
    Copr,
    Flatpak,
}

impl From<SourceArg> for SourceType {
    fn from(arg: SourceArg) -> Self {
        match arg {
            SourceArg::Fedora => SourceType::FedoraOfficial,
            SourceArg::Copr => SourceType::Copr,
            SourceArg::Flatpak => SourceType::Flatpak,
        }
    }
}

#[tokio::main]
async fn main() {
    // Rust ignores SIGPIPE at startup, which turns a closed stdout (e.g.
    // `brim list | head`) into a panic. Restore the default behaviour so
    // the process exits quietly like a well-behaved Unix tool.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    banner::print_banner();
    match run(cli).await {
        // `false` = a transaction reported failure.
        Ok(false) => std::process::exit(1),
        Ok(true) => {}
        Err(err) => {
            eprintln!("{}", format!("error: {err}").red().bold());
            std::process::exit(1);
        }
    }
}

/// Returns `Ok(false)` when a transaction itself reported failure, so the
/// single exit-code decision stays in `main`.
async fn run(cli: Cli) -> Result<bool> {
    // Config subcommands never touch the backends.
    if let Commands::Config { action } = cli.command {
        return run_config(action);
    }
    let pm = PackageManager::new();
    match cli.command {
        // Handled above, before any backend is constructed.
        Commands::Config { .. } => unreachable!(),
        Commands::Search { query, source } => {
            let pb = spinner(&format!("Searching for '{query}'…"));
            let pkgs = pm.search(&query, source.map(Into::into)).await;
            pb.finish_and_clear();
            table::print_packages(&pkgs);
        }
        Commands::Install { id, yes, source } => {
            let source = source.map(Into::into);
            let pkg = resolve(&pm, &id, source).await?;
            if !yes && !prompt::confirm("Install", &id, Some(&pkg.source.to_string())) {
                println!("Aborted.");
                return Ok(true);
            }
            let pb = spinner(&format!("Installing '{id}'…"));
            let result = pm.install(&id, source).await;
            pb.finish_and_clear();
            return Ok(report_transaction(result?));
        }
        Commands::Remove { id, yes, source } => {
            let source = source.map(Into::into);
            let pkg = resolve(&pm, &id, source).await?;
            if !yes && !prompt::confirm("Remove", &id, Some(&pkg.source.to_string())) {
                println!("Aborted.");
                return Ok(true);
            }
            let pb = spinner(&format!("Removing '{id}'…"));
            let result = pm.remove(&id, source).await;
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
            let pkgs = pm.list_installed().await;
            pb.finish_and_clear();
            table::print_packages(&pkgs);
        }
        Commands::Stats => {
            let pb = spinner("Gathering system statistics…");
            let stats = pm.system_stats().await;
            pb.finish_and_clear();
            table::print_stats(&stats);
        }
        Commands::Info { id, source } => {
            let pb = spinner(&format!("Fetching info for '{id}'…"));
            let pkg = pm.info(&id, source.map(Into::into)).await;
            pb.finish_and_clear();
            print_info(&pkg?);
        }
    }
    Ok(true)
}

/// Execute a `brim config` subcommand. Always returns `Ok(true)` on
/// success; unknown keys and bad values are `Err` so `main` exits 1.
fn run_config(action: ConfigAction) -> Result<bool> {
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
            match config.get(&key) {
                Some(value) => println!("{value}"),
                None => return Err(unknown_key(&key)),
            }
        }
        ConfigAction::Set { key, value } => {
            let Some(value) = parse_bool(&value) else {
                return Err(BrimError::Parse(format!(
                    "invalid value '{value}' (expected true or false)"
                )));
            };
            let mut config = BrimConfig::load();
            if !config.set(&key, value) {
                return Err(unknown_key(&key));
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

/// Parse a boolean argument, accepting exactly `true` or `false`.
fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The error for an unknown config key, listing valid keys.
fn unknown_key(key: &str) -> BrimError {
    BrimError::Parse(format!(
        "unknown config key '{key}' (keys: {})",
        BrimConfig::KEYS.join(", ")
    ))
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

/// Print package details for the `info` command.
fn print_info(pkg: &Package) {
    println!("{}", pkg.name.bold());
    println!("  Version:  {}", pkg.version);
    if let Some(installed) = &pkg.installed_version {
        println!("  Installed: {installed}");
    }
    println!("  Source:   {}", pkg.source);
    println!("  Status:   {}", pkg.status);
    println!("  Category: {}", pkg.category);
    println!("  Size:     {:.1} MB", pkg.size_mb);
    if let Some(license) = &pkg.license {
        println!("  License:  {license}");
    }
    if let Some(homepage) = &pkg.homepage {
        println!("  Homepage: {homepage}");
    }
    if !pkg.summary.is_empty() {
        println!("  Summary:  {}", pkg.summary);
    }
    if !pkg.description.is_empty() {
        println!();
        println!("{}", pkg.description.trim());
    }
}

/// Report the outcome of a transaction; returns `true` on success so the
/// caller can decide on the exit code.
fn report_transaction(result: TransactionResult) -> bool {
    let output = result.output.trim();
    if !output.is_empty() {
        println!("{output}");
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
    fn parse_bool_accepts_only_true_false() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("yes"), None);
        assert_eq!(parse_bool("1"), None);
    }

    #[test]
    fn report_transaction_returns_success_flag() {
        use brim_core::TransactionAction;
        let ok = TransactionResult::ok(TransactionAction::Install, "htop", "done", "");
        assert!(report_transaction(ok));
        let failed = TransactionResult::err(TransactionAction::Install, "htop", "boom", "");
        assert!(!report_transaction(failed));
    }
}

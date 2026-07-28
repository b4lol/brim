//! Tabular and summary output for packages and system statistics.

use brim_core::{Package, SourceType, SystemStats};
use colored::Colorize;

/// Maximum number of summary characters shown per row.
const SUMMARY_MAX: usize = 60;

/// Print packages as an aligned table with a colored source column.
pub fn print_packages(pkgs: &[Package]) {
    if pkgs.is_empty() {
        println!("{}", "No packages found.".dimmed());
        return;
    }

    let name_w = width(pkgs.iter().map(|p| p.name.as_str()), "NAME");
    let version_w = width(pkgs.iter().map(|p| p.version.as_str()), "VERSION");
    let source_w = width(pkgs.iter().map(|p| p.source.to_string()), "SOURCE");
    let status_w = width(pkgs.iter().map(|p| p.status.to_string()), "STATUS");

    println!(
        "{:<name_w$}  {:<version_w$}  {:<source_w$}  {:<status_w$}  SUMMARY",
        "NAME".bold(),
        "VERSION".bold(),
        "SOURCE".bold(),
        "STATUS".bold(),
    );
    for pkg in pkgs {
        let source = format!("{:<source_w$}", pkg.source.to_string());
        println!(
            "{:<name_w$}  {:<version_w$}  {}  {:<status_w$}  {}",
            pkg.name,
            pkg.version,
            color_source(pkg.source, &source),
            pkg.status.to_string(),
            truncate(&pkg.summary, SUMMARY_MAX).dimmed(),
        );
    }
}

/// Print aggregated system statistics.
pub fn print_stats(stats: &SystemStats) {
    println!("{}", "System statistics".bold());
    println!("  Installed packages: {}", stats.installed);
    println!("  Updates pending:    {}", stats.updates_pending);
    println!();
    let source_w = width(stats.sources.iter().map(|s| s.source.to_string()), "SOURCE");
    println!(
        "  {:<source_w$}  {:>9}  {:>7}",
        "SOURCE".bold(),
        "INSTALLED".bold(),
        "UPDATES".bold()
    );
    for stat in &stats.sources {
        let source = format!("{:<source_w$}", stat.source.to_string());
        println!(
            "  {}  {:>9}  {:>7}",
            color_source(stat.source, &source),
            stat.installed,
            stat.updates,
        );
    }
}

/// The widest plain-text width of `cells` and the `header`.
fn width<I, S>(cells: I, header: &str) -> usize
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    cells
        .into_iter()
        .map(|c| c.as_ref().chars().count())
        .max()
        .unwrap_or(0)
        .max(header.len())
}

/// Apply the per-source color to an already padded source cell.
fn color_source(source: SourceType, cell: &str) -> colored::ColoredString {
    match source {
        SourceType::FedoraOfficial => cell.blue(),
        SourceType::Copr => cell.magenta(),
        SourceType::Flatpak => cell.yellow(),
    }
}

/// Truncate `text` to `max` characters, appending an ellipsis when cut.
fn truncate(text: &str, max: usize) -> String {
    let text = text.replace('\n', " ");
    if text.chars().count() <= max {
        return text;
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

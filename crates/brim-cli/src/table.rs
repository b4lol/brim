//! Tabular and summary output for packages and system statistics.

use crate::sanitize::sanitize;
use brim_core::{Package, SourceType, SystemStats};
use colored::Colorize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Maximum summary column width (terminal cells) shown per row.
const SUMMARY_MAX: usize = 60;

/// Print packages as an aligned table with a colored source column.
pub fn print_packages(pkgs: &[Package]) {
    if pkgs.is_empty() {
        println!("{}", "No packages found.".dimmed());
        return;
    }

    // Render the source/status cells once; they feed both the width
    // computation and the rows.
    let sources: Vec<String> = pkgs.iter().map(|p| p.source.to_string()).collect();
    let statuses: Vec<String> = pkgs.iter().map(|p| p.status.to_string()).collect();

    let name_w = width(pkgs.iter().map(|p| p.name.as_str()), "NAME");
    let version_w = width(pkgs.iter().map(|p| p.version.as_str()), "VERSION");
    let source_w = width(sources.iter(), "SOURCE");
    let status_w = width(statuses.iter(), "STATUS");

    println!(
        "{}  {}  {}  {}  {}",
        pad("NAME", name_w).bold(),
        pad("VERSION", version_w).bold(),
        pad("SOURCE", source_w).bold(),
        pad("STATUS", status_w).bold(),
        "SUMMARY".bold(),
    );
    for (pkg, (source, status)) in pkgs.iter().zip(sources.iter().zip(&statuses)) {
        println!(
            "{}  {}  {}  {}  {}",
            pad(&sanitize(&pkg.name), name_w),
            pad(&sanitize(&pkg.version), version_w),
            color_source(pkg.source, &pad(source, source_w)),
            pad(status, status_w),
            truncate(&sanitize(&pkg.summary), SUMMARY_MAX).dimmed(),
        );
    }
}

/// Print aggregated system statistics.
pub fn print_stats(stats: &SystemStats) {
    println!("{}", "System statistics".bold());
    println!("  Installed packages: {}", stats.installed);
    println!("  Updates pending:    {}", stats.updates_pending);
    println!();
    let sources: Vec<String> = stats.sources.iter().map(|s| s.source.to_string()).collect();
    let source_w = width(sources.iter(), "SOURCE");
    println!(
        "  {}  {:>9}  {:>7}",
        pad("SOURCE", source_w).bold(),
        "INSTALLED".bold(),
        "UPDATES".bold()
    );
    for (stat, source) in stats.sources.iter().zip(&sources) {
        println!(
            "  {}  {:>9}  {:>7}",
            color_source(stat.source, &pad(source, source_w)),
            stat.installed,
            stat.updates,
        );
    }
}

/// The widest terminal-cell width of `cells` and the `header`.
fn width<I, S>(cells: I, header: &str) -> usize
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    cells
        .into_iter()
        .map(|c| UnicodeWidthStr::width(c.as_ref()))
        .max()
        .unwrap_or(0)
        .max(header.len())
}

/// Pad `text` with trailing spaces up to `width` terminal cells. Wide
/// (CJK) characters count as two cells, so columns stay aligned even
/// with non-ASCII package names.
fn pad(text: &str, width: usize) -> String {
    let padding = width.saturating_sub(UnicodeWidthStr::width(text));
    format!("{text}{}", " ".repeat(padding))
}

/// Apply the per-source color to an already padded source cell.
fn color_source(source: SourceType, cell: &str) -> colored::ColoredString {
    match source {
        SourceType::FedoraOfficial => cell.blue(),
        SourceType::Copr => cell.magenta(),
        SourceType::Flatpak => cell.yellow(),
    }
}

/// Truncate `text` to `max` terminal cells, appending an ellipsis when
/// cut. Newlines flatten to spaces; wide characters count as two cells
/// and are never split (cuts happen on char boundaries).
fn truncate(text: &str, max: usize) -> String {
    let text = text.replace('\n', " ");
    if UnicodeWidthStr::width(text.as_str()) <= max {
        return text;
    }
    // Reserve one cell for the ellipsis.
    let budget = max.saturating_sub(1);
    let mut cut = String::new();
    let mut cells = 0;
    for ch in text.chars() {
        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cells + ch_w > budget {
            break;
        }
        cells += ch_w;
        cut.push(ch);
    }
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_text() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("exact", 5), "exact");
    }

    #[test]
    fn truncate_cuts_with_ellipsis() {
        assert_eq!(truncate("hello world", 8), "hello w…");
    }

    #[test]
    fn truncate_handles_empty_and_tiny_limits() {
        assert_eq!(truncate("", 5), "");
        assert_eq!(truncate("hello", 1), "…");
        assert_eq!(truncate("hello", 0), "…");
    }

    #[test]
    fn truncate_flattens_newlines() {
        assert_eq!(truncate("a\nb", 10), "a b");
    }

    #[test]
    fn truncate_counts_wide_chars_as_two_cells() {
        // Each CJK char is 2 cells: "日本語abc" is 9 cells, fits in 9.
        assert_eq!(truncate("日本語abc", 9), "日本語abc");
        // Budget 7 cells before the ellipsis: 日本語 (6) + a (1).
        assert_eq!(truncate("日本語abc", 8), "日本語a…");
        // A wide char that would straddle the budget is dropped whole.
        assert_eq!(truncate("ab日本", 4), "ab…");
    }

    #[test]
    fn truncate_never_splits_a_char() {
        // Cutting mid-char would corrupt UTF-8; every result must be
        // valid and bounded by `max` cells plus the ellipsis.
        let out = truncate("日本語テスト", 5);
        assert!(UnicodeWidthStr::width(out.as_str()) <= 5);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn pad_aligns_wide_text() {
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("日本", 6), "日本  "); // 4 cells + 2 spaces
        assert_eq!(pad("toolong", 3), "toolong"); // never truncates
    }

    #[test]
    fn width_uses_terminal_cells() {
        assert_eq!(width(["日本"], "HEADER"), 6); // header wins
        assert_eq!(width(["日本語abc"], "H"), 9);
    }
}

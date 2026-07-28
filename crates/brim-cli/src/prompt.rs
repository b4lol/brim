//! Interactive confirmation prompt for mutating transactions.

use colored::Colorize;
use std::io::{self, BufRead, Write};

/// Ask the user to confirm `action` on `target` with a `[y/N]` prompt.
/// When `source` is given it is shown so the user knows where the
/// package will come from (e.g. `Confirm install 'htop' from Fedora?`).
///
/// Anything other than an explicit `y`/`yes` counts as "no"; end of
/// input (non-interactive shells) is also treated as "no" so the prompt
/// never hangs waiting on a closed stdin.
pub fn confirm(action: &str, target: &str, source: Option<&str>) -> bool {
    let from = match source {
        Some(source) => format!(" from {source}"),
        None => String::new(),
    };
    eprint!(
        "{} {} '{}'{}? [y/N] ",
        "Confirm".yellow().bold(),
        action.to_lowercase(),
        target,
        from
    );
    let _ = io::stderr().flush();

    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(_) => matches!(line.trim().to_lowercase().as_str(), "y" | "yes"),
        Err(_) => false,
    }
}

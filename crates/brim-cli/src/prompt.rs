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

    read_confirmation(io::stdin().lock())
}

/// Read one line from `reader`; only an explicit `y`/`yes` confirms.
/// Split from the prompt so the decision logic is testable without a
/// real stdin.
fn read_confirmation<R: BufRead>(mut reader: R) -> bool {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(_) => matches!(line.trim().to_lowercase().as_str(), "y" | "yes"),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn answer(input: &str) -> bool {
        read_confirmation(Cursor::new(input.to_string()))
    }

    #[test]
    fn accepts_y_and_yes_case_insensitively() {
        assert!(answer("y\n"));
        assert!(answer("Y\n"));
        assert!(answer("yes\n"));
        assert!(answer("YES\n"));
        assert!(answer("  yes  \n"));
    }

    #[test]
    fn rejects_everything_else() {
        assert!(!answer("n\n"));
        assert!(!answer("no\n"));
        assert!(!answer("yep\n"));
        assert!(!answer("\n"));
    }

    #[test]
    fn treats_eof_as_no() {
        assert!(!answer(""));
    }
}

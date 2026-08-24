//! ASCII art banner printed when the CLI starts.

use colored::Colorize;
use std::io::IsTerminal;

/// Print the BRIM banner in bold cyan — only on an interactive terminal,
/// so piped output (`brim list | grep …`) stays clean.
pub fn print_banner() {
    if !std::io::stdout().is_terminal() {
        return;
    }
    let banner = r#"
 ____  ____  ___ __  __
| __ )|  _ \|_ _|  \/  |
|  _ \| |_) || || |\/| |
| |_) |  _ < | || |  | |
|____/|_| \_\___|_|  |_|
"#;
    println!("{}", banner.cyan().bold());
}

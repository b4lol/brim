//! ASCII art banner printed when the CLI starts.

use colored::Colorize;

/// Print the BRIM banner in bold cyan.
pub fn print_banner() {
    let banner = r#"
 ____  ____  ___ __  __
| __ )|  _ \|_ _|  \/  |
|  _ \| |_) || || |\/| |
| |_) |  _ < | || |  | |
|____/|_| \_\___|_|  |_|
"#;
    println!("{}", banner.cyan().bold());
}

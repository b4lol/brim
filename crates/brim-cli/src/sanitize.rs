//! Sanitization of untrusted strings before they reach the terminal.
//!
//! Package metadata comes from remote sources (dnf, COPR, Flathub) and
//! can contain control characters — most dangerously ESC, which starts
//! terminal escape sequences. Stripping them keeps a malicious summary
//! or homepage from rewriting the user's terminal.

/// Remove control characters from `text`, keeping newlines and tabs.
/// This covers ESC (`\x1b`), BEL (`\x07`), and the rest of the C0/C1
/// control ranges, so no escape sequence survives to the terminal.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escape_and_bell() {
        // ESC[2J would clear the screen; BEL rings the terminal bell.
        assert_eq!(sanitize("evil\x1b[2J\x07text"), "evil[2Jtext");
    }

    #[test]
    fn strips_other_c0_controls() {
        assert_eq!(sanitize("a\x00b\x1fc\x7fd"), "abcd");
    }

    #[test]
    fn strips_c1_controls() {
        // C1 controls (U+0080..U+009F) include CSI on some terminals.
        assert_eq!(sanitize("a\u{9b}b"), "ab");
    }

    #[test]
    fn keeps_newlines_and_tabs() {
        assert_eq!(sanitize("line one\nline\ttwo"), "line one\nline\ttwo");
    }

    #[test]
    fn leaves_clean_text_untouched() {
        assert_eq!(sanitize("A normal summary."), "A normal summary.");
        assert_eq!(sanitize(""), "");
    }
}

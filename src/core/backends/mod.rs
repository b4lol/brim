//! Concrete package management backends.

pub mod apt;
pub(crate) mod cache;
pub mod copr;
pub mod dnf5;
pub mod flatpak;

use std::time::Duration;

use tokio::process::Command;

use crate::core::error::{BrimError, Result};

/// Timeout for cheap availability probes (`--version` and friends).
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for read-only query commands (search/list/info/remote-ls/curl).
/// Long enough for a cold-cache dnf5 fallback (~8 s measured) and slow
/// flatpak searches (~4 s measured), short enough that a wedged backend
/// cannot stall the UI for a minute. Transactions (install/remove/
/// upgrade) deliberately run without a timeout: real system changes may
/// take a long time.
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_secs(20);

/// Probe whether `program args` runs successfully, with [`PROBE_TIMEOUT`].
///
/// Every spawn forces `LC_ALL=C`; spawn failure or timeout means
/// unavailable.
pub(crate) async fn probe(program: &str, args: &[&str]) -> bool {
    let future = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        // A timed-out probe must not leave the child running.
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(PROBE_TIMEOUT, future).await {
        Ok(Ok(output)) => output.status.success(),
        _ => false,
    }
}

/// Reject a user-supplied argument (query, package id) that a backend CLI
/// would parse as a flag. `dnf5` has no `--` end-of-options support, so
/// rejection is the only safe option there; other backends additionally
/// pass `--`, making this defense in depth.
pub(crate) fn validate_arg(arg: &str) -> Result<()> {
    if arg.starts_with('-') {
        return Err(BrimError::InvalidInput(format!(
            "argument must not start with '-': {arg}"
        )));
    }
    Ok(())
}

/// Wrap a spawn/read I/O failure with the program name, so the error says
/// *which* backend tool could not be executed instead of surfacing a bare
/// `std::io::Error`.
pub(crate) fn spawn_error(program: &str, e: std::io::Error) -> BrimError {
    BrimError::CommandFailed(format!("failed to execute {program}: {e}"))
}

/// System package tools (dnf5, the `dnf copr` plugin) modify the system
/// and refuse to run as a regular user. Fail fast with an actionable
/// message instead of surfacing the tool's raw refusal text after the
/// fact.
pub(crate) fn require_root(tool: &str) -> Result<()> {
    // SAFETY: geteuid() cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        Ok(())
    } else {
        Err(BrimError::PrivilegeRequired(privilege_message(tool)))
    }
}

/// The actionable message for a non-root transaction attempt (pure, so
/// the wording is unit-testable).
fn privilege_message(tool: &str) -> String {
    format!("{tool} transactions require root — re-run with sudo (e.g. 'sudo brim upgrade')")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_arg_rejects_flag_like_input() {
        assert!(validate_arg("--assumeyes").is_err());
        assert!(validate_arg("-q").is_err());
        assert!(matches!(
            validate_arg("--enablerepo=x"),
            Err(BrimError::InvalidInput(_))
        ));
        assert!(validate_arg("htop").is_ok());
        assert!(validate_arg("htop.x86_64").is_ok());
        assert!(validate_arg("org.mozilla.firefox").is_ok());
        assert!(validate_arg("owner/project").is_ok());
        assert!(validate_arg("").is_ok());
    }

    #[test]
    fn privilege_message_is_actionable() {
        let message = privilege_message("dnf5");
        assert!(message.contains("dnf5"));
        assert!(message.contains("sudo"), "message must guide: {message}");
        // The dedicated variant displays the message verbatim (no
        // "command failed:" prefix — nothing was executed).
        let err = BrimError::PrivilegeRequired(message.clone());
        assert_eq!(err.to_string(), message);
    }
}

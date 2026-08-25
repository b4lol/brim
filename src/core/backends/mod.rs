//! Concrete package management backends.

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
}

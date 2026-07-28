//! Concrete package management backends.

pub mod copr;
pub mod dnf5;
pub mod flatpak;

use std::time::Duration;

use tokio::process::Command;

/// Timeout for cheap availability probes (`--version` and friends).
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for read-only query commands (search/list/info/remote-ls/curl).
/// Transactions (install/remove/upgrade) deliberately run without a
/// timeout: real system changes may take a long time.
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_secs(60);

/// Probe whether `program args` runs successfully, with [`PROBE_TIMEOUT`].
///
/// Every spawn forces `LC_ALL=C`; spawn failure or timeout means
/// unavailable.
pub(crate) async fn probe(program: &str, args: &[&str]) -> bool {
    let future = Command::new(program).args(args).env("LC_ALL", "C").output();
    match tokio::time::timeout(PROBE_TIMEOUT, future).await {
        Ok(Ok(output)) => output.status.success(),
        _ => false,
    }
}

//! Error type shared by all Brim backends and frontends.

use thiserror::Error;

/// Unified error type for Brim operations.
#[derive(Debug, Error)]
pub enum BrimError {
    /// A backend's underlying tool is not installed or not usable.
    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),
    /// A shell command exited unsuccessfully.
    #[error("command failed: {0}")]
    CommandFailed(String),
    /// Command output could not be parsed.
    #[error("failed to parse output: {0}")]
    Parse(String),
    /// The requested package does not exist.
    #[error("package not found: {0}")]
    NotFound(String),
    /// User-supplied input was rejected before reaching a backend.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// A transaction needs root privileges; the message tells the user
    /// exactly how to re-run (e.g. with `sudo`).
    #[error("{0}")]
    PrivilegeRequired(String),
    /// An HTTP request failed (network error or non-2xx status).
    #[error("http error: {0}")]
    Http(String),
    /// I/O failure while spawning or reading from a process.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Convenience alias for results returned by Brim operations.
pub type Result<T> = std::result::Result<T, BrimError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_messages() {
        assert_eq!(
            BrimError::BackendUnavailable("dnf5".into()).to_string(),
            "backend unavailable: dnf5"
        );
        assert_eq!(
            BrimError::NotFound("htop".into()).to_string(),
            "package not found: htop"
        );
    }

    #[test]
    fn io_error_converts_via_from() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "spawn failed");
        let err: BrimError = io.into();
        assert!(matches!(err, BrimError::Io(_)));
        assert!(err.to_string().contains("spawn failed"));
    }

    #[test]
    fn http_error_displays() {
        assert_eq!(
            BrimError::Http("GET x returned 404".into()).to_string(),
            "http error: GET x returned 404"
        );
    }
}

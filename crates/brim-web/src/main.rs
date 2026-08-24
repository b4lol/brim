//! Brim web frontend: hyper-based REST API plus an embedded SPA.

mod routes;
mod static_files;

use std::convert::Infallible;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use brim_core::PackageManager;
use clap::Parser;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

/// Maximum number of concurrent connections; further accepts wait for a permit.
const MAX_CONNECTIONS: usize = 64;

/// Brim web UI and REST API server.
#[derive(Parser)]
#[command(name = "brim-web", about = "Brim web UI and REST API", version)]
struct Args {
    /// Port to listen on (bound to 127.0.0.1 only).
    #[arg(long, default_value_t = 8080)]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));

    let mgr = Arc::new(PackageManager::new_async().await);
    let token = session_token()?;
    let state = Arc::new(routes::State::new(mgr, token.clone()));
    let listener = TcpListener::bind(addr).await?;
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    if std::io::stdout().is_terminal() {
        println!("Brim web UI listening on http://{addr}/#token={}", &*token);
    } else {
        // Piped output or a journal may be readable by other local users:
        // keep the token out of it and hand it over via a private file.
        let token_file = write_token_file(&format!("http://{addr}/#token={}", &*token))?;
        println!(
            "Brim web UI listening on http://{addr} (token URL in {})",
            token_file.display()
        );
    }

    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(err) => {
                // Transient errors (e.g. fd exhaustion) must not kill the server.
                eprintln!("accept error: {err}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // Backpressure: wait for a free slot instead of spawning unbounded tasks.
        let Ok(permit) = permits.clone().acquire_owned().await else {
            continue; // semaphore closed: server is shutting down
        };
        let state = state.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(routes::handle(req, state).await) }
            });
            if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                eprintln!("connection error: {err}");
            }
            drop(permit);
        });
    }
}

/// Generate a random per-session API token (128 bits, hex-encoded).
///
/// The token is printed as part of the URL fragment, so only someone who
/// can see this terminal session can drive the API.
fn session_token() -> std::io::Result<Arc<str>> {
    use std::io::Read as _;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(hex.into())
}

/// Write the full token URL to a private file and return its path.
///
/// Used when stdout is not a TTY: the file gets 0600 permissions inside a
/// 0700 directory so only the current user can read the token.
fn write_token_file(url: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = token_file_path();
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "token file path has no parent")
    })?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(&path, format!("{url}\n"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

/// Path of the token file: `$XDG_RUNTIME_DIR/brim-web/url` when set, else
/// `temp_dir()/brim-web-<uid>/url`.
fn token_file_path() -> PathBuf {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime_dir) if !runtime_dir.is_empty() => {
            PathBuf::from(runtime_dir).join("brim-web")
        }
        _ => std::env::temp_dir().join(format!("brim-web-{}", uid())),
    };
    dir.join("url")
}

/// Current real UID, parsed from /proc (the crate is Linux-only; this
/// avoids a libc dependency just for `getuid`). Falls back to the PID so
/// the directory name stays unique even if /proc is unavailable.
fn uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("Uid:")?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
        })
        .unwrap_or_else(std::process::id)
}

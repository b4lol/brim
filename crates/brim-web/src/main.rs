//! Brim web frontend: hyper-based REST API plus an embedded SPA.

mod routes;
mod static_files;

use std::convert::Infallible;
use std::net::SocketAddr;
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

    let mgr = Arc::new(PackageManager::new());
    let listener = TcpListener::bind(addr).await?;
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    println!("Brim web UI listening on http://{addr}");

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
        let mgr = mgr.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let mgr = mgr.clone();
                async move { Ok::<_, Infallible>(routes::handle(req, mgr).await) }
            });
            if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                eprintln!("connection error: {err}");
            }
            drop(permit);
        });
    }
}

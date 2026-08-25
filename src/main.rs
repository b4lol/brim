//! Brim — the Fedora app store & package manager. Single binary dispatching
//! to the CLI, the GTK4 desktop app, or the web UI.

mod cli;
mod core;
mod gui;
mod web;

use clap::Parser;
use cli::{Cli, Commands};

fn main() {
    // Rust ignores SIGPIPE at startup, which turns a closed stdout (e.g.
    // `brim list | head`) into a panic. Restore the default behaviour so
    // the process exits quietly like a well-behaved Unix tool.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    match &cli.command {
        // The GUI must not start inside a tokio runtime.
        Commands::Gui => std::process::exit(i32::from(gui::run().get())),
        Commands::Web { port } => {
            let runtime = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");
            runtime.block_on(web::run(*port));
        }
        _ => {
            let runtime = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");
            std::process::exit(runtime.block_on(cli::run(&cli)));
        }
    }
}

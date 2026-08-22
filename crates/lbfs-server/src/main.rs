//! `lbfs-server --config /etc/lbfs.toml`
//!
//! Startup, and nothing else: everything past `serve` is [`lbfs_server`]. The
//! failure modes here are all the same shape — the operator got something
//! wrong before a client ever connected — so they print one line and exit
//! nonzero rather than being wrapped in a general-purpose error type.

#![deny(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use lbfs_server::config::{Allowlist, Config, ConfigError};
use lbfs_server::rpc;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "lbfs-server", about = "Network-proxied FUSE filesystem server")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long)]
    config: PathBuf,
}

#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error("reading {path}: {source}")]
    ReadConfig {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing config: {0}")]
    Config(#[from] ConfigError),
    #[error("binding {addr}: {source}")]
    Bind {
        addr: String,
        source: std::io::Error,
    },
    #[error("starting the runtime: {0}")]
    Runtime(std::io::Error),
    #[error("serving: {0}")]
    Serve(std::io::Error),
}

fn main() {
    if let Err(e) = run() {
        eprintln!("lbfs-server: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), StartupError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let text = std::fs::read_to_string(&cli.config).map_err(|source| StartupError::ReadConfig {
        path: cli.config.display().to_string(),
        source,
    })?;
    let cfg = Config::from_toml(&text)?;
    let allow = Allowlist::new(&cfg.allowed_paths)?;
    tracing::info!(
        exports = ?cfg.allowed_paths,
        max_inflight = cfg.max_inflight,
        max_io_size = cfg.max_io_size,
        fsync = ?cfg.fsync,
        "configuration loaded"
    );

    // A multi-threaded runtime by default: the request tasks are the point of
    // the task-per-request design, and several of the backend operations park
    // on `spawn_blocking`.
    let rt = tokio::runtime::Runtime::new().map_err(StartupError::Runtime)?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&cfg.listen)
            .await
            .map_err(|source| StartupError::Bind {
                addr: cfg.listen.clone(),
                source,
            })?;
        match listener.local_addr() {
            Ok(addr) => tracing::info!(%addr, "listening"),
            Err(e) => tracing::warn!(error = %e, "listening on an unknown address"),
        }
        rpc::serve(listener, Arc::new(cfg), Arc::new(allow))
            .await
            .map_err(StartupError::Serve)
    })
}

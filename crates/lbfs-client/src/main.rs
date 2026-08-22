//! `lbfs-client <server:port> <remote-path> <mountpoint>`
//!
//! Connect, attach, mount, wait for a signal, unmount. Everything that happens
//! between the mount and the signal is [`lbfs_client::fuse`] and
//! [`lbfs_client::conn`]; what is here is the order those two are started in
//! and the failures that can happen before there is a mount to report them
//! through.
//!
//! Which is why the connection comes first. A wrong port, an export the server
//! does not offer, a version mismatch — each of those is an operator mistake,
//! and each has a distinct handshake status precisely so this can print which
//! one it was (spec §8). Mounting first would turn all three into an empty
//! directory whose every operation answers `EIO`.

#![deny(unsafe_code)]

use std::net::{SocketAddr, ToSocketAddrs};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use lbfs_client::conn::{ConnectError, Connection};
use lbfs_client::fuse::{mount_options, LbfsFuse};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "lbfs-client", about = "Mount an lbfs export over the network")]
struct Cli {
    /// The server to attach to, as `host:port`.
    server: String,

    /// The exported path, absolute, as the server sees it.
    remote_path: PathBuf,

    /// Where to mount it locally.
    mountpoint: PathBuf,

    /// How long the kernel may trust a cached name or attribute, in seconds.
    ///
    /// Zero disables both caches (spec §7). The default suits the one-client
    /// assumption the whole design rests on; lower it if something else is
    /// writing to the export behind this mount's back.
    #[arg(long, default_value_t = 1.0)]
    attr_timeout: f64,

    /// Let other users on this machine reach the mount.
    #[arg(long)]
    allow_other: bool,

    /// Ask `fusermount3` to unmount if this process dies without cleaning up.
    ///
    /// Implies `allow_other`, and needs `user_allow_other` in
    /// `/etc/fuse.conf`, which is why it is not the default.
    #[arg(long)]
    auto_unmount: bool,

    /// Write through to the server instead of letting the kernel aggregate
    /// dirty pages.
    ///
    /// The writeback cache is on by default because letting the kernel
    /// coalesce small writes is the single largest win for build workloads
    /// (spec §7). The flag travels in `HELLO`: the server reads an `OPEN`'s
    /// flags differently depending on it, so both ends must agree, and this is
    /// the only place that knows.
    #[arg(long)]
    no_writeback: bool,
}

/// Everything that can go wrong before the mount exists.
#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error("{addr}: {source}")]
    Resolve {
        addr: String,
        source: std::io::Error,
    },
    #[error("{0}: no address resolved")]
    NoAddress(String),
    #[error("--attr-timeout must be a non-negative, finite number of seconds")]
    AttrTimeout,
    #[error("the remote path must be absolute")]
    RelativeRemotePath,
    #[error("starting the runtime: {0}")]
    Runtime(std::io::Error),
    #[error("connecting to {addr}: {source}")]
    Connect {
        addr: SocketAddr,
        source: ConnectError,
    },
    #[error("mounting {path}: {source}")]
    Mount {
        path: String,
        source: std::io::Error,
    },
    #[error("installing a signal handler: {0}")]
    Signals(std::io::Error),
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lbfs-client: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), StartupError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let ttl = attr_timeout(cli.attr_timeout)?;
    if !cli.remote_path.is_absolute() {
        // The server matches the path against its allowlist after resolving it
        // from its own working directory, so a relative one is at best a
        // confusing denial.
        return Err(StartupError::RelativeRemotePath);
    }
    let addr = resolve(&cli.server)?;
    let writeback = !cli.no_writeback;

    // Multi-threaded on purpose: one FUSE dispatch thread feeds it, and the
    // whole point of the bridge is that the requests it spawns overlap.
    let rt = tokio::runtime::Runtime::new().map_err(StartupError::Runtime)?;
    let export = cli.remote_path.as_os_str().as_bytes();
    let (conn, limits, _root) = rt
        .block_on(Connection::connect(addr, export, writeback))
        .map_err(|source| StartupError::Connect { addr, source })?;

    let opts = mount_options(limits.max_io_size, cli.allow_other, cli.auto_unmount);
    let fs = LbfsFuse::new(conn, rt.handle().clone(), ttl, writeback);
    let session =
        fuser::spawn_mount2(fs, &cli.mountpoint, &opts).map_err(|source| StartupError::Mount {
            path: cli.mountpoint.display().to_string(),
            source,
        })?;
    tracing::info!(
        mountpoint = %cli.mountpoint.display(),
        %addr,
        remote = %cli.remote_path.display(),
        "mounted"
    );

    rt.block_on(wait_for_signal())
        .map_err(StartupError::Signals)?;

    // This is the drain, not just the unmount. `umount(2)` syncs the
    // superblock before it detaches, so the kernel writes back every dirty
    // page first — as ordinary `WRITE` callbacks, serviced by a session thread
    // that is still running and a connection that is still open — and this
    // does not return until it has. Dropping the session before the connection
    // is therefore the whole of "unmount, drain, exit" (spec §7); reversing the
    // two would fail those last writes with `EIO` and lose the data.
    tracing::info!("unmounting");
    drop(session);
    Ok(())
}

/// A dead connection is not a reason to exit: the mount stays present and
/// answers `EIO` until somebody unmounts it (spec §7). Only a signal ends the
/// process.
async fn wait_for_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let received = tokio::select! {
        _ = interrupt.recv() => "SIGINT",
        _ = terminate.recv() => "SIGTERM",
    };
    tracing::info!(signal = received, "shutting down");
    Ok(())
}

fn attr_timeout(secs: f64) -> Result<Duration, StartupError> {
    if !secs.is_finite() || secs < 0.0 {
        return Err(StartupError::AttrTimeout);
    }
    Duration::try_from_secs_f64(secs).map_err(|_| StartupError::AttrTimeout)
}

/// `host:port` to one address.
///
/// Blocking DNS, which is correct here: this runs once, before the runtime has
/// anything else to do, and a mount that cannot resolve its server has nothing
/// to get on with in the meantime.
fn resolve(addr: &str) -> Result<SocketAddr, StartupError> {
    let mut resolved = addr
        .to_socket_addrs()
        .map_err(|source| StartupError::Resolve {
            addr: addr.to_string(),
            source,
        })?;
    resolved
        .next()
        .ok_or_else(|| StartupError::NoAddress(addr.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_the_documented_invocation() {
        use clap::CommandFactory;
        Cli::command().debug_assert();

        let cli = Cli::parse_from([
            "lbfs-client",
            "10.0.0.2:7000",
            "/srv/exports/a",
            "/mnt/lbfs",
        ]);
        assert_eq!(cli.server, "10.0.0.2:7000");
        assert_eq!(cli.remote_path, PathBuf::from("/srv/exports/a"));
        assert_eq!(cli.mountpoint, PathBuf::from("/mnt/lbfs"));
        // Spec §7: caching on, writeback on, mount private to its owner.
        assert_eq!(cli.attr_timeout, 1.0);
        assert!(!cli.no_writeback);
        assert!(!cli.allow_other);
        assert!(!cli.auto_unmount);
    }

    #[test]
    fn attr_timeout_accepts_zero_and_fractions_and_refuses_nonsense() {
        assert_eq!(attr_timeout(0.0).unwrap(), Duration::ZERO);
        assert_eq!(attr_timeout(1.0).unwrap(), Duration::from_secs(1));
        assert_eq!(attr_timeout(0.5).unwrap(), Duration::from_millis(500));
        assert!(attr_timeout(-1.0).is_err());
        assert!(attr_timeout(f64::NAN).is_err());
        assert!(attr_timeout(f64::INFINITY).is_err());
    }

    #[test]
    fn resolve_takes_a_literal_address() {
        assert_eq!(
            resolve("127.0.0.1:7000").unwrap(),
            "127.0.0.1:7000".parse::<SocketAddr>().unwrap()
        );
        assert!(resolve("127.0.0.1").is_err());
    }
}

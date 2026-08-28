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
use lbfs_client::fuse::{session_config, LbfsFuse};
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

    /// How long the kernel may trust a cached name, in seconds.
    ///
    /// Defaults to `--attr-timeout`, which is what this client did before the
    /// two became separable. Raising it alone suits a workload that resolves
    /// the same paths repeatedly and reads their attributes rarely — a build
    /// tree is the case in point. Zero disables dentry caching. It reaches
    /// `LOOKUP`, `MKDIR`, `SYMLINK` and `LINK` replies; a file this mount
    /// created, and a name it learned from a directory listing, use
    /// `--attr-timeout` for both lifetimes because FUSE's reply for those
    /// carries only one.
    #[arg(long)]
    entry_timeout: Option<f64>,

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

    /// Run this many fuser event-loop threads instead of one.
    ///
    /// Off by default, and expected to stay off on a two-vCPU guest: the
    /// session thread peaks at 15.6% of a core under the heaviest shape
    /// measured, and a second event loop competes with the tokio workers for
    /// the other core. Each thread holds a resident 16 MiB receive buffer that
    /// does not shrink to the negotiated I/O size, so four threads cost 64 MiB.
    /// Pair it with `--fuse-clone-fd` or most of the benefit stays behind a
    /// shared descriptor. Linux only, 1 to 64.
    #[arg(long)]
    fuse_threads: Option<usize>,

    /// Give each event-loop thread its own `/dev/fuse` descriptor.
    ///
    /// `FUSE_DEV_IOC_CLONE`, Linux 4.5 and up. Without it every thread reads
    /// one descriptor and one kernel queue, which is the serialisation extra
    /// threads exist to remove. Means nothing on its own — pass
    /// `--fuse-threads` too.
    #[arg(long)]
    fuse_clone_fd: bool,
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
    #[error("--fuse-threads must be between 1 and 64")]
    FuseThreads,
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
    #[error("the FUSE session ended before a shutdown signal arrived")]
    SessionEnded,
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
    let entry_ttl = entry_timeout(cli.entry_timeout, ttl)?;
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

    // Before the mount, not after. A signal arriving in the window between
    // `spawn_mount` returning and the handlers being installed would take its
    // default action and kill this process with a mount already on the
    // directory and nothing left to unmount it. Not before `connect`, though:
    // registering a handler suppresses the default action whether or not
    // anything is awaiting it, so an earlier registration would make Ctrl-C do
    // nothing for as long as a silent server can hold the handshake open.
    let mut signals = rt.block_on(async { Signals::install() })?;

    let n_threads = event_loop_threads(cli.fuse_threads)?;
    let cfg = session_config(
        limits.max_io_size,
        cli.allow_other,
        cli.auto_unmount,
        n_threads,
        cli.fuse_clone_fd,
    );
    let fs = LbfsFuse::new(conn, rt.handle().clone(), ttl, entry_ttl, writeback);
    let session =
        fuser::spawn_mount(fs, &cli.mountpoint, &cfg).map_err(|source| StartupError::Mount {
            path: cli.mountpoint.display().to_string(),
            source,
        })?;
    tracing::info!(
        mountpoint = %cli.mountpoint.display(),
        %addr,
        remote = %cli.remote_path.display(),
        "mounted"
    );

    let ending = rt.block_on(wait_for_shutdown(&mut signals, &session));

    // This is the drain, not just the unmount. `umount(2)` syncs the
    // superblock before it detaches, so the kernel writes back every dirty
    // page first — as ordinary `WRITE` callbacks, serviced by a session thread
    // that is still running and a connection that is still open — and this
    // does not return until it has. Dropping the session before the connection
    // is therefore the whole of "unmount, drain, exit" (spec §7); reversing the
    // two would fail those last writes with `EIO` and lose the data.
    tracing::info!("unmounting");
    drop(session);
    match ending {
        Ending::Signalled => Ok(()),
        Ending::SessionEnded => Err(StartupError::SessionEnded),
    }
}

/// The two signals that end this process, held from before the mount exists.
struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

impl Signals {
    /// Must be called inside the runtime: the streams register with tokio's
    /// signal driver.
    fn install() -> Result<Signals, StartupError> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(Signals {
            interrupt: signal(SignalKind::interrupt()).map_err(StartupError::Signals)?,
            terminate: signal(SignalKind::terminate()).map_err(StartupError::Signals)?,
        })
    }
}

/// Why the process is stopping.
enum Ending {
    Signalled,
    /// The FUSE session loop stopped on its own — somebody ran
    /// `fusermount3 -u`, or `init` refused the mount.
    SessionEnded,
}

/// How long to leave a dead session unnoticed.
///
/// Nobody is waiting on the answer, so this trades promptness for not holding a
/// thread parked on a `join` that usually never returns.
const SESSION_POLL: Duration = Duration::from_millis(200);

/// Wait for a signal, or for the mount to end underneath this process.
///
/// A dead *connection* is not a reason to exit: the mount stays present and
/// answers `EIO` until somebody unmounts it (spec §7). A dead *session* is,
/// because there is no mount left to serve — and the `init` path that refuses a
/// kernel without the writeback cache reaches exactly this state, having
/// already returned `Ok` from `spawn_mount`. Without this the process would
/// wait for a signal that is not coming, holding the runtime, the socket and an
/// `ENOTCONN` mountpoint.
///
/// Polled rather than awaited because `BackgroundSession` hands out its
/// `JoinHandle` but no way to await it, and `join` would need a thread of its
/// own for as long as the mount lives.
async fn wait_for_shutdown(signals: &mut Signals, session: &fuser::BackgroundSession) -> Ending {
    loop {
        tokio::select! {
            _ = signals.interrupt.recv() => {
                tracing::info!(signal = "SIGINT", "shutting down");
                return Ending::Signalled;
            }
            _ = signals.terminate.recv() => {
                tracing::info!(signal = "SIGTERM", "shutting down");
                return Ending::Signalled;
            }
            () = tokio::time::sleep(SESSION_POLL) => {
                if session.guard.is_finished() {
                    tracing::warn!("the FUSE session ended without a shutdown signal");
                    return Ending::SessionEnded;
                }
            }
        }
    }
}

fn attr_timeout(secs: f64) -> Result<Duration, StartupError> {
    if !secs.is_finite() || secs < 0.0 {
        return Err(StartupError::AttrTimeout);
    }
    Duration::try_from_secs_f64(secs).map_err(|_| StartupError::AttrTimeout)
}

/// One to sixty-four event loops, or none named at all.
///
/// Zero is the value worth catching here rather than downstream: `Session::run`
/// answers a zero with `io::Error::other("n_threads")`, which reaches the
/// operator as a mount failure with no explanation in it. The upper bound is
/// arbitrary and generous — sixty-four threads would reserve a gigabyte of
/// receive buffer, which is more than the guests have.
fn event_loop_threads(n: Option<usize>) -> Result<Option<usize>, StartupError> {
    match n {
        None => Ok(None),
        Some(n) if (1..=64).contains(&n) => Ok(Some(n)),
        Some(_) => Err(StartupError::FuseThreads),
    }
}

/// The name lifetime, falling back to the attribute lifetime when the operator
/// named only one.
///
/// A fallback rather than a constant default, so `--attr-timeout 0` keeps
/// disabling both caches the way it always did, and a mount that names neither
/// flag behaves as every mount did before the two became separable.
fn entry_timeout(entry: Option<f64>, attr: Duration) -> Result<Duration, StartupError> {
    match entry {
        None => Ok(attr),
        Some(secs) => attr_timeout(secs),
    }
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
        assert_eq!(cli.entry_timeout, None);
        assert!(!cli.no_writeback);
        assert!(!cli.allow_other);
        assert!(!cli.auto_unmount);
    }

    /// Absent means "the same as the attribute lifetime", which is what every
    /// mount did before `entry_with_ttls` made the two separable. Present
    /// means what it says, including zero, which disables dentry caching on
    /// its own.
    #[test]
    fn the_entry_lifetime_falls_back_to_the_attribute_lifetime() {
        let attr = Duration::from_millis(500);
        assert_eq!(entry_timeout(None, attr).unwrap(), attr);
        assert_eq!(
            entry_timeout(Some(60.0), attr).unwrap(),
            Duration::from_secs(60)
        );
        assert_eq!(entry_timeout(Some(0.0), attr).unwrap(), Duration::ZERO);
        assert!(entry_timeout(Some(-1.0), attr).is_err());
        assert!(entry_timeout(Some(f64::NAN), attr).is_err());
    }

    /// The flag parses, and its absence parses as absence rather than as a
    /// number somebody has to remember the meaning of.
    #[test]
    fn the_entry_timeout_flag_parses() {
        let cli = Cli::parse_from([
            "lbfs-client",
            "--attr-timeout",
            "0.5",
            "10.0.0.2:7000",
            "/srv/exports/a",
            "/mnt/lbfs",
        ]);
        assert_eq!(cli.entry_timeout, None);

        let split = Cli::parse_from([
            "lbfs-client",
            "--attr-timeout",
            "0.5",
            "--entry-timeout",
            "60",
            "10.0.0.2:7000",
            "/srv/exports/a",
            "/mnt/lbfs",
        ]);
        assert_eq!(split.attr_timeout, 0.5);
        assert_eq!(split.entry_timeout, Some(60.0));
    }

    #[test]
    fn event_loop_threads_refuses_zero_and_absurd_counts() {
        assert_eq!(event_loop_threads(None).unwrap(), None);
        assert_eq!(event_loop_threads(Some(1)).unwrap(), Some(1));
        assert_eq!(event_loop_threads(Some(64)).unwrap(), Some(64));
        assert!(event_loop_threads(Some(0)).is_err());
        assert!(event_loop_threads(Some(65)).is_err());
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

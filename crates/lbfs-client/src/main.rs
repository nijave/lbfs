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
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use lbfs_client::conn::{ConnectError, Connection};
use lbfs_client::fuse::{session_config, LbfsFuse};
use lbfs_client::readahead;
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
    /// the other core. Each thread allocates a 16 MiB receive buffer; the
    /// measured resident cost is about 2 MB per thread under a 1 MiB
    /// negotiated I/O size, since pages fault in only as far as requests
    /// touch them. Pair it with `--fuse-clone-fd` or most of the benefit
    /// stays behind a shared descriptor. Linux only, 1 to 64.
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

    /// Readahead to ask the kernel for on this mount's backing device, in KiB.
    ///
    /// Defaults to the negotiated `max_io_size / 1024`, where the measured
    /// throughput curve flattens (`docs/benchmarks/2026-08-28-readahead.md`),
    /// and never derives below the kernel's own default of 128; `0` skips the
    /// attempt entirely. Written to the mount's
    /// `/sys/class/bdi/<dev>/read_ahead_kb` after the mount comes up, best
    /// effort: the knob is root-owned, so an unprivileged client logs the
    /// command an operator needs and carries on at the kernel's default of
    /// 128 — about half of buffered sequential read throughput.
    #[arg(long)]
    readahead_kb: Option<u32>,
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
    #[error("resolving the mountpoint {path}: {source}")]
    Mountpoint {
        path: String,
        source: std::io::Error,
    },
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

    // Resolved now, while the mountpoint is still a plain directory.
    // Canonicalizing after `spawn_mount` would `lstat` the FUSE root — a
    // `GETATTR` round trip back into this very process, blocking this thread
    // before it reaches `wait_for_shutdown` if the server wedges in that
    // window. mountinfo prints mount points post-resolution, so this is also
    // the exact path the readahead lookup below has to match.
    let mountpoint = cli
        .mountpoint
        .canonicalize()
        .map_err(|source| StartupError::Mountpoint {
            path: cli.mountpoint.display().to_string(),
            source,
        })?;

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
    // Cloned rather than moved: the exit path still needs the connection after
    // the mount has let go of it, to force the sync below.
    let fs = LbfsFuse::new(
        Arc::clone(&conn),
        rt.handle().clone(),
        ttl,
        entry_ttl,
        writeback,
    );
    let session =
        fuser::spawn_mount(fs, &mountpoint, &cfg).map_err(|source| StartupError::Mount {
            path: mountpoint.display().to_string(),
            source,
        })?;
    tracing::info!(
        mountpoint = %mountpoint.display(),
        %addr,
        remote = %cli.remote_path.display(),
        "mounted"
    );

    // After the mount, because that is when the bdi exists; best effort,
    // because the knob is root-owned and throughput is not correctness. The
    // kernel clamps the `INIT` readahead to this sysfs value, so without the
    // write a buffered sequential read runs at about half speed
    // (docs/benchmarks/2026-08-28-readahead.md). The path was resolved before
    // the mount went up, so this makes no FUSE round trip into the client.
    if let Some(kb) = readahead::effective_readahead_kb(cli.readahead_kb, limits.max_io_size) {
        readahead::apply(&mountpoint, kb);
    }

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

    // Now, and not a step earlier: the unmount above has pushed every dirty
    // page across as an ordinary `WRITE`, so the whole of this mount's data
    // sits in the server's page cache and nothing later in this shutdown will
    // flush it. Under `fsync = "ignore"` that is precisely the data a crash
    // would lose, and this is the driver-initiated half of the forced-sync
    // control (spec §11).
    force_sync_on_exit(&rt, &conn);

    match ending {
        Ending::Signalled => Ok(()),
        Ending::SessionEnded => Err(StartupError::SessionEnded),
    }
}

/// How long the exit waits for its forced sync before giving up on it.
///
/// Bounded, because the mount is already gone and the only thing this call can
/// still cost is the process's exit: a server wedged mid-`syncfs` must not turn
/// "unmount, drain, exit" into "unmount, drain, hang". Generous, because a very
/// dirty export takes real time to flush and giving up early wastes the sync
/// rather than shortening it.
const EXIT_SYNC_TIMEOUT: Duration = Duration::from_secs(60);

/// Make the export durable on the way out, whatever the server's policy.
///
/// Called after the unmount and before the connection closes, which is the only
/// window where both halves hold: the data has all arrived, and the socket still
/// works. Failure is reported and never raised — an exit that refused to happen
/// because a sync did not would be a worse bargain than an unsynced export, and
/// the operator can read the reason in the log either way.
///
/// `EOPNOTSUPP` has one specific meaning here: the server is older than the
/// control and ignored the flag, so its durability policy still decides. That
/// deserves the loudest of the three lines, because it is the case where an
/// operator believes they have a guarantee they do not have.
fn force_sync_on_exit(rt: &tokio::runtime::Runtime, conn: &Arc<Connection>) {
    let conn = Arc::clone(conn);
    let synced = rt.block_on(async move {
        tokio::time::timeout(EXIT_SYNC_TIMEOUT, conn.force_sync_export()).await
    });
    match synced {
        Ok(Ok(())) => tracing::info!("forced a sync of the export before exit"),
        Ok(Err(e)) if e.0 == libc::EOPNOTSUPP as u16 => tracing::warn!(
            "this server does not implement the forced-sync control; its \
             durability policy decided, and the export may hold unsynced writes"
        ),
        Ok(Err(e)) => tracing::warn!(errno = e.0, "the export was not synced on the way out"),
        Err(_) => tracing::warn!(
            timeout = ?EXIT_SYNC_TIMEOUT,
            "the forced sync did not finish; exiting anyway"
        ),
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
        assert_eq!(cli.readahead_kb, None);
    }

    /// Absent means "derive from the negotiated `max_io_size`", which only the
    /// handshake can resolve, so the flag parses to an `Option` and the
    /// derivation lives in [`lbfs_client::readahead::effective_readahead_kb`].
    /// Zero parses as zero — the disable case — rather than being refused.
    #[test]
    fn the_readahead_flag_parses() {
        let explicit = Cli::parse_from([
            "lbfs-client",
            "--readahead-kb",
            "2048",
            "10.0.0.2:7000",
            "/srv/exports/a",
            "/mnt/lbfs",
        ]);
        assert_eq!(explicit.readahead_kb, Some(2048));

        let disabled = Cli::parse_from([
            "lbfs-client",
            "--readahead-kb",
            "0",
            "10.0.0.2:7000",
            "/srv/exports/a",
            "/mnt/lbfs",
        ]);
        assert_eq!(disabled.readahead_kb, Some(0));
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

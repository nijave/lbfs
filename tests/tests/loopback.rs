//! Spec §10 layer 3: a real FUSE mount, over a real socket, driven by
//! `std::fs`.
//!
//! Everything below this file tests one side of the wire. `mux.rs` scripts a
//! server the client cannot argue with; `live.rs` puts the real server behind
//! the real multiplexer but stops at the `Connection` API; `protocol.rs` speaks
//! frames. None of them can answer the question this file exists for: does the
//! kernel agree? A `READDIR` cursor that round-trips through `postcard` may
//! still be the wrong number for `getdents64`; an attribute that decodes
//! cleanly may still be one `fuse_invalid_attr` rejects; a lookup count the
//! protocol balances on paper may still leak a descriptor per listing. The only
//! way to find out is to mount it and call `open(2)`.
//!
//! ```text
//!   test thread ──▶ std::fs on <tmp>/mnt
//!                      │ /dev/fuse
//!                      ▼
//!            fuser session thread ──▶ LbfsFuse ──▶ client runtime
//!                                                      │ 127.0.0.1:0
//!                                                      ▼
//!                                              server runtime ──▶ <tmp>/export
//! ```
//!
//! # Why every test is a plain `#[test]`
//!
//! The body of a test blocks: `std::fs::write` on the mountpoint does not
//! return until the FUSE round trip has completed, which needs the client
//! runtime to make progress. Running that body *on* the client runtime — which
//! is what `#[tokio::test]` would do — parks a worker thread on work only that
//! runtime can finish, and on the single-threaded runtime `#[tokio::test]`
//! builds by default it is an immediate deadlock. So the runtimes are built by
//! hand and the test thread stays outside both of them, exactly as `main.rs`
//! arranges it for the real binary.
//!
//! # Why the server gets a runtime of its own
//!
//! One test has to make the server *die* while the mount stays up. In process,
//! the honest way to do that is to shut down every task and descriptor it owns
//! at once, and a runtime is the only handle that covers the accept loop, the
//! sessions it spawned, and both halves of every socket. Aborting the accept
//! task alone would leave the sessions running.
//!
//! # Not skipped when `/dev/fuse` is missing
//!
//! These cases are `#[ignore]`d, so they run only when something asks for them
//! by name — `make test-loopback`, which is the whole point of the target. A
//! run that asked for the mount suite and silently did nothing is the failure
//! mode worth avoiding, so a host without the device fails and says which
//! requirement it is missing.

#![deny(unsafe_code)]

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use lbfs_client::conn::{Connection, Proposal};
use lbfs_client::fuse::{session_config, LbfsFuse, CONTROL_XATTR_SYNC};
use lbfs_client::session::Session;
use lbfs_proto::frame::{DEFAULT_MAX_INFLIGHT, DEFAULT_MAX_IO_SIZE};
use lbfs_server::config::{Allowlist, Config, FsyncPolicy};
use rustix::fs::{StatVfsMountFlags, XattrFlags};
use tokio::runtime::Runtime;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// How long a mount has to start answering before the test gives up.
///
/// Generous because it covers the TCP handshake, `HELLO`, `ATTACH`, the FUSE
/// mount syscall and `INIT`; short enough that a mount that will never come up
/// fails the suite rather than hanging it.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// How long an asynchronous consequence — a batched `FORGET` reaching the
/// server, a closed socket tearing down a session — has to land.
///
/// The client's forget batcher holds a partial batch for 500 ms, so anything
/// waiting on a `FORGET` must allow at least that; the rest is slack for a
/// loaded machine.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the FUSE session thread has to notice its mount is gone.
///
/// Bounded rather than joined outright because the unmount is lazy: a
/// descriptor still open on the mount keeps the connection alive and the
/// session thread parked on `/dev/fuse` indefinitely. A test that leaks one
/// should fail with that sentence in front of it, not hang the suite.
const UNMOUNT_TIMEOUT: Duration = Duration::from_secs(30);

/// How often to look again while inside one of the bounds above. A poll
/// interval, never a substitute for a condition: nothing here sleeps for a
/// fixed time and then asserts.
const POLL: Duration = Duration::from_millis(10);

/// How long a severed mount may spend re-attaching before it gives up.
///
/// Ten seconds, which is what `--reconnect-timeout` defaults to and what the
/// drills are timed against. It sits under the three-quarters clamp
/// `Session::new` applies to the server's 60-second grace, so the number the
/// cases below reason about is this one.
const RECONNECT_DEADLINE: Duration = Duration::from_secs(10);

/// How long to give the client to notice that its socket died, before issuing
/// the call that has to park.
///
/// A closed socket is news that travels: the reader task has to wake, read the
/// end of the stream and kill the connection before the session can know
/// anything is wrong. A call issued inside that window rides a connection the
/// client still believes in and fails `EIO` like anything else in flight at the
/// break — which is correct, and not what the severed-connection cases are
/// about. `crates/lbfs-client/tests/mux.rs` pauses for the same reason and for
/// the same length; both are orders of magnitude longer than the wake-up takes.
const NOTICED: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Host requirements
// ---------------------------------------------------------------------------

/// Fail, loudly and by name, on a host that cannot mount.
fn require_fuse() {
    assert!(
        Path::new("/dev/fuse").exists(),
        "the loopback suite mounts a real filesystem and this host has no \
         /dev/fuse. Load the `fuse` module (`modprobe fuse`), or run the suite \
         in the VM (`make vm-test`). It is not skipped, because a `make \
         test-loopback` that quietly proved nothing is worse than a red one."
    );
    assert!(
        which("fusermount3").is_some(),
        "the loopback suite needs `fusermount3` on PATH: fuser's pure-Rust \
         mount path runs it for both the unprivileged mount and the unmount. \
         Install fuse3."
    );
}

fn which(prog: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(prog))
        .find(|candidate| candidate.is_file())
}

/// Fail, by name, on a host whose descriptor limit this case would exhaust.
///
/// The server runs in this process and holds one `O_PATH` descriptor per node
/// the kernel has looked up, so a case that stats every name in a large
/// directory costs a descriptor per name — against a limit the test process
/// shares with cargo's whole test binary. A box left at the traditional 1024
/// soft limit would meet `EMFILE` somewhere in the middle of the listing and
/// report it as a confusing I/O error a long way from the cause, so the check
/// happens up front and names the number to raise it to.
fn require_open_files(needed: u64) {
    let soft = rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .current
        .unwrap_or(u64::MAX);
    assert!(
        soft >= needed,
        "this case registers one server descriptor per directory entry and \
         needs RLIMIT_NOFILE of at least {needed}; this process has a soft \
         limit of {soft}. Raise it (`ulimit -n {needed}`) and run again."
    );
}

// ---------------------------------------------------------------------------
// Waiting
// ---------------------------------------------------------------------------

/// Poll `ready` until it holds, or fail saying what never happened.
///
/// The alternative — sleep, then assert — is the shape that passes on a quiet
/// laptop and fails in CI. Every wait in this file is bounded and every bound
/// names its subject.
fn wait_for(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if ready() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "gave up after {timeout:?} waiting for {what}"
        );
        std::thread::sleep(POLL);
    }
}

/// Whether the kernel currently has a FUSE filesystem mounted at `mnt`.
///
/// `/proc/self/mounts` rather than `/proc/mounts` so the answer is about this
/// process's namespace. The fstype is `fuse` (the client sets `fsname`, which
/// names the *source*, not a subtype), but `fuse.` prefixes are accepted too so
/// this keeps working if the mount ever grows one.
fn is_fuse_mount(mnt: &Path) -> bool {
    let Ok(table) = std::fs::read_to_string("/proc/self/mounts") else {
        return false;
    };
    table.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let (Some(_source), Some(point), Some(kind)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return false;
        };
        Path::new(point) == mnt && kind.starts_with("fuse")
    })
}

/// The last resort for a mount `fuser` did not take down.
///
/// A leaked mount is not one failed test, it is every later run of the suite:
/// the tempdir underneath it can never be cleaned up and the next mount at that
/// path is somebody else's problem. `-z` detaches whatever the state of the
/// session, which is exactly what is wanted when the ordinary path has already
/// failed.
fn force_unmount(mnt: &Path) {
    for args in [&["-u"][..], &["-u", "-z"][..]] {
        if !is_fuse_mount(mnt) {
            return;
        }
        let _ = std::process::Command::new("fusermount3")
            .args(args)
            .arg(mnt)
            .status();
    }
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// What a test wants to vary about the stack under it.
struct Opts {
    /// The `HELLO` flag and the kernel capability together: the server reads an
    /// `OPEN`'s flags differently depending on it, so it is never just a client
    /// tuning knob.
    writeback: bool,
    fsync: FsyncPolicy,
    /// Attribute lifetime. Zero makes every `stat` a round trip, which is
    /// what a test about a *dead* server needs — with the default second, the
    /// kernel would answer from cache and prove nothing.
    ttl: Duration,
    /// Name lifetime. Defaults to `ttl`, which is what the shipped client does
    /// when `--entry-timeout` is absent.
    entry_ttl: Duration,
    /// Put a [`Breaker`] between the client and the server, and point the
    /// client at it. Every other mount in this file dials the server straight,
    /// where the only way to take the socket away is to take the server with
    /// it.
    breaker: bool,
    /// Ask the server to hold this mount's session across a disconnection, and
    /// give the client [`RECONNECT_DEADLINE`] to re-attach inside.
    ///
    /// Off by default because the library is off by default (design §8.2).
    /// Every case that does not name it keeps today's teardown semantics
    /// exactly — no ticket in the `ATTACH` reply, nothing retained past a
    /// socket, no parked call — which is what makes the fd-census cases and
    /// `a_dead_server_leaves_an_eio_mount_that_still_unmounts` mean after this
    /// feature what they meant before it.
    resume: bool,
}

impl Default for Opts {
    /// What the shipped client does by default (spec §7), except for
    /// resumption: the *library* default is off, and only the cases that sever
    /// a connection ask for it.
    fn default() -> Opts {
        Opts {
            writeback: true,
            fsync: FsyncPolicy::Honor,
            ttl: Duration::from_secs(1),
            entry_ttl: Duration::from_secs(1),
            breaker: false,
            resume: false,
        }
    }
}

// ---------------------------------------------------------------------------
// A severable connection
// ---------------------------------------------------------------------------

/// A forwarding proxy between the client and the server, with a way to cut it.
///
/// The harness starts its server in this process and the client dials it
/// straight, so nothing here could take the socket away without taking the
/// server down with it — and a mount surviving a transport failure to a server
/// that is *still running* is the whole of what session resumption promises.
/// This listens on a port of its own, dials the real server for every
/// connection it accepts, and copies both directions. One task owns both
/// sockets of a link, so [`Breaker::sever`] drops the halves by aborting it:
/// each peer sees its connection end, while the listener goes on accepting,
/// which is what the client's redial needs.
///
/// **A test double for a flaky network, not a proxy anybody ships.** No
/// backpressure story worth the name, no shutdown handling beyond what
/// `copy_bidirectional` does for it, and no reason to exist outside this file:
/// `ss -K` is the real tool and needs two machines, which is `vm/tests/`.
struct Breaker {
    /// Where the client dials. The server's own address never reaches it.
    addr: SocketAddr,
    /// One entry per link the proxy has built and not yet cut, each owning both
    /// of that link's sockets.
    links: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Breaker {
    /// Start forwarding to `upstream`, on the runtime that serves it.
    ///
    /// The server's runtime rather than a third one: the proxy stands in for
    /// the wire, and a case that takes the server away
    /// ([`ServerSide::kill`]) means the wire to go with it.
    fn start(rt: &Runtime, upstream: SocketAddr) -> Breaker {
        let listener = rt
            .block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await })
            .expect("the proxy binds a loopback port of its own");
        let addr = listener.local_addr().unwrap();
        let links: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::clone(&links);
        rt.spawn(async move {
            loop {
                let Ok((mut down, _peer)) = listener.accept().await else {
                    return;
                };
                let Ok(mut up) = tokio::net::TcpStream::connect(upstream).await else {
                    // Nothing to forward to — the server is gone. Dropping the
                    // accepted socket ends the client's dial the way a refused
                    // one would, and its supervisor comes back.
                    continue;
                };
                // What both real ends set on their own sockets. A proxy that
                // let Nagle hold a reply would add latency this suite would
                // then have to explain.
                let _ = down.set_nodelay(true);
                let _ = up.set_nodelay(true);
                let link = tokio::spawn(async move {
                    let _ = tokio::io::copy_bidirectional(&mut down, &mut up).await;
                });
                accepted.lock().unwrap().push(link);
            }
        });
        Breaker { addr, links }
    }

    /// Cut every link running through the proxy.
    ///
    /// Aborting a link drops the two sockets it owns, which is what a reset in
    /// the middle looks like from either end: the client's connection dies and
    /// the server's session task tears down and hands its session to the
    /// registry. The listener is deliberately untouched — the supervisor
    /// redials this same address, and the next accept builds a fresh link to
    /// the same, still-running server.
    fn sever(&self) {
        let links = std::mem::take(&mut *self.links.lock().unwrap());
        assert!(
            !links.is_empty(),
            "sever() with nothing to sever: no connection has come through the \
             proxy, so this case is not testing what it means to test"
        );
        for link in links {
            link.abort();
        }
    }
}

/// The server half: a runtime, and the address it ended up on.
struct ServerSide {
    rt: Option<Runtime>,
    addr: SocketAddr,
}

impl ServerSide {
    fn start(export: &Path, opts: &Opts) -> ServerSide {
        let rt = runtime("lbfs-server");
        let cfg = Config {
            listen: "127.0.0.1:0".to_string(),
            // The resolved path: the server matches its allowlist against what
            // the kernel reports for the descriptor it opened, so a pattern
            // built from an unresolved path is denied wherever `/tmp` is a
            // symlink.
            allowed_paths: vec![export.to_str().unwrap().to_string()],
            max_inflight: DEFAULT_MAX_INFLIGHT,
            max_io_size: DEFAULT_MAX_IO_SIZE,
            fsync: opts.fsync,
            resume_grace: Duration::from_secs(60),
            max_resumable_sessions: 64,
        };
        let allow = Allowlist::new(&cfg.allowed_paths).unwrap();
        // Port 0, so the suite never collides with a server on the developer's
        // machine nor with a sibling test.
        let listener = rt
            .block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await })
            .unwrap();
        let addr = listener.local_addr().unwrap();
        rt.spawn(async move {
            let _ = lbfs_server::rpc::serve(listener, Arc::new(cfg), Arc::new(allow)).await;
        });
        ServerSide { rt: Some(rt), addr }
    }

    /// The runtime the server is serving on, for anything that belongs to its
    /// side of the wire.
    fn rt(&self) -> &Runtime {
        self.rt.as_ref().expect("the server is still running")
    }

    /// Take the server away without touching the mount.
    ///
    /// Shutting the runtime down drops every task it owns, and with them both
    /// halves of every accepted socket — which is what the client sees as the
    /// peer vanishing. The timeout covers the `spawn_blocking` calls the
    /// backend makes; past it the runtime leaks its blocking threads rather
    /// than hanging the test.
    fn kill(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

/// A mounted lbfs, and everything underneath it.
///
/// Field order is the teardown order: the session unmounts first, then the
/// connection closes, then the runtimes stop, and only then is the tempdir
/// removed. Reversing any pair of those either fails the last writes with
/// `EIO`, or — the one that would be silently destructive — walks
/// `remove_dir_all` through a mountpoint that is still live and deletes the
/// export through it. See [`Loopback::drop`].
struct Loopback {
    session: Option<fuser::BackgroundSession>,
    /// The client's own session, the object the mount holds and the one whose
    /// teardown sends `DETACH`. Dropped after the FUSE session and before the
    /// connection below, because it holds the *current* connection — which is
    /// a different one from `conn` on any mount that has been severed.
    lbfs_session: Option<Arc<Session>>,
    /// The connection this mount started on, and after a sever a dead one: the
    /// session swapped a new one in underneath the bridge, and a `Connection`
    /// that has died is never revived.
    conn: Option<Arc<Connection>>,
    /// The proxy in the path, when the case asked for one.
    breaker: Option<Breaker>,
    /// The bridge's callbacks spawn onto this runtime's handle, so it has to
    /// outlive the session and the connection whose reader and writer tasks
    /// live on it. A case that wants to drive the connection directly, rather
    /// than through the mount, blocks on it — see [`Loopback::on_client_rt`].
    client_rt: Runtime,
    server: ServerSide,
    root: Option<tempfile::TempDir>,
    export: PathBuf,
    mnt: PathBuf,
}

impl Loopback {
    fn start(opts: Opts) -> Loopback {
        require_fuse();
        let root = tempfile::tempdir().unwrap();
        let export = root.path().join("export");
        let mnt = root.path().join("mnt");
        std::fs::create_dir(&export).unwrap();
        std::fs::create_dir(&mnt).unwrap();
        // Both resolved: the allowlist is matched against a resolved path, and
        // the descriptor census below compares resolved link targets.
        let export = export.canonicalize().unwrap();
        let mnt = mnt.canonicalize().unwrap();

        let server = ServerSide::start(&export, &opts);
        // The proxy stands where the wire would be, so the address the client
        // dials is its own rather than the server's — and every redial goes
        // back to the same place, which is what makes a sever survivable.
        let breaker = opts
            .breaker
            .then(|| Breaker::start(server.rt(), server.addr));
        let addr = breaker.as_ref().map_or(server.addr, |b| b.addr);
        let client_rt = runtime("lbfs-client");
        // The same proposal the binary builds, and named here for the same
        // reason: the session keeps it, so a redial asks for what this
        // handshake asked for.
        let proposal = Proposal {
            writeback: opts.writeback,
            resume: opts.resume,
            ..Proposal::default()
        };
        let (conn, limits, _root_attr) = client_rt
            .block_on(Connection::connect_with(
                addr,
                export.as_os_str().as_bytes(),
                proposal,
            ))
            .expect("the client attaches to the export this test just exported");
        // Whatever the `ATTACH` reply carried: `Some` for a case that asked to
        // resume against a server that retains, `None` everywhere else, and a
        // session with no ticket never redials whatever deadline it is handed.
        let ticket = conn.ticket;
        let deadline = if opts.resume {
            RECONNECT_DEADLINE
        } else {
            Duration::ZERO
        };
        // Built inside the runtime, exactly as `main.rs` builds it and for the
        // reason its doc comment gives: a session that means to come back
        // spawns the reconnect supervisor, which needs a runtime context its
        // plain signature does not advertise.
        //
        // The mount holds this session; the harness keeps a clone, because its
        // teardown is what sends `DETACH`. It keeps a clone of the first
        // connection too, because the cases that reach past the mount — the fd
        // census, the forced-sync acknowledgement — speak to the socket.
        let lbfs_session = client_rt.block_on(async {
            Session::new(
                Arc::clone(&conn),
                addr,
                export.as_os_str().as_bytes().to_vec(),
                proposal,
                ticket,
                deadline,
            )
        });

        let fs = LbfsFuse::new(
            Arc::clone(&lbfs_session),
            client_rt.handle().clone(),
            opts.ttl,
            opts.entry_ttl,
            opts.writeback,
        );
        // The same option list the binary builds, from the same negotiated
        // ceiling: `max_read` has to agree with what the multiplexer will
        // accept or the kernel issues reads that come back `EINVAL`.
        let session = fuser::spawn_mount(
            fs,
            &mnt,
            &session_config(limits.max_io_size, false, false, None, false),
        )
        .expect("the mount succeeds");

        let mounted = Loopback {
            session: Some(session),
            lbfs_session: Some(lbfs_session),
            conn: Some(conn),
            breaker,
            client_rt,
            server,
            root: Some(root),
            export,
            mnt,
        };
        mounted.wait_ready();
        mounted
    }

    /// Wait until the mount answers, or say why it never will.
    ///
    /// `spawn_mount` returns once the mount syscall is done, but `INIT` runs
    /// afterwards on the session thread — and `init` is allowed to refuse,
    /// which ends the session and leaves an `ENOTCONN` mountpoint behind a
    /// perfectly successful `spawn_mount`. Watching the session thread turns
    /// that into a named failure instead of a twenty-second timeout.
    fn wait_ready(&self) {
        let session = self.session.as_ref().expect("just mounted");
        wait_for("the mount to answer a readdir", READY_TIMEOUT, || {
            assert!(
                !session.guard.is_finished(),
                "the FUSE session ended before the mount answered; the client's \
                 `init` refused the kernel's offer (run with RUST_LOG=debug)"
            );
            is_fuse_mount(&self.mnt) && std::fs::read_dir(&self.mnt).is_ok()
        });
    }

    fn mnt(&self) -> &Path {
        &self.mnt
    }

    fn export(&self) -> &Path {
        &self.export
    }

    fn conn(&self) -> &Arc<Connection> {
        self.conn.as_ref().expect("the connection is still held")
    }

    /// The proxy in the path, for a case that means to cut it.
    fn breaker(&self) -> &Breaker {
        self.breaker
            .as_ref()
            .expect("this case has to ask for `breaker: true` in its `Opts`")
    }

    /// Run one of the connection's own futures to completion.
    ///
    /// The escape hatch for a case whose subject is a call the mount cannot
    /// make — the forced-sync control's acknowledgement being the one that
    /// matters, since userspace sees only `setxattr`'s zero and never the reply
    /// flag underneath it. Blocking is safe from here: the test thread is not a
    /// worker of this runtime.
    fn on_client_rt<F: std::future::Future>(&self, f: F) -> F::Output {
        self.client_rt.block_on(f)
    }

    /// Unmount and wait for the session thread to finish.
    ///
    /// Joining is the point. Dropping the session alone unmounts and returns;
    /// the kernel's `FORGET`s for every evicted inode, the writeback of every
    /// dirty page and the final `DESTROY` all still have to cross the socket,
    /// and the thread that serves them is the one being joined. Anything
    /// asserting about what the mount left behind has to happen after this
    /// returns.
    ///
    /// **Every file opened on the mount must be closed first.** `fusermount3`
    /// unmounts with `MNT_DETACH`, which takes the mountpoint out of the mount
    /// table at once but leaves the superblock — and with it the FUSE
    /// connection — alive until the last reference goes. One `File` still in
    /// scope therefore leaves the session thread blocked on `/dev/fuse`
    /// forever, which is why the join is bounded rather than trusted.
    fn unmount(&mut self) {
        assert!(
            self.try_unmount(),
            "the FUSE session at {} did not end within {UNMOUNT_TIMEOUT:?} of \
             the unmount. Almost always a file left open on the mount: the \
             unmount is lazy, so the connection outlives the mountpoint until \
             the last descriptor into it is closed.",
            self.mnt.display()
        );
        assert!(
            !is_fuse_mount(&self.mnt),
            "{} is still mounted after the session ended",
            self.mnt.display()
        );
    }

    /// The same, reporting rather than asserting, so [`Loopback::drop`] can use
    /// it. A panic raised while unwinding aborts the whole test binary and
    /// takes every other case's diagnostics with it, and
    /// `BackgroundSession::umount_and_join` returns an `io::Result` that a
    /// failed unmount fills in.
    fn try_unmount(&mut self) -> bool {
        let Some(session) = self.session.take() else {
            return !is_fuse_mount(&self.mnt);
        };
        // On a helper thread, because the join is the part that can hang and
        // this must be able to give up on it. The helper keeps the session
        // alive after a timeout, so a mount that comes down late still comes
        // down rather than being left behind.
        let (done, ended) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome =
                std::panic::catch_unwind(AssertUnwindSafe(move || session.umount_and_join()));
            let _ = done.send(matches!(outcome, Ok(Ok(()))));
        });
        let unmounted = match ended.recv_timeout(UNMOUNT_TIMEOUT) {
            Ok(true) => true,
            Ok(false) => {
                eprintln!("lbfs loopback: the FUSE session ended badly; forcing the unmount");
                force_unmount(&self.mnt);
                !is_fuse_mount(&self.mnt)
            }
            Err(_) => false,
        };
        // The binary's exit sequence, in the binary's order: after the drain —
        // which flushes writeback and the `FORGET`s the kernel emits for every
        // evicted inode, both needing the session — first the exit sync, then
        // `shutdown()`. The sync goes over `live()` exactly as `main.rs` sends
        // it: a session still redialling, or dead, skips it rather than parking
        // the teardown behind a reconnect deadline. `shutdown()` then sends
        // `DETACH` on a mount that asked to resume, which is what hands the
        // server's descriptors back now rather than at the end of the grace; on
        // one that did not it marks the session dead and nothing else — no
        // ticket, nothing to detach, and no supervisor to stop.
        if let Some(lbfs) = &self.lbfs_session {
            if let Some(conn) = lbfs.live() {
                // Best effort, like the binary's: a failed sync is the export's
                // problem to report, never the teardown's to hang on.
                let _ = self.client_rt.block_on(async {
                    tokio::time::timeout(UNMOUNT_TIMEOUT, conn.force_sync_export()).await
                });
            }
            self.client_rt.block_on(lbfs.shutdown());
        }
        unmounted
    }

    /// Drop this side's references to the session and the connection, closing
    /// the socket.
    ///
    /// Separate from [`Loopback::unmount`] because the interesting assertions
    /// live between the two: after the unmount the mount's own `Arc` is gone
    /// but the socket is still open, which is the only moment at which the
    /// server's answer to "did every `FORGET` land?" is still observable.
    ///
    /// Both references, because after a reconnect they are two different
    /// connections and the live one is the session's.
    fn disconnect(&mut self) {
        self.lbfs_session = None;
        self.conn = None;
    }

    /// Descriptors this process holds onto anything inside the export.
    ///
    /// The server is in this process, so its `O_PATH` node descriptors are
    /// visible in `/proc/self/fd` — and scoping the census to one tempdir is
    /// what makes it a number rather than noise. Nothing the test itself opens
    /// counts: the test works through the mountpoint, which is a different
    /// path.
    fn export_fds(&self) -> usize {
        std::fs::read_dir("/proc/self/fd")
            .expect("/proc is mounted")
            .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
            .filter(|target| target.starts_with(&self.export))
            .count()
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        if !self.try_unmount() {
            eprintln!(
                "lbfs loopback: the FUSE session at {} outlived its unmount",
                self.mnt.display()
            );
        }
        force_unmount(&self.mnt);
        if is_fuse_mount(&self.mnt) {
            // `TempDir`'s own cleanup is `remove_dir_all`, and the mountpoint
            // is inside it. Running that over a mount that is still live would
            // not fail — it would recurse through the mount and delete the
            // export. Leaking a tempdir is the cheaper mistake by a wide
            // margin, so the directory is deliberately not removed.
            eprintln!(
                "lbfs loopback: {} is STILL MOUNTED after every unmount attempt; \
                 leaking {} rather than deleting the export through it. \
                 Unmount it by hand before running the suite again.",
                self.mnt.display(),
                self.root.as_ref().map_or_else(
                    || "the tempdir".to_string(),
                    |d| d.path().display().to_string()
                ),
            );
            std::mem::forget(self.root.take());
        }
    }
}

/// A multi-threaded runtime, because the whole point of the bridge is that the
/// requests it spawns overlap.
fn runtime(name: &str) -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name(name)
        .build()
        .expect("a tokio runtime starts")
}

// ---------------------------------------------------------------------------
// Small helpers over the mount
// ---------------------------------------------------------------------------

/// The errno an operation failed with, or `None` if it succeeded.
fn errno_of(r: std::io::Result<impl Sized>) -> Option<i32> {
    r.err().and_then(|e| e.raw_os_error())
}

fn xattr_value(path: &Path, name: &str) -> rustix::io::Result<Vec<u8>> {
    let mut buf = [0u8; 1024];
    let len = rustix::fs::getxattr(path, name, &mut buf[..])?;
    Ok(buf[..len].to_vec())
}

fn xattr_names(path: &Path) -> BTreeSet<String> {
    let mut buf = [0u8; 4096];
    let len = rustix::fs::listxattr(path, &mut buf[..]).unwrap();
    buf[..len]
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect()
}

/// The `user.` namespace only.
///
/// Everything else in a listing belongs to the host rather than the test: on a
/// machine with SELinux enforcing, every file carries a `security.selinux`
/// label, and it travels through the mount exactly as it should. Asserting on
/// the whole set would make the suite pass or fail on whether an LSM is loaded.
fn user_xattr_names(path: &Path) -> BTreeSet<String> {
    xattr_names(path)
        .into_iter()
        .filter(|name| name.starts_with("user."))
        .collect()
}

fn names_in(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Create, read, append, overwrite, truncate-on-open — checked both through the
/// mount and against the export, because a mount that only agrees with itself
/// would pass every assertion in the first half of this.
fn file_content_round_trips(writeback: bool) {
    let mut lb = Loopback::start(Opts {
        writeback,
        ..Opts::default()
    });
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();

    std::fs::write(mnt.join("hello.txt"), "hello lbfs").unwrap();
    assert_eq!(
        std::fs::read_to_string(mnt.join("hello.txt")).unwrap(),
        "hello lbfs"
    );
    assert_eq!(
        std::fs::read_to_string(export.join("hello.txt")).unwrap(),
        "hello lbfs",
        "closing a file flushes it all the way to the server"
    );

    // Append. Under the writeback cache the server has been told to strip
    // `O_APPEND` and the kernel computes the offset itself; without it the
    // server keeps `O_APPEND` and the kernel does not. The observable result
    // has to be the same either way, which is why this runs in both modes.
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(mnt.join("hello.txt"))
            .unwrap();
        f.write_all(b" and again").unwrap();
        f.sync_all().unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(mnt.join("hello.txt")).unwrap(),
        "hello lbfs and again"
    );
    assert_eq!(
        std::fs::read_to_string(export.join("hello.txt")).unwrap(),
        "hello lbfs and again"
    );

    // A write at an offset in the middle, which is the ordinary `WRITE` path
    // with a non-zero offset rather than an append.
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(mnt.join("hello.txt"))
            .unwrap();
        f.seek(SeekFrom::Start(6)).unwrap();
        f.write_all(b"LBFS").unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(mnt.join("hello.txt")).unwrap(),
        "hello LBFS and again"
    );

    // `O_TRUNC` on open. The server drops the flag from `OPEN` on purpose and
    // waits for the `SETATTR` the kernel sends instead, which only arrives
    // because the client never asks for `FUSE_ATOMIC_O_TRUNC`. A regression
    // there leaves the old bytes in place and this is what catches it.
    std::fs::write(mnt.join("hello.txt"), "short").unwrap();
    assert_eq!(
        std::fs::read_to_string(mnt.join("hello.txt")).unwrap(),
        "short"
    );
    assert_eq!(std::fs::metadata(mnt.join("hello.txt")).unwrap().len(), 5);
    assert_eq!(
        std::fs::read_to_string(export.join("hello.txt")).unwrap(),
        "short"
    );

    std::fs::remove_file(mnt.join("hello.txt")).unwrap();
    assert_eq!(std::fs::read_dir(&export).unwrap().count(), 0);
    lb.unmount();
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn file_content_round_trips_with_the_writeback_cache() {
    file_content_round_trips(true);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn file_content_round_trips_without_the_writeback_cache() {
    file_content_round_trips(false);
}

/// Two threads, two `O_DIRECT` descriptors, one file, one moment.
///
/// This is the shape the mount used to serialise. With only `FOPEN_KEEP_CACHE`
/// on the reply, every `O_DIRECT` write went through `fuse_cache_write_iter`,
/// which holds `inode_lock` from `fs/fuse/file.c:1494` to `file.c:1525` —
/// across the whole round trip, because a `pwrite` is a synchronous iocb — and
/// four threads on one file measured 0.98 × one thread. With
/// `FOPEN_DIRECT_IO | FOPEN_PARALLEL_DIRECT_WRITES` the same writes take
/// `inode_lock_shared` (`file.c:1432-1450`) and overlap.
///
/// **A loopback mount cannot prove they overlapped.** One host, one runtime,
/// and no honest timing floor to compare against. What it proves is that
/// nothing was lost, torn or misplaced once the kernel let them run together,
/// which is the failure this change could actually introduce. The parallelism
/// itself is a VM measurement; see the plan's acceptance section.
///
/// Three shapes ride along, because each reaches a different branch of
/// `fuse_dio_wr_exclusive_lock`:
///
/// * the file gets its size through an `O_DIRECT` `CREATE`, so the create path
///   answers with the same reply the open path does (`fs/fuse/dir.c:887`);
/// * the concurrent pair writes inside that size, which is the only case the
///   kernel runs shared (`file.c:1419-1421`);
/// * a second pair writes past the end, which the kernel keeps exclusive, and
///   which must still land both blocks.
fn two_direct_writers_on_one_file_both_land(writeback: bool) {
    const BLOCK: usize = 64 * 1024;

    let mut lb = Loopback::start(Opts {
        writeback,
        ..Opts::default()
    });
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();
    let path = mnt.join("shared.dat");

    // Created and sized through an `O_DIRECT` descriptor, so this half
    // exercises the `CREATE` reply rather than the `OPEN` reply. Zeros rather
    // than `set_len`, so the concurrent writes below land on real blocks.
    {
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(&path)
            .unwrap();
        f.write_all(&vec![0u8; 2 * BLOCK]).unwrap();
    }
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        2 * BLOCK as u64,
        "the O_DIRECT create did not reach its full size"
    );

    // Inside the end of file: the shared-lock case.
    std::thread::scope(|s| {
        for (mark, offset) in [(b'b', 0u64), (b'c', BLOCK as u64)] {
            let path = path.clone();
            s.spawn(move || {
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(&path)
                    .unwrap();
                f.write_all_at(&vec![mark; BLOCK], offset).unwrap();
            });
        }
    });

    // Past the end of file: the exclusive fallback. Both must still land, and
    // the file must end up exactly twice as long.
    std::thread::scope(|s| {
        for (mark, offset) in [(b'd', 2 * BLOCK as u64), (b'e', 3 * BLOCK as u64)] {
            let path = path.clone();
            s.spawn(move || {
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(&path)
                    .unwrap();
                f.write_all_at(&vec![mark; BLOCK], offset).unwrap();
            });
        }
    });

    // Read the export directly, behind the mount's back: a mount that only
    // agrees with itself would pass every assertion made through it.
    let landed = std::fs::read(export.join("shared.dat")).unwrap();
    assert_eq!(landed.len(), 4 * BLOCK, "the export has the wrong length");
    for (i, mark) in (*b"bcde").into_iter().enumerate() {
        let block = &landed[i * BLOCK..(i + 1) * BLOCK];
        assert!(
            block.iter().all(|&b| b == mark),
            "block {i} is not a solid run of {:?}; first wrong byte at {:?}",
            mark as char,
            block.iter().position(|&b| b != mark)
        );
    }

    lb.unmount();
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn two_direct_writers_on_one_file_both_land_with_the_writeback_cache() {
    two_direct_writers_on_one_file_both_land(true);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn two_direct_writers_on_one_file_both_land_without_the_writeback_cache() {
    two_direct_writers_on_one_file_both_land(false);
}

/// Two appenders, one file, both `O_DIRECT`, both mount shapes.
///
/// Append is the one write shape this change deliberately leaves alone, and
/// the reason is a single line: `fuse_dio_wr_exclusive_lock` returns true for
/// `IOCB_APPEND` before it looks at anything else
/// (`fs/fuse/file.c:1412-1413`), because an append has to know the eventual end
/// of the file. So two appenders serialise, and this test says so by insisting
/// that each block arrives whole.
///
/// Both mount shapes, because the server reads `O_APPEND` differently in each
/// and only one of them can be wrong at a time:
///
/// * **writeback on** — the server strips `O_APPEND` from its own descriptor,
///   and the client's kernel picks the offset itself through
///   `generic_write_checks` (`file.c:1792` on the direct path, `file.c:1496` on
///   the cached one) while holding the exclusive lock. Two appends that raced
///   would overwrite one another and the file would come out short.
/// * **writeback off** — the server keeps `O_APPEND`, and the export's own
///   kernel places the bytes at the true end of the file, so a stale client
///   `i_size` costs nothing.
///
/// The direct path also skips the `fuse_update_attributes(STATX_SIZE |
/// STATX_MODE)` that opens `fuse_cache_write_iter` (`file.c:1482-1486`). That
/// is exactly the refresh the writeback cache already answered locally, so the
/// offset arithmetic must come out the same. This test is what says it did.
fn appends_stay_whole_with_direct_io(writeback: bool) {
    const BLOCK: usize = 64 * 1024;

    let mut lb = Loopback::start(Opts {
        writeback,
        ..Opts::default()
    });
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();
    let path = mnt.join("appended.dat");

    std::fs::write(&path, vec![b'a'; BLOCK]).unwrap();

    std::thread::scope(|s| {
        for mark in *b"bc" {
            let path = path.clone();
            s.spawn(move || {
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(&path)
                    .unwrap();
                f.write_all(&vec![mark; BLOCK]).unwrap();
            });
        }
    });

    let landed = std::fs::read(export.join("appended.dat")).unwrap();
    assert_eq!(
        landed.len(),
        3 * BLOCK,
        "two appends of {BLOCK} bytes onto {BLOCK} bytes must give 3 blocks; \
         a short file means the two appends chose the same offset"
    );
    assert!(landed[..BLOCK].iter().all(|&b| b == b'a'));

    // Order is nobody's business — the exclusive lock says one goes first, not
    // which. Wholeness is: neither block may carry a byte of the other.
    let second = &landed[BLOCK..2 * BLOCK];
    let third = &landed[2 * BLOCK..];
    let mut marks = [second[0], third[0]];
    marks.sort_unstable();
    assert_eq!(marks, [b'b', b'c'], "one appender's block never arrived");
    assert!(second.iter().all(|&b| b == second[0]), "block two is torn");
    assert!(third.iter().all(|&b| b == third[0]), "block three is torn");

    // One more append through an ordinary descriptor, to prove the cached path
    // still agrees with the direct one about where the end of the file is.
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"tail").unwrap();
        f.sync_all().unwrap();
    }
    let landed = std::fs::read(export.join("appended.dat")).unwrap();
    assert_eq!(landed.len(), 3 * BLOCK + 4);
    assert_eq!(&landed[3 * BLOCK..], b"tail");

    lb.unmount();
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn appends_stay_whole_with_direct_io_and_the_writeback_cache() {
    appends_stay_whole_with_direct_io(true);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn appends_stay_whole_with_direct_io_without_the_writeback_cache() {
    appends_stay_whole_with_direct_io(false);
}

/// One file, two descriptors, one of them direct — the mixed-access promise of
/// spec §7, checked in both directions.
///
/// Opening the cached descriptor puts the inode into caching mode
/// (`fs/fuse/iomode.c:238`, `iomode.c:63-65`), which sends every direct write
/// back to the exclusive lock (`fs/fuse/file.c:1416-1417`). That costs the
/// parallelism and buys back today's behaviour, so nothing here can regress
/// into a race. What it must not cost is coherence, and coherence comes from
/// the direct path doing its own page work: a flush before the transfer
/// (`file.c:1667-1673`), an invalidate before a write (`file.c:1682-1688`) and
/// another after it (`file.c:1741-1748`).
///
/// So: a direct write must be visible to a cached reader with no flush, and a
/// cached write must be visible to a direct reader with no `fsync`. Neither
/// test calls `sync_all`, on purpose — an explicit flush would prove nothing.
fn cached_and_direct_descriptors_stay_coherent(writeback: bool) {
    const BLOCK: usize = 64 * 1024;

    let mut lb = Loopback::start(Opts {
        writeback,
        ..Opts::default()
    });
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();
    let path = mnt.join("mixed.dat");

    std::fs::write(&path, vec![b'a'; 2 * BLOCK]).unwrap();

    let cached = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let direct = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(&path)
        .unwrap();

    // Direct write, cached read. The direct path invalidated the range, so the
    // cached descriptor has to go back to the server for it.
    direct.write_all_at(&vec![b'b'; BLOCK], 0).unwrap();
    let mut seen = vec![0u8; BLOCK];
    cached.read_exact_at(&mut seen, 0).unwrap();
    assert!(
        seen.iter().all(|&b| b == b'b'),
        "the cached descriptor served stale pages after a direct write"
    );

    // Cached write, direct read, no flush in between. The direct read's own
    // `filemap_write_and_wait_range` is what has to push the dirty page out.
    cached
        .write_all_at(&vec![b'c'; BLOCK], BLOCK as u64)
        .unwrap();
    let mut seen = vec![0u8; BLOCK];
    direct.read_exact_at(&mut seen, BLOCK as u64).unwrap();
    assert!(
        seen.iter().all(|&b| b == b'c'),
        "the direct descriptor read around a dirty page instead of flushing it"
    );

    // Both descriptors closed before the export is inspected: the second block
    // only has to reach the server by the time the cached handle is gone.
    drop(direct);
    drop(cached);

    let landed = std::fs::read(export.join("mixed.dat")).unwrap();
    assert_eq!(landed.len(), 2 * BLOCK);
    assert!(landed[..BLOCK].iter().all(|&b| b == b'b'));
    assert!(landed[BLOCK..].iter().all(|&b| b == b'c'));

    lb.unmount();
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn cached_and_direct_descriptors_stay_coherent_with_the_writeback_cache() {
    cached_and_direct_descriptors_stay_coherent(true);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn cached_and_direct_descriptors_stay_coherent_without_the_writeback_cache() {
    cached_and_direct_descriptors_stay_coherent(false);
}

/// Unmounting is the drain, and the drain is what the shipped binary leans on.
///
/// `crates/lbfs-client/src/main.rs` treats `drop(session)` as "unmount, drain,
/// exit": `umount(2)` syncs the superblock before it detaches, so whatever the
/// client kernel still holds comes back through this bridge as ordinary
/// `WRITE` callbacks, on a session thread that is still running and a
/// connection that is still open. Every other case in this file reads its data
/// back through the mount, which proves the round trip and not the teardown.
/// This one reads only from the export, and only after the unmount has
/// returned, so a teardown that detached early would show up as missing bytes
/// rather than as a passing test.
///
/// It doubles as the guard on the unmount path itself. `Loopback::unmount`
/// gives the session thread a bounded time to end; a crate whose unmount stops
/// waking that thread fails here with a timeout instead of hanging the run.
fn writes_reach_the_export_by_the_time_the_unmount_returns(writeback: bool) {
    let mut lb = Loopback::start(Opts {
        writeback,
        ..Opts::default()
    });
    lb.wait_ready();

    // Many small files plus one large one: the small ones exercise the
    // per-file teardown path, the large one spans enough pages that the
    // writeback thread, rather than the closing descriptor, carries some of it.
    let mut expected: Vec<(std::path::PathBuf, Vec<u8>)> = Vec::new();
    for i in 0..64u8 {
        let body = vec![i; 64 << 10];
        std::fs::write(lb.mnt().join(format!("small-{i}")), &body).unwrap();
        expected.push((lb.export().join(format!("small-{i}")), body));
    }
    let big: Vec<u8> = (0..(8u32 << 20)).map(|n| (n % 251) as u8).collect();
    std::fs::write(lb.mnt().join("big"), &big).unwrap();
    expected.push((lb.export().join("big"), big));

    lb.unmount();

    for (path, body) in expected {
        let landed = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("{} is missing after the unmount: {e}", path.display()));
        assert_eq!(
            landed.len(),
            body.len(),
            "{} is short after the unmount",
            path.display()
        );
        assert!(landed == body, "{} holds the wrong bytes", path.display());
    }
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn writes_reach_the_export_on_unmount_with_the_writeback_cache() {
    writes_reach_the_export_by_the_time_the_unmount_returns(true);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn writes_reach_the_export_on_unmount_without_the_writeback_cache() {
    writes_reach_the_export_by_the_time_the_unmount_returns(false);
}

/// A long name lifetime beside a short attribute lifetime, end to end.
///
/// The point of separating them is that a path can stay resolved while its
/// attributes go stale, so this asserts both halves: a `stat` past the
/// attribute lifetime sees a size the server changed behind the mount's back,
/// and the name itself never had to be looked up again for that to happen.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn a_long_name_lifetime_does_not_hold_a_stale_size() {
    let lb = Loopback::start(Opts {
        ttl: Duration::from_millis(50),
        entry_ttl: Duration::from_secs(3600),
        // With the writeback cache on, the kernel owns `i_size` for a file
        // this mount wrote and never refetches it, so no attribute lifetime —
        // short or long — would let the server's change show through. The
        // subject here is the TTL split, so the cache that overrides both
        // lifetimes stays off.
        writeback: false,
        ..Opts::default()
    });

    let seen = lb.mnt().join("grows");
    let real = lb.export().join("grows");
    std::fs::write(&seen, b"one").unwrap();
    assert_eq!(std::fs::metadata(&seen).unwrap().len(), 3);

    // Behind the mount's back, so only an expired attribute lifetime can
    // reveal it.
    std::fs::write(&real, b"four-plus").unwrap();
    std::thread::sleep(Duration::from_millis(200));

    assert_eq!(
        std::fs::metadata(&seen).unwrap().len(),
        9,
        "the attribute lifetime did not expire, or the entry lifetime pinned it"
    );
}

/// The promise `FUSE_HANDLE_KILLPRIV_V2` buys, checked end to end.
///
/// Asking the kernel for that capability tells it to stop clearing set-user-ID
/// itself, which is worth one round trip per write and worth nothing at all if
/// the bits then survive. So this walks the whole path: chmod through the
/// mount, write through the mount, and read the mode off the export directly,
/// behind the mount's back.
///
/// Both writeback settings, because the kernel reaches the wire flag by two
/// different routes. With the cache on, `fuse_cache_write_iter` sees a file
/// needing a strip and switches to the write-through path so the flag can ride
/// a synchronous request (`fs/fuse/file.c:1489-1491`, `file.c:1205-1206`). With
/// it off, `fuse_perform_write` gets there directly.
fn privileged_bits_die_on_write(writeback: bool) {
    let lb = Loopback::start(Opts {
        writeback,
        ..Opts::default()
    });
    lb.wait_ready();

    let seen = lb.mnt().join("suid");
    let real = lb.export().join("suid");

    std::fs::write(&seen, b"old").unwrap();
    std::fs::set_permissions(&seen, std::os::unix::fs::PermissionsExt::from_mode(0o4755)).unwrap();
    assert_eq!(
        std::fs::metadata(&real).unwrap().mode() & 0o7777,
        0o4755,
        "the chmod did not reach the export"
    );

    std::fs::write(&seen, b"new").unwrap();

    assert_eq!(
        std::fs::metadata(&real).unwrap().mode() & 0o7777,
        0o0755,
        "set-user-ID survived a write through the mount"
    );
    assert_eq!(std::fs::read(&real).unwrap(), b"new");

    // Set-group-ID with group execute goes the same way; without it the bit is
    // a mandatory-locking marker and stays.
    let exec = lb.mnt().join("sgid-exec");
    let exec_real = lb.export().join("sgid-exec");
    std::fs::write(&exec, b"old").unwrap();
    std::fs::set_permissions(&exec, std::os::unix::fs::PermissionsExt::from_mode(0o2775)).unwrap();
    assert_eq!(
        std::fs::metadata(&exec_real).unwrap().mode() & 0o7777,
        0o2775,
        "the chmod did not reach the export"
    );
    std::fs::write(&exec, b"new").unwrap();
    assert_eq!(
        std::fs::metadata(&exec_real).unwrap().mode() & 0o7777,
        0o0775
    );

    let mark = lb.mnt().join("sgid-mand");
    let mark_real = lb.export().join("sgid-mand");
    std::fs::write(&mark, b"old").unwrap();
    std::fs::set_permissions(&mark, std::os::unix::fs::PermissionsExt::from_mode(0o2664)).unwrap();
    assert_eq!(
        std::fs::metadata(&mark_real).unwrap().mode() & 0o7777,
        0o2664,
        "the chmod did not reach the export"
    );
    std::fs::write(&mark, b"new").unwrap();
    assert_eq!(
        std::fs::metadata(&mark_real).unwrap().mode() & 0o7777,
        0o2664
    );

    // Truncate carries the same obligation as write.
    let trunc = lb.mnt().join("suid-trunc");
    let trunc_real = lb.export().join("suid-trunc");
    std::fs::write(&trunc, b"0123456789").unwrap();
    std::fs::set_permissions(&trunc, std::os::unix::fs::PermissionsExt::from_mode(0o4755)).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&trunc)
        .unwrap()
        .set_len(4)
        .unwrap();
    assert_eq!(
        std::fs::metadata(&trunc_real).unwrap().mode() & 0o7777,
        0o0755
    );
    assert_eq!(std::fs::metadata(&trunc_real).unwrap().len(), 4);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn privileged_bits_die_on_write_with_the_writeback_cache() {
    privileged_bits_die_on_write(true);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn privileged_bits_die_on_write_without_the_writeback_cache() {
    privileged_bits_die_on_write(false);
}

/// Several times the negotiated I/O ceiling in one call, so the kernel has to
/// split it and the client has to put the pieces back at the right offsets.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn a_large_sequential_write_and_read_survive_chunking() {
    let mut lb = Loopback::start(Opts::default());
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();

    // Four times `max_io_size`, so at least four `WRITE` frames and four
    // `READ`s. A repeating period coprime with every power of two means a
    // chunk reassembled at the wrong offset cannot happen to match.
    let size = 4 * DEFAULT_MAX_IO_SIZE as usize;
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    std::fs::write(mnt.join("big.bin"), &data).unwrap();

    let read_back = std::fs::read(mnt.join("big.bin")).unwrap();
    assert_eq!(read_back.len(), data.len());
    assert!(
        read_back == data,
        "the bytes read back through the mount differ from the bytes written"
    );
    let on_server = std::fs::read(export.join("big.bin")).unwrap();
    assert!(
        on_server == data,
        "the bytes the server stored differ from the bytes written"
    );

    // A read that starts in the middle of a chunk, so the offset arithmetic is
    // exercised somewhere other than a boundary. Scoped, like every other open
    // file in this suite — see [`Loopback::unmount`].
    let from = 3 * DEFAULT_MAX_IO_SIZE as usize + 12_345;
    {
        let mut f = std::fs::File::open(mnt.join("big.bin")).unwrap();
        f.seek(SeekFrom::Start(from as u64)).unwrap();
        let mut tail = Vec::new();
        f.read_to_end(&mut tail).unwrap();
        assert!(
            tail == data[from..],
            "a read from an offset came back wrong"
        );
    }

    lb.unmount();
}

// ---------------------------------------------------------------------------
// Namespace
// ---------------------------------------------------------------------------

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn directories_are_made_walked_renamed_and_removed() {
    let mut lb = Loopback::start(Opts::default());
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();

    std::fs::create_dir(mnt.join("dir")).unwrap();
    std::fs::create_dir_all(mnt.join("dir/nested/deep")).unwrap();
    std::fs::write(mnt.join("dir/nested/f"), "moved").unwrap();
    assert!(export.join("dir/nested/deep").is_dir());

    // Rename across directories.
    std::fs::rename(mnt.join("dir/nested/f"), mnt.join("dir/g")).unwrap();
    assert!(!mnt.join("dir/nested/f").exists());
    assert_eq!(std::fs::read_to_string(mnt.join("dir/g")).unwrap(), "moved");
    assert!(export.join("dir/g").is_file());

    // Rename over an existing name, which POSIX says replaces it silently.
    std::fs::write(mnt.join("dir/victim"), "gone").unwrap();
    std::fs::rename(mnt.join("dir/g"), mnt.join("dir/victim")).unwrap();
    assert_eq!(
        std::fs::read_to_string(mnt.join("dir/victim")).unwrap(),
        "moved"
    );
    assert_eq!(names_in(&export.join("dir")), set(["nested", "victim"]));

    // Rename a directory.
    std::fs::rename(mnt.join("dir/nested"), mnt.join("dir/renamed")).unwrap();
    assert!(mnt.join("dir/renamed/deep").is_dir());

    // A non-empty directory cannot be removed, and the errno has to be the
    // backend's rather than something invented on the way through.
    assert_eq!(
        errno_of(std::fs::remove_dir(mnt.join("dir"))),
        Some(libc::ENOTEMPTY)
    );
    // Nor can a directory be unlinked as if it were a file.
    assert_eq!(
        errno_of(std::fs::remove_file(mnt.join("dir/renamed"))),
        Some(libc::EISDIR)
    );
    // Nor a file be removed as if it were a directory.
    assert_eq!(
        errno_of(std::fs::remove_dir(mnt.join("dir/victim"))),
        Some(libc::ENOTDIR)
    );
    assert_eq!(
        errno_of(std::fs::metadata(mnt.join("no-such-name"))),
        Some(libc::ENOENT)
    );

    std::fs::remove_dir(mnt.join("dir/renamed/deep")).unwrap();
    std::fs::remove_dir(mnt.join("dir/renamed")).unwrap();
    std::fs::remove_file(mnt.join("dir/victim")).unwrap();
    std::fs::remove_dir(mnt.join("dir")).unwrap();

    // Server-side truth: the export is as empty as it started.
    assert_eq!(std::fs::read_dir(&export).unwrap().count(), 0);
    lb.unmount();
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn symlinks_are_created_read_and_followed() {
    let mut lb = Loopback::start(Opts::default());
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();

    std::fs::write(mnt.join("target.txt"), "pointed at").unwrap();
    std::os::unix::fs::symlink("target.txt", mnt.join("link")).unwrap();

    assert_eq!(
        std::fs::read_link(mnt.join("link")).unwrap(),
        Path::new("target.txt")
    );
    assert!(std::fs::symlink_metadata(mnt.join("link"))
        .unwrap()
        .file_type()
        .is_symlink());
    // Following it is the kernel's job, but only after `READLINK` gives it
    // something to follow.
    assert_eq!(
        std::fs::read_to_string(mnt.join("link")).unwrap(),
        "pointed at"
    );
    assert_eq!(
        std::fs::read_link(export.join("link")).unwrap(),
        Path::new("target.txt"),
        "the server stored the target verbatim"
    );

    // A target that does not resolve still reads back exactly, and only fails
    // when something tries to follow it.
    std::os::unix::fs::symlink("../nowhere/at/all", mnt.join("dangling")).unwrap();
    assert_eq!(
        std::fs::read_link(mnt.join("dangling")).unwrap(),
        Path::new("../nowhere/at/all")
    );
    assert_eq!(
        errno_of(std::fs::read(mnt.join("dangling"))),
        Some(libc::ENOENT)
    );

    // Unlinking a symlink removes the link, never the target.
    std::fs::remove_file(mnt.join("link")).unwrap();
    assert!(mnt.join("target.txt").exists());
    assert_eq!(names_in(&export), set(["dangling", "target.txt"]));
    lb.unmount();
}

/// Two names, one inode — which the mount can only report by making both names
/// resolve to one node id, because `attr.ino` *is* the FUSE nodeid.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn hard_links_share_an_inode_and_move_the_link_count() {
    let mut lb = Loopback::start(Opts::default());
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();

    std::fs::write(mnt.join("original"), "shared bytes").unwrap();
    assert_eq!(std::fs::metadata(mnt.join("original")).unwrap().nlink(), 1);

    std::fs::hard_link(mnt.join("original"), mnt.join("alias")).unwrap();
    let first = std::fs::metadata(mnt.join("original")).unwrap();
    let second = std::fs::metadata(mnt.join("alias")).unwrap();
    assert_eq!(
        first.ino(),
        second.ino(),
        "hard links must report one st_ino: the server keys its node table on \
         (st_dev, st_ino), so both names have to land on one node id"
    );
    assert_eq!(first.nlink(), 2);
    assert_eq!(second.nlink(), 2);
    assert_eq!(second.len(), "shared bytes".len() as u64);

    // One inode means one set of bytes, whichever name reaches them.
    std::fs::write(mnt.join("alias"), "rewritten!!!").unwrap();
    assert_eq!(
        std::fs::read_to_string(mnt.join("original")).unwrap(),
        "rewritten!!!"
    );
    assert_eq!(
        std::fs::metadata(export.join("original")).unwrap().ino(),
        std::fs::metadata(export.join("alias")).unwrap().ino(),
        "the server's own view agrees that these are one file"
    );

    std::fs::remove_file(mnt.join("alias")).unwrap();
    assert_eq!(std::fs::metadata(mnt.join("original")).unwrap().nlink(), 1);
    assert_eq!(
        std::fs::read_to_string(mnt.join("original")).unwrap(),
        "rewritten!!!"
    );
    lb.unmount();
}

/// Enough names that the listing cannot arrive in one page, so the cursor the
/// server hands back has to be right roughly a hundred times in a row.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn a_large_directory_lists_every_name_across_many_pages() {
    // Names are deliberately long: the client asks for a 4 KiB page and the
    // server charges `namelen + 160` per `READDIRPLUS` entry, so ~20 names to
    // a page and ~100 pages for the listing.
    const NAMES: usize = 2_000;
    // The stat pass at the end of this case looks every name up, and the
    // server keeps a descriptor per looked-up node: measured peak is 2025
    // descriptors for this process against `NAMES` of 2000. Checked before
    // anything is mounted, so a host that cannot run the case says so instead
    // of failing with `EMFILE` halfway through a listing.
    require_open_files(NAMES as u64 + 256);

    let mut lb = Loopback::start(Opts::default());
    let mnt = lb.mnt().to_path_buf();

    // Built on the server side, and never looked at through the mount before
    // it is complete, so no cached listing can flatter the result.
    let big = lb.export().join("big");
    std::fs::create_dir(&big).unwrap();
    let mut want = BTreeSet::new();
    for i in 0..NAMES {
        let name = format!("entry-{i:05}-with-a-name-long-enough-to-fill-pages");
        std::fs::write(big.join(&name), "").unwrap();
        want.insert(name);
    }

    let got = names_in(&mnt.join("big"));
    assert_eq!(got.len(), NAMES, "the listing lost or invented names");
    assert_eq!(got, want);

    // A second listing, through a fresh `OPENDIR`, has to agree with the first:
    // a cursor that only works on a cold directory is a cursor that does not
    // work.
    assert_eq!(names_in(&mnt.join("big")), want);

    // `read_dir` never reports the dots, but a client that emitted them into
    // the kernel's buffer under the wrong offsets would show up here.
    assert!(!got.contains(".") && !got.contains(".."));

    // And the attributes the listing carries have to be usable, which is the
    // half of `READDIRPLUS` a name-only comparison cannot see.
    let sized = std::fs::read_dir(mnt.join("big"))
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().metadata().unwrap().is_file())
        .count();
    assert_eq!(sized, NAMES);

    lb.unmount();
}

/// Names sized so that the kernel refuses the *first* entry of a server page
/// with entries from earlier pages already in the reply.
///
/// This is the case a page-relative index cannot tell apart from "the buffer
/// could not hold one entry". The client asks the server for 4 KiB of listing
/// per round trip and pours as many pages as that takes into the one buffer the
/// kernel handed it. The two sides price an entry differently — the server
/// charges `namelen + 160`, the kernel `align8(152 + namelen)` — so a page that
/// exactly fills the server's budget leaves the kernel's buffer a few bytes
/// short. Those few bytes add up until one page's opening entry is the one that
/// does not fit, and a bridge that reads "first of this page" as "first of this
/// reply" answers with `EIO` in a debug build and a false error log in a
/// release one.
///
/// The lengths below tune the residue rather than leaving it to luck. Ten
/// names — nine of 249 bytes and one of 243 — cost the server 4084 of its 4088
/// usable bytes and the kernel 4072 of a 4096-byte page, a shortfall of 24
/// bytes per page against an entry that costs 400. The kernel's readdir buffer
/// is a whole number of pages, so the two accountings stay in step until the
/// buffer runs out, and the entry it runs out on opens its page for every
/// buffer from 4 KiB to 52 KiB. Every window of ten names holds exactly one
/// short one, so the packing survives whatever order the export's own
/// filesystem lists them in.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn a_page_boundary_inside_one_reply_does_not_fail_the_listing() {
    // Enough names to outlast the largest buffer the arithmetic above covers,
    // with room to spare.
    const NAMES: usize = 600;
    // Same descriptor arithmetic as the case above: one server-side `O_PATH`
    // per name the listing resolves.
    require_open_files(NAMES as u64 + 256);

    let mut lb = Loopback::start(Opts::default());
    let mnt = lb.mnt().to_path_buf();

    let wide = lb.export().join("wide");
    std::fs::create_dir(&wide).unwrap();
    let mut want = BTreeSet::new();
    for i in 0..NAMES {
        let name = tuned_name(i);
        std::fs::write(wide.join(&name), "").unwrap();
        want.insert(name);
    }

    assert_eq!(names_in(&mnt.join("wide")), want);
    // A fresh `OPENDIR`, so the second pass pages the listing again rather
    // than reading the kernel's cache of the first.
    assert_eq!(names_in(&mnt.join("wide")), want);

    // `READDIRPLUS` carries attributes and `READDIR` does not, and the kernel
    // picks one form per call. Statting every name exercises the other loop
    // over the same boundary.
    let files = std::fs::read_dir(mnt.join("wide"))
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().metadata().unwrap().is_file())
        .count();
    assert_eq!(files, NAMES);

    lb.unmount();
}

/// One unique name of the length its position in the block calls for.
fn tuned_name(i: usize) -> String {
    let len = if i.is_multiple_of(10) { 243 } else { 249 };
    let prefix = format!("{i:05}-");
    format!("{prefix}{}", "n".repeat(len - prefix.len()))
}

// ---------------------------------------------------------------------------
// Attributes
// ---------------------------------------------------------------------------

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn truncate_shrinks_extends_and_zero_fills() {
    let mut lb = Loopback::start(Opts::default());
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();

    std::fs::write(mnt.join("trunc"), vec![b'a'; 4096]).unwrap();

    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(mnt.join("trunc"))
        .unwrap();
    f.set_len(10).unwrap();
    drop(f);
    assert_eq!(std::fs::metadata(mnt.join("trunc")).unwrap().len(), 10);
    assert_eq!(std::fs::read(mnt.join("trunc")).unwrap(), vec![b'a'; 10]);

    // Extending leaves a hole, and a hole reads as zeros.
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(mnt.join("trunc"))
        .unwrap();
    f.set_len(8192).unwrap();
    drop(f);
    let grown = std::fs::read(mnt.join("trunc")).unwrap();
    assert_eq!(grown.len(), 8192);
    assert_eq!(&grown[..10], &[b'a'; 10]);
    assert!(
        grown[10..].iter().all(|b| *b == 0),
        "the extension has to read back as zeros"
    );
    assert_eq!(std::fs::metadata(export.join("trunc")).unwrap().len(), 8192);

    lb.unmount();
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn an_mtime_set_through_the_mount_round_trips() {
    let mut lb = Loopback::start(Opts::default());
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();

    std::fs::write(mnt.join("stamped"), "t").unwrap();

    // A time with a non-zero nanosecond field, because the seconds alone would
    // survive a conversion that dropped the fraction.
    let when = SystemTime::UNIX_EPOCH + Duration::new(1_600_000_000, 123_456_789);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(mnt.join("stamped"))
        .unwrap();
    f.set_modified(when).unwrap();
    drop(f);

    assert_eq!(
        std::fs::metadata(mnt.join("stamped"))
            .unwrap()
            .modified()
            .unwrap(),
        when
    );
    assert_eq!(
        std::fs::metadata(export.join("stamped"))
            .unwrap()
            .modified()
            .unwrap(),
        when,
        "the server stored the time the mount was given, to the nanosecond"
    );

    // And an ordinary write moves it forward again, rather than freezing it at
    // whatever `SETATTR` last said.
    std::fs::write(mnt.join("stamped"), "later").unwrap();
    assert!(
        std::fs::metadata(mnt.join("stamped"))
            .unwrap()
            .modified()
            .unwrap()
            > when
    );
    lb.unmount();
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn xattrs_are_set_read_listed_and_removed() {
    let mut lb = Loopback::start(Opts::default());
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();
    let f = mnt.join("attrs");
    std::fs::write(&f, "body").unwrap();

    rustix::fs::setxattr(&f, "user.one", b"first", XattrFlags::empty()).unwrap();
    rustix::fs::setxattr(&f, "user.two", b"second", XattrFlags::empty()).unwrap();
    assert_eq!(xattr_value(&f, "user.one").unwrap(), b"first");
    assert_eq!(xattr_value(&f, "user.two").unwrap(), b"second");

    let names = xattr_names(&f);
    assert!(
        names.contains("user.one") && names.contains("user.two"),
        "listxattr reported {names:?}"
    );

    // FUSE reads an xattr in two steps, and the first one asks only for the
    // length. A client that answered it with the value — or with the wrong
    // length — breaks every caller that sizes its buffer this way.
    let mut nothing: [u8; 0] = [];
    assert_eq!(
        rustix::fs::getxattr(&f, "user.one", &mut nothing[..]).unwrap(),
        b"first".len()
    );
    // And the second step has to answer `ERANGE` when the buffer is too small
    // rather than truncating.
    let mut too_small = [0u8; 1];
    assert_eq!(
        rustix::fs::getxattr(&f, "user.one", &mut too_small[..]),
        Err(rustix::io::Errno::RANGE)
    );

    // `XATTR_CREATE` on a name that exists is the backend's `EEXIST`, carried
    // through unaltered.
    assert_eq!(
        rustix::fs::setxattr(&f, "user.one", b"again", XattrFlags::CREATE),
        Err(rustix::io::Errno::EXIST)
    );
    rustix::fs::setxattr(&f, "user.one", b"replaced", XattrFlags::REPLACE).unwrap();
    assert_eq!(xattr_value(&f, "user.one").unwrap(), b"replaced");

    // Server-side truth, before and after the removal.
    assert_eq!(
        xattr_value(&export.join("attrs"), "user.one").unwrap(),
        b"replaced"
    );
    rustix::fs::removexattr(&f, "user.one").unwrap();
    assert_eq!(
        xattr_value(&f, "user.one"),
        Err(rustix::io::Errno::NODATA),
        "a removed xattr is gone, not empty"
    );
    assert_eq!(user_xattr_names(&f), set(["user.two"]));
    assert_eq!(user_xattr_names(&export.join("attrs")), set(["user.two"]));

    lb.unmount();
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn statfs_reports_the_exported_filesystem() {
    let mut lb = Loopback::start(Opts::default());
    let inside = rustix::fs::statvfs(lb.mnt()).unwrap();
    let outside = rustix::fs::statvfs(lb.export()).unwrap();

    // The numbers that describe the backing filesystem come from it verbatim.
    assert_eq!(inside.f_bsize, outside.f_bsize);
    assert_eq!(inside.f_frsize, outside.f_frsize);
    assert_eq!(inside.f_blocks, outside.f_blocks);
    assert_eq!(inside.f_namemax, outside.f_namemax);
    // Free space moves under a live tmpfs, so it is checked for plausibility
    // rather than equality — a zero here would mean the reply was never filled
    // in.
    assert!(inside.f_bfree > 0 && inside.f_bavail > 0);
    assert!(inside.f_files > 0);

    // The flags are the local mount's rather than the server's, and they are
    // the ones the client insisted on: a compromised server's setuid bit or
    // device node must not be honoured here.
    assert!(inside.f_flag.contains(StatVfsMountFlags::NOSUID));
    assert!(inside.f_flag.contains(StatVfsMountFlags::NODEV));

    lb.unmount();
}

/// Both durability policies answer, and the data is on the server afterwards.
///
/// What the policy changes — whether the backend issues `fdatasync` or returns
/// without one — is by construction invisible from userspace, so what is pinned
/// here is that neither policy turns `fsync(2)` into an error and neither loses
/// the write.
fn fsync_is_honoured_under(policy: FsyncPolicy) {
    let mut lb = Loopback::start(Opts {
        fsync: policy,
        ..Opts::default()
    });
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();

    let mut f = std::fs::File::create(mnt.join("durable")).unwrap();
    f.write_all(b"durable bytes").unwrap();
    f.sync_all().unwrap();
    // `fsync` flushes the page cache on its way out, so the bytes are on the
    // server before the file is even closed.
    assert_eq!(
        std::fs::read_to_string(export.join("durable")).unwrap(),
        "durable bytes"
    );

    f.write_all(b" and more").unwrap();
    f.sync_data().unwrap();
    assert_eq!(
        std::fs::read_to_string(export.join("durable")).unwrap(),
        "durable bytes and more"
    );
    drop(f);

    // `FSYNCDIR` takes the same path with a directory handle.
    std::fs::File::open(&mnt).unwrap().sync_all().unwrap();

    lb.unmount();
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn fsync_is_honoured_under_the_honor_policy() {
    fsync_is_honoured_under(FsyncPolicy::Honor);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn fsync_is_answered_under_the_ignore_policy() {
    fsync_is_honoured_under(FsyncPolicy::Ignore);
}

/// The forced-sync control, driven from user space through the mount root.
///
/// A `setxattr` of [`CONTROL_XATTR_SYNC`] on the mountpoint never reaches the
/// server as an attribute: the bridge turns it into a forced `FSYNCDIR`, which
/// the server answers with a real `syncfs(2)` whatever its durability policy
/// (spec §6, §11).
///
/// **What this proves and what it does not.** The `setxattr` returning zero says
/// the round trip worked. The connection's own call is the stronger claim: it
/// fails with `EOPNOTSUPP` unless the server acknowledged the forced sync on the
/// reply frame, and the server sets that bit only on the branch that performed
/// the syscall — so an `Ok` here is the server reporting, across a real socket,
/// that `syncfs(2)` ran and returned success. What no test at this layer can
/// show is the bytes reaching the platter; that needs a power cut, which is VM
/// work.
fn the_forced_sync_control_works_under(policy: FsyncPolicy) {
    let mut lb = Loopback::start(Opts {
        fsync: policy,
        ..Opts::default()
    });
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();
    let control = std::str::from_utf8(CONTROL_XATTR_SYNC).unwrap();

    // Data the policy may have left dirty in the server's page cache.
    std::fs::write(mnt.join("unsynced"), b"bytes nobody fsynced").unwrap();

    // The user-space entry point. Any value asks for one sync.
    rustix::fs::setxattr(&mnt, control, b"1", XattrFlags::empty())
        .unwrap_or_else(|e| panic!("{policy:?}: the control xattr must succeed, got {e:?}"));
    // And again, because a control is not a one-shot.
    rustix::fs::setxattr(&mnt, control, b"", XattrFlags::empty()).unwrap();

    // The same call the driver makes at unmount, reporting whether the server
    // acknowledged it. This is the assertion that pins the honour branch.
    lb.on_client_rt(lb.conn().force_sync_export())
        .unwrap_or_else(|e| panic!("{policy:?}: the server must acknowledge the sync, got {e:?}"));

    // The control stores nothing, so it reads back absent and never lists.
    assert_eq!(
        errno_of(xattr_value(&mnt, control).map_err(std::io::Error::from)),
        Some(libc::ENODATA),
        "the control is an action, not an attribute"
    );
    assert!(
        !user_xattr_names(&mnt).contains(control),
        "the control must not appear in the mount root's listing"
    );

    // The shadowing bound: the same name on any other file is an ordinary
    // attribute and travels like one. Only the root inode loses the name.
    let f = mnt.join("ordinary");
    std::fs::write(&f, b"body").unwrap();
    rustix::fs::setxattr(&f, control, b"stored", XattrFlags::empty()).unwrap();
    assert_eq!(xattr_value(&f, control).unwrap(), b"stored");
    assert!(user_xattr_names(&f).contains(control));
    // The server has it too, under the same name.
    assert_eq!(
        xattr_value(&export.join("ordinary"), control).unwrap(),
        b"stored"
    );

    // The mount is still healthy after all of it.
    assert_eq!(
        std::fs::read(export.join("unsynced")).unwrap(),
        b"bytes nobody fsynced"
    );
    lb.unmount();
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn the_forced_sync_control_works_under_the_ignore_policy() {
    the_forced_sync_control_works_under(FsyncPolicy::Ignore);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn the_forced_sync_control_works_under_the_honor_policy() {
    the_forced_sync_control_works_under(FsyncPolicy::Honor);
}

// An application's own `fsync(2)` still rides the server's policy — the bridge
// must not quietly force every sync the kernel passes down, or `fsync =
// "ignore"` would have nothing left to configure. That property has no test at
// *this* layer, and cannot: the FIFO that witnesses a real `fsync(2)` is opened
// by the kernel's own `fifo_open` rather than by FUSE, so a sync on one never
// reaches the server at all. It is pinned where the witness works instead —
// `crates/lbfs-client/tests/live.rs` drives the connection directly, and
// `tests/tests/protocol.rs` drives the frames.

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

/// Many threads at once, which is the only way to reach the property the whole
/// design rests on: one FUSE dispatch thread, many requests in flight.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn concurrent_threads_share_the_mount() {
    let mut lb = Loopback::start(Opts::default());
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();

    const THREADS: usize = 8;
    const PER_THREAD: usize = 32;

    // A file every reader can pull on at the same time, big enough that the
    // reads overlap rather than finishing one at a time.
    let shared: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(mnt.join("shared.bin"), &shared).unwrap();

    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let mnt = mnt.as_path();
            let shared = &shared;
            scope.spawn(move || {
                for i in 0..PER_THREAD {
                    let path = mnt.join(format!("t{t}-{i:02}"));
                    let body = format!("thread {t} file {i}");
                    std::fs::write(&path, &body).unwrap();
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
                }
                // Every thread also reads the same file, so the multiplexer is
                // correlating replies for one inode across eight callers.
                let got = std::fs::read(mnt.join("shared.bin")).unwrap();
                assert!(got == *shared, "thread {t} read the shared file wrong");
                // And lists the directory while the others are still writing
                // into it, which is a listing over a moving target.
                let _ = std::fs::read_dir(mnt).unwrap().count();
            });
        }
    });

    // Every file, exactly once, with the right contents — checked on the server
    // rather than through the mount's caches.
    let mut expected: BTreeSet<String> = (0..THREADS)
        .flat_map(|t| (0..PER_THREAD).map(move |i| format!("t{t}-{i:02}")))
        .collect();
    expected.insert("shared.bin".to_string());
    assert_eq!(names_in(&export), expected);
    for t in 0..THREADS {
        for i in 0..PER_THREAD {
            assert_eq!(
                std::fs::read_to_string(export.join(format!("t{t}-{i:02}"))).unwrap(),
                format!("thread {t} file {i}")
            );
        }
    }
    assert_eq!(
        lb.conn().dropped_forgets(),
        0,
        "the forget queue overflowed under load, which leaks server nodes"
    );
    lb.unmount();
}

// ---------------------------------------------------------------------------
// Teardown
// ---------------------------------------------------------------------------

/// The unmount has to give everything back, and the only witness that does not
/// take the client's word for it is the server's descriptor count.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn unmounting_returns_every_server_descriptor() {
    let mut lb = Loopback::start(Opts::default());
    let mnt = lb.mnt().to_path_buf();

    // An attached session that has served one `readdir` holds a handful: the
    // export root, the registered root node, and whatever handle the readiness
    // check left in flight. The number is not pinned — it is the *baseline*,
    // and what matters is that it is small and that it comes back.
    let attached = lb.export_fds();
    assert!(
        (1..=8).contains(&attached),
        "an attached session should hold a descriptor or two for the export \
         root, not {attached}"
    );

    const DIRS: usize = 64;
    for i in 0..DIRS {
        std::fs::create_dir(mnt.join(format!("d{i}"))).unwrap();
        std::fs::write(mnt.join(format!("d{i}/f")), "content").unwrap();
        assert_eq!(
            std::fs::read_to_string(mnt.join(format!("d{i}/f"))).unwrap(),
            "content"
        );
    }
    let held = lb.export_fds();
    assert!(
        held >= 2 * DIRS,
        "the server should be holding a descriptor per registered node while \
         the mount is live; {DIRS} directories and {DIRS} files came to {held}"
    );

    lb.unmount();
    assert!(!is_fuse_mount(&mnt), "the unmount left the mount behind");
    assert_eq!(
        std::fs::read_dir(&mnt).unwrap().count(),
        0,
        "the mountpoint is the empty directory it was before the mount"
    );
    assert_eq!(
        lb.conn().dropped_forgets(),
        0,
        "forgets were dropped, so the server is holding nodes nothing will \
         ever retire"
    );

    // Closing the socket is what ends the session, and the session is what owns
    // the node table. Nothing under the export may survive it.
    lb.disconnect();
    wait_for(
        "the server to close every descriptor into the export",
        SETTLE_TIMEOUT,
        || lb.export_fds() == 0,
    );
}

/// Descriptors come back *during* the mount too, not only when it ends.
///
/// This is the leak that does not announce itself: every `LOOKUP` and every
/// `READDIRPLUS` entry costs the server a registered node and an `O_PATH`
/// descriptor, and the only thing that ever gives one back is a `FORGET`. A
/// bridge that dropped them would pass every other test in this file and walk a
/// long-running mount into `EMFILE`.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn forgets_release_descriptors_while_the_mount_is_live() {
    let mut lb = Loopback::start(Opts::default());
    let mnt = lb.mnt().to_path_buf();

    const FILES: usize = 200;
    /// The export root, the root node, and any handle still in flight. The
    /// point of the test is two hundred descriptors going away, not the last
    /// two.
    const RESIDUE: usize = 8;

    for i in 0..FILES {
        std::fs::write(mnt.join(format!("f{i:03}")), "x").unwrap();
    }
    // A full listing, which takes a lookup count for every name in one go.
    assert_eq!(names_in(&mnt).len(), FILES);
    let held = lb.export_fds();
    assert!(
        held >= FILES,
        "expected a descriptor per registered node, got {held} for {FILES} files"
    );

    for i in 0..FILES {
        std::fs::remove_file(mnt.join(format!("f{i:03}"))).unwrap();
    }

    // Unlinking evicts the inode, evicting the inode queues a `FORGET`, and the
    // client batches those behind a 500 ms timer — so this is a bounded wait
    // rather than an immediate assertion.
    wait_for(
        "the server to release the descriptors for the unlinked files",
        SETTLE_TIMEOUT,
        || lb.export_fds() <= RESIDUE,
    );
    lb.unmount();
}

/// A server that vanishes leaves a mount that answers `EIO` and can still be
/// taken down — never one that hangs, and never one that lies (spec §7).
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn a_dead_server_leaves_an_eio_mount_that_still_unmounts() {
    // No caching, or the kernel would answer from its own copies and prove
    // nothing about the connection underneath.
    let mut lb = Loopback::start(Opts {
        ttl: Duration::ZERO,
        entry_ttl: Duration::ZERO,
        ..Opts::default()
    });
    let mnt = lb.mnt().to_path_buf();
    std::fs::write(mnt.join("before"), "written while alive").unwrap();

    lb.server.kill();

    // The client notices at its own pace — the socket has to reach EOF and the
    // reader task has to mark the connection dead — so this is a bounded wait
    // for the first `EIO`, not an immediate assertion.
    wait_for("the mount to start answering EIO", SETTLE_TIMEOUT, || {
        errno_of(std::fs::metadata(mnt.join("never-existed"))) == Some(libc::EIO)
    });
    assert!(lb.conn().is_dead());
    assert_eq!(
        errno_of(std::fs::read(mnt.join("before"))),
        Some(libc::EIO),
        "a name the kernel knows about still needs the server to open it"
    );
    assert_eq!(
        errno_of(std::fs::write(mnt.join("after"), "x")),
        Some(libc::EIO)
    );
    assert_eq!(errno_of(std::fs::read_dir(&mnt)), Some(libc::EIO));

    // And the mount comes down anyway. This is the assertion that matters most
    // for anybody operating it: losing the server must not cost a reboot.
    lb.unmount();
    assert!(
        !is_fuse_mount(&mnt),
        "a mount whose server died could not be unmounted"
    );
    assert_eq!(std::fs::read_dir(&mnt).unwrap().count(), 0);
}

// ---------------------------------------------------------------------------
// A severed connection
// ---------------------------------------------------------------------------
//
// What every case below is about, and what none of the cases above can reach:
// the server stays up and the *wire* fails. The client re-attaches to the
// session it already had, so node ids, open descriptors and directory cursors
// go on meaning what they meant (design §2), while anything that was in flight
// at the break still failed `EIO` (design §3.1).
//
// These are the only cases in the file that ask for resumption. Every other
// mount here keeps the library default — off — so nothing else in the suite
// changed clocks or teardown when this feature landed.

/// The `Opts` every severed-connection case starts from.
fn severable() -> Opts {
    Opts {
        breaker: true,
        resume: true,
        ..Opts::default()
    }
}

/// Wait until the mount is serving again after a [`Breaker::sever`].
///
/// The probe is a `readdir` of the mount root, because `OPENDIR` is a request
/// no cache can answer: an `Ok` here is a server on the other end of a working
/// socket rather than the client's own memory of one. The first call parks
/// inside the client for as long as the reconnect takes, which is the feature
/// working rather than a wait; the bound around it is for the case where the
/// mount never comes back at all.
fn wait_for_the_mount_to_answer_again(mnt: &Path) {
    wait_for(
        "the mount to answer again after the sever",
        SETTLE_TIMEOUT,
        || std::fs::read_dir(mnt).is_ok(),
    );
}

/// A descriptor open across a severed connection still addresses its file.
///
/// The `Fh` is an index into a table the server keeps, and the descriptor
/// behind it is what pins the inode — so the second write below lands in the
/// same file as the first because neither the table nor the descriptor ever
/// went away. A client that re-opened by name would pass this case and fail
/// `a_held_descriptor_keeps_its_file_across_a_sever`, which is why the two are
/// separate.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn an_open_descriptor_survives_a_sever() {
    let mut lb = Loopback::start(severable());
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();

    let mut held = std::fs::File::create(mnt.join("held")).unwrap();
    held.write_all(b"before the break").unwrap();
    // On the server before the cut, so what the case proves afterwards is
    // about the descriptor rather than about what the page cache happened to
    // still be holding.
    held.sync_all().unwrap();

    lb.breaker().sever();
    std::thread::sleep(NOTICED);
    wait_for_the_mount_to_answer_again(&mnt);

    // The same `File`, so the same `Fh`, so the same descriptor on the server.
    held.write_all(b", and after it").unwrap();
    held.sync_all().unwrap();
    drop(held);

    // Read from the export rather than back through the mount, which would
    // only prove the mount agrees with itself.
    assert_eq!(
        std::fs::read_to_string(export.join("held")).unwrap(),
        "before the break, and after it",
        "the two writes had to land in order, in one file, through one \
         descriptor that outlived the socket under it"
    );
    lb.unmount();
}

/// A directory walk in progress survives a severed connection.
///
/// `OPENDIR` snapshots the listing and the handle hands out cookies over that
/// snapshot (spec §3.3), so a resumed session continues the same listing from
/// the last cookie the *first* connection issued. Under any design that rebuilt
/// the handle instead, that cookie belongs to a snapshot nobody has any more:
/// `EINVAL`, or a listing quietly missing names — which is the failure
/// `readdir(3)` must never have.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn a_directory_walk_survives_a_sever() {
    // Enough names, at enough bytes each, that no single `getdents64` can hold
    // the listing: glibc reads into a 32 KiB buffer and the kernel charges
    // `align8(152 + namelen)` an entry, so this needs three of them and the
    // last two happen after the break.
    const NAMES: usize = 300;
    // One server-side `O_PATH` per name the listing resolves, as in the other
    // large-directory cases.
    require_open_files(NAMES as u64 + 256);

    let mut lb = Loopback::start(severable());
    let mnt = lb.mnt().to_path_buf();

    // Built on the export and never listed through the mount before it is
    // complete, so no cached listing can flatter the result.
    let dir = lb.export().join("walk");
    std::fs::create_dir(&dir).unwrap();
    let mut want = BTreeSet::new();
    for i in 0..NAMES {
        let name = format!("entry-{i:04}-with-a-name-long-enough-to-need-several-pages");
        std::fs::write(dir.join(&name), "").unwrap();
        want.insert(name);
    }

    let mut walk = std::fs::read_dir(mnt.join("walk")).unwrap();
    let mut seen = Vec::new();
    seen.push(
        walk.next()
            .expect("the listing has entries")
            .unwrap()
            .file_name()
            .into_string()
            .unwrap(),
    );

    lb.breaker().sever();
    std::thread::sleep(NOTICED);

    // The same iterator, so the same `Dh` and the same cursor. Every `next`
    // from here parks until the session comes back and then reads on.
    for entry in walk {
        seen.push(entry.unwrap().file_name().into_string().unwrap());
    }

    let unique: BTreeSet<String> = seen.iter().cloned().collect();
    assert_eq!(
        unique.len(),
        seen.len(),
        "the listing repeated a name across the break, which is a cursor that \
         went backwards"
    );
    assert_eq!(
        unique, want,
        "the two halves of the walk do not add up to the directory: a name is \
         missing, or one arrived that is not there"
    );
    lb.unmount();
}

/// The mount comes down while the client is still trying to re-attach.
///
/// Spec §8's rule does not bend for this feature: a filesystem may fail, and
/// may not hang. A supervisor with seconds left on its deadline must not be
/// able to hold an unmount for even one of them, which is what
/// `Session::shutdown` is for.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn a_mount_unmounts_while_a_sever_is_still_reconnecting() {
    let mut lb = Loopback::start(severable());
    let mnt = lb.mnt().to_path_buf();

    // Written and closed, so the unmount below has no dirty page to flush and
    // measures the supervisor rather than a parked writeback behind it.
    std::fs::write(mnt.join("before"), "written while the wire was up").unwrap();

    lb.breaker().sever();
    // The redial cannot land: with the server gone every dial is refused, so
    // the supervisor stays in its loop for the whole of RECONNECT_DEADLINE.
    // The only way it leaves `Reconnecting` inside the next ten seconds is the
    // unmount marking the session dead — which is the assertion.
    lb.server.kill();
    std::thread::sleep(NOTICED);

    let started = Instant::now();
    lb.unmount();
    let waited = started.elapsed();
    assert!(
        waited < RECONNECT_DEADLINE,
        "the unmount took {waited:?} against a {RECONNECT_DEADLINE:?} reconnect \
         deadline, so it waited the supervisor out rather than cancelling it"
    );
    assert!(
        !is_fuse_mount(&mnt),
        "a mount whose connection was severed could not be unmounted"
    );
    assert_eq!(std::fs::read_dir(&mnt).unwrap().count(), 0);
}

/// A file replaced on the export during the gap does not change what a held
/// descriptor reads.
///
/// **The case the whole design exists to get right.** Design §2.1: re-looking a
/// name up across the gap would bind the client's remembered node id to a fresh
/// inode, and every read afterwards would return bytes from a file the
/// application never opened — data loss with no message attached. Retention
/// makes it a non-event, because the server's `O_PATH` descriptor pins the
/// original inode whatever happens to the name above it.
///
/// The protocol-level twin of this lives in `tests/tests/protocol.rs`; this one
/// is the same claim with a real kernel, a real `open(2)` and a real page
/// cache in the way.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn a_held_descriptor_keeps_its_file_across_a_sever() {
    let mut lb = Loopback::start(Opts {
        // Nothing cached. A held descriptor reading the original bytes out of
        // the client's own page cache would prove nothing about the server,
        // and a fresh lookup answered from the entry cache would never reach
        // the new file at all.
        ttl: Duration::ZERO,
        entry_ttl: Duration::ZERO,
        ..severable()
    });
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();

    // Made on the export, so nothing this mount did put its bytes into the
    // client's page cache on the way past.
    std::fs::write(export.join("identity"), "the original bytes").unwrap();
    let held = std::fs::File::open(mnt.join("identity")).unwrap();
    let original = held.metadata().unwrap().ino();

    lb.breaker().sever();
    // Replaced rather than rewritten: a new inode under the old name, which is
    // exactly what a rebuild by name cannot tell from the old file.
    std::fs::write(export.join("scratch"), "entirely different bytes").unwrap();
    std::fs::rename(export.join("scratch"), export.join("identity")).unwrap();

    wait_for_the_mount_to_answer_again(&mnt);

    let mut through_the_descriptor = String::new();
    (&held)
        .read_to_string(&mut through_the_descriptor)
        .expect("the held descriptor still reads");
    assert_eq!(
        through_the_descriptor, "the original bytes",
        "the descriptor followed the name to the new file, which is the one \
         thing a reconnect may never do"
    );

    // And the name resolves to the new file, on a node id that is not the one
    // the descriptor holds. `attr.ino` is the FUSE node id, so this is the
    // client-visible half of "a different `NodeId` with a different
    // generation".
    assert_eq!(
        std::fs::read_to_string(mnt.join("identity")).unwrap(),
        "entirely different bytes"
    );
    assert_ne!(
        std::fs::metadata(mnt.join("identity")).unwrap().ino(),
        original,
        "a fresh lookup of the name has to yield a new node, or the old id \
         quietly came to mean the new file"
    );

    drop(held);
    lb.unmount();
}

/// The server's descriptors come back at the unmount, not at the end of the
/// grace.
///
/// Retention is what makes a clean unmount worth a message: a socket closing
/// cannot mean "drop this session", because a crashed client closes its socket
/// the same way a polite one does — and the crashed client is the case
/// retention exists for (design §7.5). So the client says it out loud, and this
/// is the case that reads the consequence off the server.
///
/// The bound is the point. `SETTLE_TIMEOUT` is thirty seconds and the harness
/// configures a sixty-second `resume_grace`, so a session left to the reaper
/// cannot pass this wait — only a `DETACH` can.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn descriptors_come_back_after_a_severed_mount_unmounts() {
    let mut lb = Loopback::start(severable());
    let mnt = lb.mnt().to_path_buf();

    const DIRS: usize = 32;
    for i in 0..DIRS {
        std::fs::create_dir(mnt.join(format!("d{i}"))).unwrap();
        std::fs::write(mnt.join(format!("d{i}/f")), "content").unwrap();
    }
    let held = lb.export_fds();
    assert!(
        held >= 2 * DIRS,
        "the server should be holding a descriptor per registered node while \
         the mount is live; {DIRS} directories and {DIRS} files came to {held}"
    );

    lb.breaker().sever();
    std::thread::sleep(NOTICED);
    wait_for_the_mount_to_answer_again(&mnt);
    assert!(
        lb.export_fds() >= 2 * DIRS,
        "the session came back with fewer descriptors than it had, so \
         something was rebuilt rather than retained"
    );

    // Unmount, which drains and then detaches, and drop both connections. What
    // is left is the server, and it must be holding nothing.
    lb.unmount();
    lb.disconnect();
    wait_for(
        "the server to close every descriptor into the export",
        SETTLE_TIMEOUT,
        || lb.export_fds() == 0,
    );
}

// ---------------------------------------------------------------------------

fn set<const N: usize>(names: [&str; N]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

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
use std::os::unix::fs::MetadataExt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use lbfs_client::conn::Connection;
use lbfs_client::fuse::{mount_options, LbfsFuse};
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
        "the loopback suite needs `fusermount3` on PATH: libfuse3 shells out to \
         it for both the unprivileged mount and the unmount. Install fuse3."
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
    /// Entry and attribute lifetime. Zero makes every name and every `stat` a
    /// round trip, which is what a test about a *dead* server needs — with the
    /// default second, the kernel would answer from cache and prove nothing.
    ttl: Duration,
}

impl Default for Opts {
    /// What the shipped client does by default (spec §7).
    fn default() -> Opts {
        Opts {
            writeback: true,
            fsync: FsyncPolicy::Honor,
            ttl: Duration::from_secs(1),
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
    conn: Option<Arc<Connection>>,
    /// Held, never read: the bridge's callbacks spawn onto this runtime's
    /// handle, so it has to outlive the session and the connection that its
    /// reader and writer tasks live on.
    _client_rt: Runtime,
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
        let client_rt = runtime("lbfs-client");
        let (conn, limits, _root_attr) = client_rt
            .block_on(Connection::connect(
                server.addr,
                export.as_os_str().as_bytes(),
                opts.writeback,
            ))
            .expect("the client attaches to the export this test just exported");

        let fs = LbfsFuse::new(
            Arc::clone(&conn),
            client_rt.handle().clone(),
            opts.ttl,
            opts.writeback,
        );
        // The same option list the binary builds, from the same negotiated
        // ceiling: `max_read` has to agree with what the multiplexer will
        // accept or the kernel issues reads that come back `EINVAL`.
        let session =
            fuser::spawn_mount2(fs, &mnt, &mount_options(limits.max_io_size, false, false))
                .expect("the mount succeeds");

        let mounted = Loopback {
            session: Some(session),
            conn: Some(conn),
            _client_rt: client_rt,
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
    /// `spawn_mount2` returns once the mount syscall is done, but `INIT` runs
    /// afterwards on the session thread — and `init` is allowed to refuse,
    /// which ends the session and leaves an `ENOTCONN` mountpoint behind a
    /// perfectly successful `spawn_mount2`. Watching the session thread turns
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

    /// Unmount and wait for the session thread to finish.
    ///
    /// Joining is the point. Dropping the session alone unmounts and returns;
    /// the kernel's `FORGET`s for every evicted inode, the writeback of every
    /// dirty page and the final `DESTROY` all still have to cross the socket,
    /// and the thread that serves them is the one being joined. Anything
    /// asserting about what the mount left behind has to happen after this
    /// returns.
    ///
    /// **Every file opened on the mount must be closed first.** libfuse3
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
    /// `BackgroundSession::join` unwraps both the thread result and the
    /// session's `io::Result`.
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
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(move || session.join()));
            let _ = done.send(outcome.is_ok());
        });
        match ended.recv_timeout(UNMOUNT_TIMEOUT) {
            Ok(true) => true,
            Ok(false) => {
                eprintln!("lbfs loopback: the FUSE session ended badly; forcing the unmount");
                force_unmount(&self.mnt);
                !is_fuse_mount(&self.mnt)
            }
            Err(_) => false,
        }
    }

    /// Drop this side's reference to the connection, closing the socket.
    ///
    /// Separate from [`Loopback::unmount`] because the interesting assertions
    /// live between the two: after the unmount the session's `Arc` is gone but
    /// the socket is still open, which is the only moment at which the server's
    /// answer to "did every `FORGET` land?" is still observable.
    fn disconnect(&mut self) {
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
    std::fs::write(&exec, b"new").unwrap();
    assert_eq!(
        std::fs::metadata(&exec_real).unwrap().mode() & 0o7777,
        0o0775
    );

    let mark = lb.mnt().join("sgid-mand");
    let mark_real = lb.export().join("sgid-mand");
    std::fs::write(&mark, b"old").unwrap();
    std::fs::set_permissions(&mark, std::os::unix::fs::PermissionsExt::from_mode(0o2664)).unwrap();
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

fn set<const N: usize>(names: [&str; N]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

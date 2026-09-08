//! The shipped binary, end to end: mount, exercise, `SIGTERM`, clean exit.
//!
//! The bulk of spec §10 layer 3 lives in `lbfs-tests/tests/loopback.rs`, which
//! mounts `LbfsFuse` in process. That is the right shape for almost everything
//! — it can reach the connection, count the server's descriptors, and kill the
//! server without killing itself — but there is one thing it structurally
//! cannot test, because it does not run it: `main.rs`. Argument parsing, the
//! order the connection and the mount are started in, the signal handlers
//! installed in the window between them, and the drain on the way out are all
//! code that only exists in the binary, and all of it is code whose failure
//! mode is a mount left behind on somebody's machine.
//!
//! So this file is small and deliberately duplicative of a few assertions
//! elsewhere: what it is actually pinning is `lbfs-client <server> <export>
//! <mountpoint>`, `kill -TERM`, exit status 0, nothing still mounted.
//!
//! It lives here rather than beside the rest because `CARGO_BIN_EXE_*` is only
//! set for a test target in the package that builds the binary.

#![deny(unsafe_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lbfs_proto::frame::{DEFAULT_MAX_INFLIGHT, DEFAULT_MAX_IO_SIZE};
use lbfs_server::config::{Allowlist, Config, FsyncPolicy};

/// How long the child has to connect, mount and start answering.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long it has to unmount and exit after the signal.
const EXIT_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(10);

fn require_fuse() {
    assert!(
        Path::new("/dev/fuse").exists(),
        "this test runs the real client binary against a real mount and this \
         host has no /dev/fuse. Load the `fuse` module, or run the suite in the \
         VM (`make vm-test`)."
    );
    assert!(
        which("fusermount3").is_some(),
        "this test needs `fusermount3` on PATH: fuser's pure-Rust mount path \
         runs it for the mount and the unmount, so without it the client fails \
         to mount and this case reports only an exit code. Install fuse3."
    );
}

fn which(prog: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(prog))
        .find(|candidate| candidate.is_file())
}

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

fn force_unmount(mnt: &Path) {
    for args in [&["-u"][..], &["-u", "-z"][..]] {
        if !is_fuse_mount(mnt) {
            return;
        }
        let _ = Command::new("fusermount3").args(args).arg(mnt).status();
    }
}

/// A server on an OS-assigned port, serving `export`, on a runtime of its own.
fn serve(export: &Path) -> (tokio::runtime::Runtime, SocketAddr) {
    serve_with(export, FsyncPolicy::Honor)
}

/// The same, for a case whose subject is the durability policy.
fn serve_with(export: &Path, fsync: FsyncPolicy) -> (tokio::runtime::Runtime, SocketAddr) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let cfg = Config {
        listen: "127.0.0.1:0".to_string(),
        allowed_paths: vec![export.to_str().unwrap().to_string()],
        max_inflight: DEFAULT_MAX_INFLIGHT,
        max_io_size: DEFAULT_MAX_IO_SIZE,
        fsync,
        resume_grace: Duration::from_secs(60),
        max_resumable_sessions: 64,
    };
    let allow = Allowlist::new(&cfg.allowed_paths).unwrap();
    let listener = rt
        .block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await })
        .unwrap();
    let addr = listener.local_addr().unwrap();
    rt.spawn(async move {
        let _ = lbfs_server::rpc::serve(listener, Arc::new(cfg), Arc::new(allow)).await;
    });
    (rt, addr)
}

/// The child process and the mountpoint it owns.
///
/// The guard exists for the panic path: a test that fails between the mount and
/// the signal would otherwise leave both a running client and a live mount, and
/// the mount is the one that breaks every later run.
struct ClientProcess {
    child: Option<Child>,
    mnt: PathBuf,
}

impl ClientProcess {
    fn spawn(addr: SocketAddr, export: &Path, mnt: &Path) -> ClientProcess {
        // Inherited, so a failure in CI shows the client's own diagnosis
        // rather than only this test's assertion.
        ClientProcess::spawn_with(addr, export, mnt, &[], Stdio::inherit)
    }

    /// The same, letting the caller add flags and capture the client's log.
    ///
    /// The log is what two cases are actually about. The binary's shutdown
    /// reports what it did about the forced sync (spec §11) and reports it
    /// nowhere else — `syncfs` leaves no trace a test can stat for — and the
    /// handshake reports whether this mount is resumable, which nothing outside
    /// the process can see either.
    fn spawn_with(
        addr: SocketAddr,
        export: &Path,
        mnt: &Path,
        flags: &[&str],
        out: fn() -> Stdio,
    ) -> ClientProcess {
        let child = Command::new(env!("CARGO_BIN_EXE_lbfs-client"))
            .args(flags)
            .arg(addr.to_string())
            .arg(export)
            .arg(mnt)
            // `tracing_subscriber::fmt()` writes to stdout, so that is the
            // handle a case wanting the client's own log has to take.
            .stdout(out())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the lbfs-client binary runs");
        ClientProcess {
            child: Some(child),
            mnt: mnt.to_path_buf(),
        }
    }

    fn wait_until_mounted(&mut self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("the client exited with {status} before it mounted anything");
            }
            if is_fuse_mount(&self.mnt) && std::fs::read_dir(&self.mnt).is_ok() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the client did not mount {} within {READY_TIMEOUT:?}",
                self.mnt.display()
            );
            std::thread::sleep(POLL);
        }
    }

    /// `SIGTERM`, then wait for the exit status.
    ///
    /// This is the whole contract the binary offers an init system: a signal
    /// arrives, the mount comes down, dirty pages drain through a session and a
    /// socket that are both still open, and the process leaves with status 0.
    fn terminate(&mut self) -> std::process::ExitStatus {
        rustix::process::kill_process(
            rustix::process::Pid::from_child(self.child.as_ref().expect("still running")),
            rustix::process::Signal::TERM,
        )
        .expect("the client is signalled");
        self.wait_for_exit("exit after SIGTERM")
    }

    /// `SIGTERM`, then the client's whole log, for a case that asserts on it.
    ///
    /// The read comes first and the wait second, and that order is the point:
    /// the read returns when the child closes its stdout, which is when it
    /// exits, so this both collects the log and waits for the exit — and it
    /// cannot deadlock the way a wait-then-read would on a child still filling
    /// a pipe nobody is draining. Only usable on a process spawned with
    /// [`ClientProcess::spawn_with`] and a pipe.
    fn terminate_capturing(&mut self) -> String {
        use std::io::Read;

        rustix::process::kill_process(
            rustix::process::Pid::from_child(self.child.as_ref().expect("still running")),
            rustix::process::Signal::TERM,
        )
        .expect("the client is signalled");

        let mut out = self
            .child
            .as_mut()
            .expect("still running")
            .stdout
            .take()
            .expect("spawned with a piped stdout");
        let mut log = String::new();
        out.read_to_string(&mut log)
            .expect("the client's log reads");
        let status = self.wait_for_exit("exit after SIGTERM");
        assert!(status.success(), "the client exited with {status}:\n{log}");
        // Inherited output is what every other case relies on for diagnosis;
        // this one took the pipe, so it hands the log back to the terminal too.
        print!("{log}");
        log
    }

    /// Wait for the child to leave, or kill it and fail saying what it never
    /// did.
    ///
    /// Bounded rather than a plain `wait`, because every reason this child
    /// might not exit — a handshake that never completes, a signal handler that
    /// never fires — is a bug that would otherwise hang `make test-loopback`
    /// with no diagnosis instead of failing it with one.
    fn wait_for_exit(&mut self, what: &str) -> std::process::ExitStatus {
        let mut child = self.child.take().expect("still running");
        let deadline = Instant::now() + EXIT_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the client did not {what} within {EXIT_TIMEOUT:?}");
            }
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for ClientProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        force_unmount(&self.mnt);
        // Reported, never asserted. This runs on the panic path too, and a
        // panic raised while unwinding aborts the whole test binary — the
        // original failure's message with it. The surviving mount is worth
        // shouting about; it is not worth losing the reason the test failed.
        //
        // Nothing leaks the tempdir here the way the in-process suite does,
        // because the child is already dead by this line: a mount whose server
        // process is gone answers `ENOTCONN`, so the `remove_dir_all` behind
        // `TempDir` fails at the mountpoint rather than recursing through it
        // and deleting the export.
        if is_fuse_mount(&self.mnt) {
            eprintln!(
                "lbfs loopback: {} is STILL MOUNTED after every unmount \
                 attempt; unmount it by hand before running the suite again.",
                self.mnt.display()
            );
        }
    }
}

/// One tempdir holding both the export and the mountpoint, resolved.
fn workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let export = root.path().join("export");
    let mnt = root.path().join("mnt");
    std::fs::create_dir(&export).unwrap();
    std::fs::create_dir(&mnt).unwrap();
    // Resolved, because the server matches its allowlist against the path the
    // kernel reports for the descriptor it opened.
    let export = export.canonicalize().unwrap();
    let mnt = mnt.canonicalize().unwrap();
    (root, export, mnt)
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn the_binary_mounts_serves_and_unmounts_on_sigterm() {
    require_fuse();
    let (_root, export, mnt) = workspace();
    let (_server, addr) = serve(&export);

    let mut client = ClientProcess::spawn(addr, &export, &mnt);
    client.wait_until_mounted();

    // The sequence from the task brief, through the binary rather than the
    // library. Everything here is covered in more depth by the in-process
    // suite; what is new is that it is happening over a mount some other
    // process set up.
    std::fs::write(mnt.join("hello.txt"), "hello lbfs").unwrap();
    assert_eq!(
        std::fs::read_to_string(mnt.join("hello.txt")).unwrap(),
        "hello lbfs"
    );
    std::fs::create_dir(mnt.join("dir")).unwrap();
    std::fs::rename(mnt.join("hello.txt"), mnt.join("dir/hi.txt")).unwrap();
    std::os::unix::fs::symlink("dir/hi.txt", mnt.join("link")).unwrap();
    assert_eq!(
        std::fs::read_to_string(mnt.join("link")).unwrap(),
        "hello lbfs"
    );
    std::fs::hard_link(mnt.join("dir/hi.txt"), mnt.join("hard")).unwrap();
    assert_eq!(
        std::fs::metadata(mnt.join("hard")).unwrap().len(),
        "hello lbfs".len() as u64
    );

    let listed: Vec<String> = std::fs::read_dir(&mnt)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    for expected in ["dir", "link", "hard"] {
        assert!(
            listed.contains(&expected.to_string()),
            "listing: {listed:?}"
        );
    }

    std::fs::remove_file(mnt.join("hard")).unwrap();
    std::fs::remove_file(mnt.join("link")).unwrap();
    std::fs::remove_file(mnt.join("dir/hi.txt")).unwrap();
    std::fs::remove_dir(mnt.join("dir")).unwrap();

    // Server-side truth: the export tempdir is now empty.
    assert_eq!(std::fs::read_dir(&export).unwrap().count(), 0);

    let status = client.terminate();
    assert!(
        status.success(),
        "the client exited with {status} after SIGTERM"
    );
    assert!(
        !is_fuse_mount(&mnt),
        "the client exited cleanly but left its mount behind"
    );
    assert_eq!(
        std::fs::read_dir(&mnt).unwrap().count(),
        0,
        "the mountpoint is the empty directory it was before the mount"
    );
}

/// An export the server does not offer is an operator mistake, and the binary
/// has to say so and stop — not mount an empty directory whose every operation
/// answers `EIO`.
#[test]
#[ignore = "runs the real client binary; run with `make test-loopback`"]
fn the_binary_refuses_to_mount_an_export_the_server_does_not_offer() {
    let (root, export, mnt) = workspace();
    let (_server, addr) = serve(&export);

    let refused = root.path().join("not-exported");
    std::fs::create_dir(&refused).unwrap();
    // Spawned through the same guard as the mounting case, and waited for with
    // the same bound: a client that hung in `connect` would otherwise hang
    // `make test-loopback` rather than failing it.
    let mut client = ClientProcess::spawn(addr, &refused.canonicalize().unwrap(), &mnt);
    let status = client.wait_for_exit("exit after a refused ATTACH");

    assert!(!status.success(), "a denied ATTACH must not exit 0");
    assert!(
        !is_fuse_mount(&mnt),
        "the client mounted before it knew the export was refused"
    );
}

/// The readahead attempt runs, fails without privileges, and names the exact
/// command an operator needs (`docs/benchmarks/2026-08-28-readahead.md`).
///
/// The knob at `/sys/class/bdi/<dev>/read_ahead_kb` is `root:root` mode 644
/// and this suite runs unprivileged, so the write earns `EACCES` — which is
/// the case worth pinning: one WARN carrying the command, no second complaint,
/// no claim of success, and a mount that keeps serving I/O afterwards. The
/// `echo 1024` in the message is the negotiated 1 MiB `max_io_size` over 1024,
/// so the same line also proves the default derivation ran end to end.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn the_binary_warns_once_with_the_readahead_command_it_may_not_run() {
    require_fuse();
    if rustix::process::geteuid().is_root() {
        // Root would write the knob successfully and this case pins the
        // unprivileged path; the ordinary suite never runs as root.
        eprintln!("lbfs loopback: skipping the readahead WARN case under root");
        return;
    }
    let (_root, export, mnt) = workspace();
    let (_server, addr) = serve(&export);

    let mut client = ClientProcess::spawn_with(addr, &export, &mnt, &[], Stdio::piped);
    client.wait_until_mounted();

    // Non-fatal by observation: the mount serves reads and writes after the
    // attempt has already failed.
    std::fs::write(mnt.join("after.txt"), "still serving").unwrap();
    assert_eq!(
        std::fs::read_to_string(mnt.join("after.txt")).unwrap(),
        "still serving"
    );

    let log = client.terminate_capturing();
    let warns: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("WARN") && line.contains("read_ahead_kb"))
        .collect();
    assert_eq!(
        warns.len(),
        1,
        "exactly one readahead WARN; the log was:\n{log}"
    );
    assert!(
        warns[0].contains("echo 1024 | sudo tee /sys/class/bdi/"),
        "the WARN must carry the operator's command; the line was:\n{}",
        warns[0]
    );
    assert!(
        warns[0].contains("/read_ahead_kb"),
        "the command must name the knob; the line was:\n{}",
        warns[0]
    );
    // The success line is INFO and mentions the knob; the WARN above also says
    // "set the mount's readahead" (as "cannot set ..."), so the level is what
    // separates a claim of success from the complaint.
    assert!(
        !log.lines()
            .any(|line| line.contains("INFO") && line.contains("readahead")),
        "an unprivileged client must not claim it set the knob:\n{log}"
    );
    assert!(!is_fuse_mount(&mnt), "the client left its mount behind");
}

/// The binary forces a real sync of the export on its way out (spec §11).
///
/// The second of the control's two entry points, and the one no user-space test
/// can reach: nothing asks for it, so the only evidence it happened is the
/// client saying so. `syncfs(2)` changes nothing a later `stat` can see, and the
/// mount is gone by the time it runs — so the log is the witness, and the
/// server's acknowledgement is what the log is reporting. A line claiming the
/// sync appears only when the reply carried `FLAG_FORCE_SYNC`, which the server
/// sets only on the branch that made the syscall.
///
/// `fsync = "ignore"` on purpose: that is the policy under which every byte this
/// mount wrote is still dirty in the server's page cache when the unmount
/// finishes, and the exit sync is the last thing that will ever flush it.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn the_binary_forces_a_sync_of_the_export_before_it_exits() {
    require_fuse();
    let (_root, export, mnt) = workspace();
    let (_server, addr) = serve_with(&export, FsyncPolicy::Ignore);

    let mut client = ClientProcess::spawn_with(addr, &export, &mnt, &[], Stdio::piped);
    client.wait_until_mounted();

    // Data the policy leaves dirty on the server: written, never fsynced.
    std::fs::write(mnt.join("unsynced.txt"), "nobody called fsync").unwrap();

    let log = client.terminate_capturing();
    assert!(
        log.contains("forced a sync of the export before exit"),
        "the binary must force a sync on the way out; its log was:\n{log}"
    );
    // The failure modes have their own lines, and none of them may appear.
    for unwanted in [
        "does not implement the forced-sync control",
        "was not synced on the way out",
        "did not finish",
    ] {
        assert!(!log.contains(unwanted), "{unwanted:?} in:\n{log}");
    }
    assert!(!is_fuse_mount(&mnt), "the client left its mount behind");
    assert_eq!(
        std::fs::read_to_string(export.join("unsynced.txt")).unwrap(),
        "nobody called fsync"
    );
}

/// `--no-reconnect` restores what a lost server always cost: `EIO` at once,
/// then a clean unmount, with nothing asked of the server and nothing retained
/// by it.
///
/// The negative half of the resumption feature, and the one an operator falls
/// back on. The flag clears the handshake request as well as the deadline, so
/// there is no ticket, no retained session, and — the part this case measures —
/// no ten-second park in front of the first error. The mount here also runs
/// with `--attr-timeout 0`, for the reason the in-process twin gives: with
/// caching on the kernel would answer from its own copies and prove nothing
/// about the connection underneath.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn a_no_reconnect_mount_dies_with_its_server_as_it_always_has() {
    require_fuse();
    let (_root, export, mnt) = workspace();
    let (server, addr) = serve(&export);

    let mut client = ClientProcess::spawn_with(
        addr,
        &export,
        &mnt,
        &["--no-reconnect", "--attr-timeout", "0"],
        Stdio::piped,
    );
    client.wait_until_mounted();
    std::fs::write(mnt.join("before.txt"), "written while the server lived").unwrap();

    // The server vanishes with the mount still up.
    server.shutdown_timeout(Duration::from_secs(5));

    // The client notices at its own pace — the socket has to reach EOF and the
    // reader task has to mark the connection dead — so this is a bounded wait
    // for the first `EIO` rather than an immediate assertion.
    let started = Instant::now();
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    while errno_of(std::fs::metadata(mnt.join("never-existed"))) != Some(libc::EIO) {
        assert!(
            Instant::now() < deadline,
            "the mount never started answering EIO"
        );
        std::thread::sleep(POLL);
    }
    let settled = started.elapsed();
    assert!(
        settled < NO_PARK,
        "a --no-reconnect mount answered EIO only after {settled:?}, which is \
         the reconnect park this flag exists to remove"
    );
    assert_eq!(
        errno_of(std::fs::read(mnt.join("before.txt"))),
        Some(libc::EIO),
        "a name the kernel knows about still needs the server to open it"
    );

    let log = plain(&client.terminate_capturing());
    assert!(
        log.contains("resumable=false"),
        "--no-reconnect must not ask for a ticket; the log was:\n{log}"
    );
    assert!(
        !log.contains("re-attaching"),
        "--no-reconnect must not redial; the log was:\n{log}"
    );
    assert!(
        !is_fuse_mount(&mnt),
        "the mount whose server died could not be taken down"
    );
    assert_eq!(std::fs::read_dir(&mnt).unwrap().count(), 0);
}

/// How long the mount has to notice that its server is gone.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(20);

/// The bound that separates "noticed the socket died" from "waited out a
/// reconnect deadline". The deadline this binary defaults to is ten seconds.
const NO_PARK: Duration = Duration::from_secs(5);

/// The errno behind a failed filesystem call, or `None` if it succeeded.
fn errno_of<T>(result: std::io::Result<T>) -> Option<i32> {
    result.err().and_then(|e| e.raw_os_error())
}

/// The client's log with its colour codes taken out.
///
/// `tracing_subscriber::fmt` paints field names and values even when its output
/// is a pipe, which drops escape sequences between `resumable`, `=` and what it
/// settled on — so a test asserting on a field has to read past the paint. The
/// cases that assert on a whole message need no such thing, since a message is
/// one unbroken run of characters.
fn plain(log: &str) -> String {
    let mut out = String::with_capacity(log.len());
    let mut chars = log.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // A CSI sequence ends at its final byte, which is the only letter in
        // it; everything between is parameters this has no use for.
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

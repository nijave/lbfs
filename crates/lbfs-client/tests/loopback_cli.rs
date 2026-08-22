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
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let cfg = Config {
        listen: "127.0.0.1:0".to_string(),
        allowed_paths: vec![export.to_str().unwrap().to_string()],
        max_inflight: DEFAULT_MAX_INFLIGHT,
        max_io_size: DEFAULT_MAX_IO_SIZE,
        fsync: FsyncPolicy::Honor,
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
        let child = Command::new(env!("CARGO_BIN_EXE_lbfs-client"))
            .arg(addr.to_string())
            .arg(export)
            .arg(mnt)
            // Inherited, so a failure in CI shows the client's own diagnosis
            // rather than only this test's assertion.
            .stdout(Stdio::inherit())
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
        let mut child = self.child.take().expect("still running");
        rustix::process::kill_process(
            rustix::process::Pid::from_child(&child),
            rustix::process::Signal::TERM,
        )
        .expect("the client is signalled");

        let deadline = Instant::now() + EXIT_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the client did not exit within {EXIT_TIMEOUT:?} of SIGTERM");
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
        assert!(
            !is_fuse_mount(&self.mnt),
            "{} is still mounted; unmount it before running the suite again",
            self.mnt.display()
        );
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
    let status = Command::new(env!("CARGO_BIN_EXE_lbfs-client"))
        .arg(addr.to_string())
        .arg(refused.canonicalize().unwrap())
        .arg(&mnt)
        .status()
        .expect("the lbfs-client binary runs");

    assert!(!status.success(), "a denied ATTACH must not exit 0");
    assert!(
        !is_fuse_mount(&mnt),
        "the client mounted before it knew the export was refused"
    );
}

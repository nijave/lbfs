//! The multiplexer against a real `lbfs-server`, in process.
//!
//! `mux.rs` proves the machinery — correlation, the window, what happens to a
//! frame the protocol forbids — against a server the test writes by hand. This
//! file answers the question that one structurally cannot: does the client
//! agree with the *actual* server about what a request looks like?
//!
//! That matters most for the typed call methods. Each one picks an opcode and
//! a reply type by hand, and a `LISTXATTR` wrapper that sent `Opcode::Getxattr`
//! would pass every scripted test in the suite, because the script answers
//! whatever the test told it to. Here the server decides, so a wrong opcode, a
//! misnamed field or a reply decoded as the wrong struct fails immediately.
//!
//! No FUSE and no VM: this is still a socket and a tempdir (spec §10 layers
//! 1–2). The mount itself proves out in Task 16.

#![deny(unsafe_code)]

use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::time::Duration;

use lbfs_client::conn::Connection;
use lbfs_proto::frame::{DEFAULT_MAX_INFLIGHT, DEFAULT_MAX_IO_SIZE};
use lbfs_proto::ops::CopyFileRangeRequest;
use lbfs_proto::types::{SetattrArgs, TimeSet, ROOT_NODE};
use lbfs_proto::Errno;
use lbfs_server::config::{Allowlist, Config, FsyncPolicy};
use lbfs_server::rpc::{run_session, Server};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// A real server, serving one tempdir, for exactly one client.
///
/// One client because the session task is held here so a test can end it: the
/// accept loop in `rpc::serve` spawns sessions it does not hand back, and
/// killing a session is the only honest way to make a live server disappear
/// from under a connected client.
struct Live {
    addr: SocketAddr,
    export: tempfile::TempDir,
    session: JoinHandle<()>,
}

impl Live {
    async fn start() -> Live {
        let export = tempfile::tempdir().unwrap();
        // The allowlist is matched against the path the server resolves from
        // its own descriptor, so an unresolved `/tmp` symlink would be denied.
        let resolved = export.path().canonicalize().unwrap();
        let cfg = Config {
            listen: "127.0.0.1:0".to_string(),
            allowed_paths: vec![resolved.to_str().unwrap().to_string()],
            max_inflight: DEFAULT_MAX_INFLIGHT,
            max_io_size: DEFAULT_MAX_IO_SIZE,
            fsync: FsyncPolicy::Honor,
        };
        let allow = Allowlist::new(&cfg.allowed_paths).unwrap();
        let server = Server::new(Arc::new(cfg), Arc::new(allow)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let session = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            sock.set_nodelay(true).unwrap();
            run_session(sock, server).await;
        });
        Live {
            addr,
            export,
            session,
        }
    }

    /// The export path as the wire carries it: bytes, never text.
    fn export_path(&self) -> Vec<u8> {
        self.export
            .path()
            .canonicalize()
            .unwrap()
            .as_os_str()
            .as_bytes()
            .to_vec()
    }

    fn join(&self, name: &str) -> std::path::PathBuf {
        self.export.path().join(name)
    }

    async fn attach(&self) -> Arc<Connection> {
        let (conn, hello, root) = Connection::connect(self.addr, &self.export_path(), true)
            .await
            .expect("the handshake succeeds against a real server");
        assert_eq!(hello.version, lbfs_proto::frame::PROTOCOL_VERSION);
        assert_eq!(hello.max_inflight, DEFAULT_MAX_INFLIGHT);
        assert_eq!(hello.max_io_size, DEFAULT_MAX_IO_SIZE);
        assert!(
            root.mode & libc::S_IFMT == libc::S_IFDIR,
            "ATTACH reports the export root's own attributes"
        );
        conn
    }
}

#[tokio::test]
async fn connect_lookup_getattr_read_and_write() {
    let live = Live::start().await;
    std::fs::write(live.join("f"), b"xyz").unwrap();
    let conn = live.attach().await;

    let entry = conn.lookup(ROOT_NODE, b"f").await.unwrap();
    assert_eq!(entry.attr.size, 3);
    let attr = conn.getattr(entry.node, None).await.unwrap();
    assert_eq!(attr.size, 3);
    assert_eq!(attr.ino, entry.attr.ino);

    let fh = conn.open(entry.node, libc::O_RDWR as u32).await.unwrap();
    assert_eq!(conn.read(entry.node, fh, 0, 4096).await.unwrap(), b"xyz");
    let written = conn.write(entry.node, fh, 3, b"!!".to_vec()).await.unwrap();
    assert_eq!(written, 2);
    conn.fsync(entry.node, fh, false).await.unwrap();
    conn.flush(entry.node, fh).await.unwrap();
    conn.release(entry.node, fh).await.unwrap();
    assert_eq!(std::fs::read(live.join("f")).unwrap(), b"xyz!!");

    // A name that is not there is an errno, not a dead connection.
    assert_eq!(
        conn.lookup(ROOT_NODE, b"absent").await.unwrap_err(),
        Errno::ENOENT
    );
    assert!(!conn.is_dead());
}

#[tokio::test]
async fn concurrent_calls_multiplex_over_one_socket() {
    let live = Live::start().await;
    for i in 0..64 {
        std::fs::write(live.join(&format!("f{i:02}")), vec![b'x'; i]).unwrap();
    }
    let conn = live.attach().await;

    // Sixty-four lookups down one socket, all outstanding at once. The window
    // is 128 by default, so none of them waits for a permit and the server is
    // free to answer in whatever order its handlers finish.
    let mut calls = Vec::new();
    for i in 0..64usize {
        let conn = Arc::clone(&conn);
        calls.push(tokio::spawn(async move {
            (
                i,
                conn.lookup(ROOT_NODE, format!("f{i:02}").as_bytes()).await,
            )
        }));
    }
    for call in calls {
        let (i, entry) = call.await.unwrap();
        let entry = entry.unwrap_or_else(|e| panic!("lookup f{i:02} failed: {e:?}"));
        assert_eq!(entry.attr.size, i as u64, "f{i:02} got another's reply");
    }
}

#[tokio::test]
async fn forget_batches_and_the_server_forgets() {
    let live = Live::start().await;
    std::fs::write(live.join("f"), b"xyz").unwrap();
    let conn = live.attach().await;

    let entry = conn.lookup(ROOT_NODE, b"f").await.unwrap();
    assert!(conn.getattr(entry.node, None).await.is_ok());

    // Queued, not sent: the batcher holds it for the flush timer. Nothing here
    // waits for a reply, because a FORGET has none.
    conn.send_forget(entry.node, 1);
    tokio::time::sleep(Duration::from_millis(800)).await;

    // The node's one lookup count is gone, so the server no longer knows it.
    assert_eq!(
        conn.getattr(entry.node, None).await.unwrap_err(),
        Errno::ESTALE
    );
    assert!(
        !conn.is_dead(),
        "a forget costs no window permit and no session"
    );

    // And the connection is still good for a fresh lookup of the same name.
    let again = conn.lookup(ROOT_NODE, b"f").await.unwrap();
    assert_eq!(again.attr.size, 3);
}

#[tokio::test]
async fn a_dead_server_turns_every_later_call_into_eio() {
    let live = Live::start().await;
    std::fs::write(live.join("f"), b"xyz").unwrap();
    let conn = live.attach().await;
    assert!(conn.lookup(ROOT_NODE, b"f").await.is_ok());

    // The session task owns the socket; aborting it is a server that went away
    // without a word, which is what a crash or a power cut looks like from
    // here. There is no reconnect in v1 (spec §7).
    live.session.abort();

    // The client learns from its own reader task, so give it a moment.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !conn.is_dead() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(conn.is_dead(), "the client must notice a vanished server");

    assert_eq!(conn.lookup(ROOT_NODE, b"f").await.unwrap_err(), Errno::EIO);
    assert_eq!(conn.getattr(ROOT_NODE, None).await.unwrap_err(), Errno::EIO);
    assert_eq!(conn.statfs(ROOT_NODE).await.unwrap_err(), Errno::EIO);
    // And a forget still returns rather than blocking a kernel callback.
    conn.send_forget(2, 1);
}

#[tokio::test]
async fn every_typed_call_reaches_its_own_opcode() {
    let live = Live::start().await;
    let conn = live.attach().await;

    // --- create, write, read, lseek, fallocate, copy_file_range ------------
    let (file, fh) = conn
        .create(ROOT_NODE, b"a", 0o644, libc::O_RDWR as u32)
        .await
        .unwrap();
    assert_eq!(
        conn.write(file.node, fh, 0, b"hello".to_vec())
            .await
            .unwrap(),
        5
    );
    assert_eq!(conn.read(file.node, fh, 0, 4096).await.unwrap(), b"hello");
    assert_eq!(
        conn.lseek(file.node, fh, 0, libc::SEEK_END as u32)
            .await
            .unwrap(),
        5
    );
    conn.fallocate(file.node, fh, 0, 16, 0).await.unwrap();
    assert_eq!(conn.getattr(file.node, Some(fh)).await.unwrap().size, 16);

    let (copy, copy_fh) = conn
        .create(ROOT_NODE, b"b", 0o644, libc::O_RDWR as u32)
        .await
        .unwrap();
    let copied = conn
        .copy_file_range(&CopyFileRangeRequest {
            node_in: file.node,
            fh_in: fh,
            off_in: 0,
            node_out: copy.node,
            fh_out: copy_fh,
            off_out: 0,
            len: 5,
        })
        .await
        .unwrap();
    assert_eq!(copied, 5);
    assert_eq!(conn.read(copy.node, copy_fh, 0, 5).await.unwrap(), b"hello");
    conn.release(copy.node, copy_fh).await.unwrap();

    // --- setattr -----------------------------------------------------------
    let attr = conn
        .setattr(
            file.node,
            SetattrArgs {
                mode: Some(0o600),
                uid: None,
                gid: None,
                size: Some(5),
                atime: TimeSet::Omit,
                mtime: TimeSet::Set { sec: 1, nsec: 2 },
                fh: Some(fh),
            },
        )
        .await
        .unwrap();
    assert_eq!(attr.mode & 0o777, 0o600);
    assert_eq!(attr.size, 5);
    assert_eq!((attr.mtime_sec, attr.mtime_nsec), (1, 2));
    conn.release(file.node, fh).await.unwrap();

    // --- links and symlinks ------------------------------------------------
    let link = conn.link(file.node, ROOT_NODE, b"a-hard").await.unwrap();
    assert_eq!(link.attr.nlink, 2);
    let sym = conn.symlink(ROOT_NODE, b"a-soft", b"a").await.unwrap();
    assert_eq!(conn.readlink(sym.node).await.unwrap(), b"a");

    // --- directories -------------------------------------------------------
    let dir = conn.mkdir(ROOT_NODE, b"d", 0o755).await.unwrap();
    let dh = conn.opendir(dir.node).await.unwrap();
    let page = conn.readdir(dir.node, dh, 0, 1 << 20).await.unwrap();
    let names: Vec<&[u8]> = page.entries.iter().map(|e| e.name.as_slice()).collect();
    assert!(page.end && names.contains(&b".".as_slice()) && names.contains(&b"..".as_slice()));
    assert!(
        page.entries.iter().all(|e| e.ino != 0),
        "a zero d_ino is a dirent glibc drops"
    );
    conn.fsyncdir(dir.node, dh, false).await.unwrap();
    conn.releasedir(dir.node, dh).await.unwrap();

    let root_dh = conn.opendir(ROOT_NODE).await.unwrap();
    let plus = conn
        .readdirplus(ROOT_NODE, root_dh, 0, 1 << 20)
        .await
        .unwrap();
    let listed: Vec<&[u8]> = plus.entries.iter().map(|e| e.name.as_slice()).collect();
    for want in [b"a".as_slice(), b"a-hard", b"a-soft", b"b", b"d"] {
        assert!(listed.contains(&want), "readdirplus missed {want:?}");
    }
    // Every entry READDIRPLUS registered owes a FORGET; the batcher makes that
    // one frame rather than one per name.
    for entry in &plus.entries {
        if entry.entry.node != 0 {
            conn.send_forget(entry.entry.node, 1);
        }
    }
    conn.releasedir(ROOT_NODE, root_dh).await.unwrap();

    // --- statfs ------------------------------------------------------------
    let stat = conn.statfs(ROOT_NODE).await.unwrap();
    assert!(stat.bsize > 0 && stat.namelen > 0);

    // --- xattrs ------------------------------------------------------------
    // tmpfs has carried `user.*` xattrs since 6.6, but an export on a
    // filesystem that has not is a skip rather than a failure - the point here
    // is which opcode each wrapper sends, not what the backend supports.
    match conn.setxattr(file.node, b"user.k", b"v".to_vec(), 0).await {
        Ok(()) => {
            assert_eq!(
                conn.getxattr(file.node, b"user.k", 0).await.unwrap(),
                (1, Vec::new()),
                "size == 0 asks only for the length"
            );
            let (size, value) = conn.getxattr(file.node, b"user.k", 64).await.unwrap();
            assert_eq!((size, value), (1, b"v".to_vec()));
            let (_, names) = conn.listxattr(file.node, 256).await.unwrap();
            assert!(names.windows(6).any(|w| w == b"user.k"));
            conn.removexattr(file.node, b"user.k").await.unwrap();
            assert_eq!(
                conn.getxattr(file.node, b"user.k", 64).await.unwrap_err(),
                Errno::ENODATA
            );
        }
        Err(e) => eprintln!("skipping the xattr sweep: SETXATTR answered {e:?}"),
    }

    // --- rename, unlink, rmdir ---------------------------------------------
    conn.rename(ROOT_NODE, b"b", ROOT_NODE, b"b2", 0)
        .await
        .unwrap();
    assert_eq!(
        conn.lookup(ROOT_NODE, b"b").await.unwrap_err(),
        Errno::ENOENT
    );
    conn.unlink(ROOT_NODE, b"b2").await.unwrap();
    conn.unlink(ROOT_NODE, b"a-hard").await.unwrap();
    conn.unlink(ROOT_NODE, b"a-soft").await.unwrap();
    conn.unlink(ROOT_NODE, b"a").await.unwrap();
    conn.rmdir(ROOT_NODE, b"d").await.unwrap();
    let dh = conn.opendir(ROOT_NODE).await.unwrap();
    let page = conn.readdir(ROOT_NODE, dh, 0, 1 << 20).await.unwrap();
    assert_eq!(page.entries.len(), 2, "only . and .. are left");
    conn.releasedir(ROOT_NODE, dh).await.unwrap();

    assert!(
        !conn.is_dead(),
        "thirty opcodes and the session is still up"
    );
}

//! The server speaking raw frames over a real TCP socket.
//!
//! This is the smoke path for the RPC layer: handshake, attach, a request of
//! each shape (metadata, bulk read, bulk write, no-reply), and the four ways a
//! session dies. The exhaustive per-opcode matrix belongs to the loopback
//! suite; what is pinned here is the frame plumbing itself — correlation ids
//! that survive out-of-order completion, statuses that reject rather than
//! serve, and the violations that must close the connection instead of
//! answering.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use lbfs_proto::frame::{
    FrameHeader, FLAG_NO_REPLY, MAGIC, MAX_BODY_SIZE, PROTOCOL_VERSION, STATUS_ATTACH_DENIED,
    STATUS_NOT_EXPORTED, STATUS_OK, STATUS_VERSION_MISMATCH,
};
use lbfs_proto::io::{read_body, read_header, write_frame};
use lbfs_proto::ops::{
    AttachReply, AttachRequest, ForgetRequest, GetattrRequest, GetxattrRequest, HelloReply,
    HelloRequest, LookupRequest, Opcode, OpenReply, OpenRequest, ReadRequest, ReleaseRequest,
    SetxattrRequest, WriteReply, WriteRequest,
};
use lbfs_proto::types::{Entry, FileAttr, XattrReply, ROOT_NODE};
use lbfs_server::config::{Allowlist, Config, FsyncPolicy};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Start a server on an OS-assigned port with `patterns` as its allowlist.
///
/// Port 0 rather than 9423: the suite must not collide with a running server
/// or with a sibling test.
async fn start_server(patterns: Vec<String>) -> SocketAddr {
    let cfg = Config {
        listen: "127.0.0.1:0".to_string(),
        allowed_paths: patterns,
        max_inflight: 128,
        max_io_size: 1 << 20,
        fsync: FsyncPolicy::Honor,
    };
    let allow = Allowlist::new(&cfg.allowed_paths).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = lbfs_server::rpc::serve(listener, Arc::new(cfg), Arc::new(allow)).await;
    });
    addr
}

/// The common case: export one directory, allowlisted by its exact path.
async fn start_server_for(export: &Path) -> SocketAddr {
    start_server(vec![resolved(export)]).await
}

/// A path as the kernel will report it for the descriptor the server opens.
fn resolved(p: &Path) -> String {
    p.canonicalize().unwrap().to_str().unwrap().to_string()
}

fn path_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

async fn send(s: &mut TcpStream, id: u64, op: u16, flags: u16, body: &[u8], data: &[u8]) {
    let hdr = FrameHeader {
        request_id: id,
        op_or_status: op,
        flags,
        body_len: body.len() as u32,
        data_len: data.len() as u32,
    };
    write_frame(s, hdr, body, data).await.unwrap();
}

async fn call(s: &mut TcpStream, id: u64, op: Opcode, body: &[u8]) -> (FrameHeader, Vec<u8>) {
    send(s, id, op as u16, 0, body, &[]).await;
    let (hdr, body, _) = recv(s).await;
    assert_eq!(hdr.request_id, id);
    (hdr, body)
}

async fn recv(s: &mut TcpStream) -> (FrameHeader, Vec<u8>, Vec<u8>) {
    let hdr = read_header(s).await.unwrap();
    let body = read_body(s, hdr.body_len, MAX_BODY_SIZE).await.unwrap();
    let mut data = vec![0u8; hdr.data_len as usize];
    s.read_exact(&mut data).await.unwrap();
    (hdr, body, data)
}

fn enc<T: serde::Serialize>(v: &T) -> Vec<u8> {
    postcard::to_allocvec(v).unwrap()
}

fn dec<T: serde::de::DeserializeOwned>(b: &[u8]) -> T {
    postcard::from_bytes(b).unwrap()
}

fn hello_body(version: u32, max_io_size: u32) -> Vec<u8> {
    enc(&HelloRequest {
        magic: MAGIC,
        version,
        max_inflight: 128,
        max_io_size,
        writeback: false,
    })
}

/// HELLO then ATTACH, both expected to succeed.
async fn hello_attach(s: &mut TcpStream, path: &Path) -> (HelloReply, AttachReply) {
    let (hdr, body) = call(s, 1, Opcode::Hello, &hello_body(PROTOCOL_VERSION, 1 << 20)).await;
    assert_eq!(hdr.op_or_status, STATUS_OK);
    let hello: HelloReply = dec(&body);

    let attach = enc(&AttachRequest {
        path: path_bytes(path),
    });
    let (hdr, body) = call(s, 2, Opcode::Attach, &attach).await;
    assert_eq!(hdr.op_or_status, STATUS_OK, "attach rejected");
    (hello, dec(&body))
}

/// Whether the server has closed its side.
async fn at_eof(s: &mut TcpStream) -> bool {
    let mut byte = [0u8; 1];
    matches!(s.read(&mut byte).await, Ok(0))
}

/// Read until the peer is gone, however it goes, and report how much arrived.
async fn drain_until_close(s: &mut TcpStream) -> usize {
    let mut buf = vec![0u8; 64 << 10];
    let mut total = 0;
    loop {
        match s.read(&mut buf).await {
            Ok(0) | Err(_) => return total,
            Ok(n) => total += n,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hello_attach_lookup_read_write_over_tcp() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), b"hello world").unwrap();
    let addr = start_server_for(dir.path()).await;
    let mut s = TcpStream::connect(addr).await.unwrap();

    let (hello, attach) = hello_attach(&mut s, dir.path()).await;
    assert_eq!(hello.version, PROTOCOL_VERSION);
    assert_eq!(hello.max_inflight, 128);
    assert_eq!(hello.max_io_size, 1 << 20);
    assert_eq!(hello.max_body_size, MAX_BODY_SIZE);
    assert_eq!(attach.root_attr.mode & libc::S_IFMT, libc::S_IFDIR);

    // LOOKUP
    let body = enc(&LookupRequest {
        parent: ROOT_NODE,
        name: b"f".to_vec(),
    });
    let (hdr, body) = call(&mut s, 3, Opcode::Lookup, &body).await;
    assert_eq!(hdr.op_or_status, STATUS_OK);
    let entry: Entry = dec(&body);
    assert_eq!(entry.attr.size, 11);

    // OPEN
    let body = enc(&OpenRequest {
        node: entry.node,
        flags: libc::O_RDWR as u32,
    });
    let (hdr, body) = call(&mut s, 4, Opcode::Open, &body).await;
    assert_eq!(hdr.op_or_status, STATUS_OK);
    let open: OpenReply = dec(&body);

    // READ and GETATTR back to back: both answers must arrive, each matched to
    // its own request id, whichever order they complete in.
    let read = enc(&ReadRequest {
        node: entry.node,
        fh: open.fh,
        offset: 0,
        size: 4096,
    });
    let getattr = enc(&GetattrRequest {
        node: ROOT_NODE,
        fh: None,
    });
    send(&mut s, 10, Opcode::Read as u16, 0, &read, &[]).await;
    send(&mut s, 11, Opcode::Getattr as u16, 0, &getattr, &[]).await;
    let mut seen = Vec::new();
    for _ in 0..2 {
        let (hdr, body, data) = recv(&mut s).await;
        assert_eq!(hdr.op_or_status, STATUS_OK);
        match hdr.request_id {
            10 => {
                assert_eq!(data, b"hello world");
                assert_eq!(hdr.body_len, 0, "READ answers in the data segment");
            }
            11 => {
                let attr: FileAttr = dec(&body);
                assert_eq!(attr.mode & libc::S_IFMT, libc::S_IFDIR);
            }
            other => panic!("unexpected request_id {other}"),
        }
        seen.push(hdr.request_id);
    }
    seen.sort_unstable();
    assert_eq!(seen, vec![10, 11]);

    // WRITE, with the payload in the data segment.
    let body = enc(&WriteRequest {
        node: entry.node,
        fh: open.fh,
        offset: 11,
    });
    send(&mut s, 12, Opcode::Write as u16, 0, &body, b"!!").await;
    let (hdr, body, _) = recv(&mut s).await;
    assert_eq!((hdr.request_id, hdr.op_or_status), (12, STATUS_OK));
    let written: WriteReply = dec(&body);
    assert_eq!(written.written, 2);
    assert_eq!(
        std::fs::read(dir.path().join("f")).unwrap(),
        b"hello world!!"
    );

    // FORGET carries NO_REPLY: the next frame back must be the GETATTR's, not
    // an acknowledgement of the forget.
    let body = enc(&ForgetRequest {
        items: vec![(entry.node, 1)],
    });
    send(&mut s, 13, Opcode::Forget as u16, FLAG_NO_REPLY, &body, &[]).await;
    let (hdr, _) = call(&mut s, 14, Opcode::Getattr, &getattr).await;
    assert_eq!((hdr.request_id, hdr.op_or_status), (14, STATUS_OK));

    // RELEASE still works against the handle after the forget.
    let body = enc(&ReleaseRequest {
        node: entry.node,
        fh: open.fh,
    });
    let (hdr, body) = call(&mut s, 15, Opcode::Release, &body).await;
    assert_eq!(hdr.op_or_status, STATUS_OK);
    assert!(body.is_empty());
}

#[tokio::test]
async fn attach_denied_not_exported_and_version_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let exports = dir.path().join("exports");
    let outside = dir.path().join("secret");
    std::fs::create_dir_all(&exports).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let addr = start_server(vec![resolved(&exports)]).await;

    // A real directory that no pattern matches.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let (hdr, _) = call(
        &mut s,
        1,
        Opcode::Hello,
        &hello_body(PROTOCOL_VERSION, 1 << 20),
    )
    .await;
    assert_eq!(hdr.op_or_status, STATUS_OK);
    let body = enc(&AttachRequest {
        path: path_bytes(&outside),
    });
    let (hdr, _) = call(&mut s, 2, Opcode::Attach, &body).await;
    assert_eq!(hdr.op_or_status, STATUS_ATTACH_DENIED);
    assert!(at_eof(&mut s).await, "server closes after a denied attach");

    // A path that does not exist at all.
    let mut s = TcpStream::connect(addr).await.unwrap();
    call(
        &mut s,
        1,
        Opcode::Hello,
        &hello_body(PROTOCOL_VERSION, 1 << 20),
    )
    .await;
    let body = enc(&AttachRequest {
        path: path_bytes(&exports.join("nope")),
    });
    let (hdr, _) = call(&mut s, 2, Opcode::Attach, &body).await;
    assert_eq!(hdr.op_or_status, STATUS_NOT_EXPORTED);
    assert!(at_eof(&mut s).await);

    // A version this server does not speak.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let (hdr, body) = call(&mut s, 1, Opcode::Hello, &hello_body(999, 1 << 20)).await;
    assert_eq!(hdr.op_or_status, STATUS_VERSION_MISMATCH);
    assert!(body.is_empty());
    assert!(
        at_eof(&mut s).await,
        "server closes after a version mismatch"
    );
}

/// The allowlist is matched against the *descriptor's* resolved path, not the
/// path the client asked for (spec §3.2 step 3, §4).
#[tokio::test]
async fn attach_matches_the_resolved_descriptor_path() {
    let dir = tempfile::tempdir().unwrap();
    let exports = dir.path().join("exports");
    let outside = dir.path().join("secret");
    std::fs::create_dir_all(exports.join("data")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    // A symlink sitting inside the exported tree but pointing out of it.
    std::os::unix::fs::symlink(&outside, exports.join("leak")).unwrap();
    // A symlink outside the tree pointing into it.
    std::os::unix::fs::symlink(exports.join("data"), dir.path().join("way-in")).unwrap();

    let addr = start_server(vec![format!("{}/*", resolved(&exports))]).await;

    // Resolves to .../secret, which no pattern matches.
    let mut s = TcpStream::connect(addr).await.unwrap();
    call(
        &mut s,
        1,
        Opcode::Hello,
        &hello_body(PROTOCOL_VERSION, 1 << 20),
    )
    .await;
    let body = enc(&AttachRequest {
        path: path_bytes(&exports.join("leak")),
    });
    let (hdr, _) = call(&mut s, 2, Opcode::Attach, &body).await;
    assert_eq!(hdr.op_or_status, STATUS_ATTACH_DENIED);

    // Resolves to .../exports/data, which does: the requested path being
    // outside the allowlist is not what the check is about.
    let mut s = TcpStream::connect(addr).await.unwrap();
    hello_attach(&mut s, &dir.path().join("way-in")).await;
}

#[tokio::test]
async fn unknown_opcode_closes_connection() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start_server_for(dir.path()).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    hello_attach(&mut s, dir.path()).await;

    send(&mut s, 3, 9999, 0, &[], &[]).await;
    assert!(at_eof(&mut s).await, "unknown opcode is connection-fatal");
}

/// More requests in flight than the settled window is a violation.
///
/// Getting there deterministically takes a client that refuses to read: the
/// permits are released as replies reach the socket, so the only way to hold
/// them is to stop the replies from draining. Megabyte reads and a window of
/// eight put far more bytes in the writer than any socket buffer will take, so
/// the permits stay held while the requests keep arriving.
#[tokio::test]
async fn window_overrun_closes_connection() {
    const READ_SIZE: usize = 1 << 20;
    const BURST: u64 = 32;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big"), vec![7u8; READ_SIZE]).unwrap();
    let addr = start_server_for(dir.path()).await;
    let mut s = TcpStream::connect(addr).await.unwrap();

    // The smallest window the protocol allows, so the burst does not have to
    // be enormous.
    let hello = enc(&HelloRequest {
        magic: MAGIC,
        version: PROTOCOL_VERSION,
        max_inflight: 8,
        max_io_size: READ_SIZE as u32,
        writeback: false,
    });
    let (hdr, body) = call(&mut s, 1, Opcode::Hello, &hello).await;
    assert_eq!(hdr.op_or_status, STATUS_OK);
    let hello: HelloReply = dec(&body);
    assert_eq!(hello.max_inflight, 8);

    let attach = enc(&AttachRequest {
        path: path_bytes(dir.path()),
    });
    call(&mut s, 2, Opcode::Attach, &attach).await;
    let body = enc(&LookupRequest {
        parent: ROOT_NODE,
        name: b"big".to_vec(),
    });
    let (_, body) = call(&mut s, 3, Opcode::Lookup, &body).await;
    let entry: Entry = dec(&body);
    let body = enc(&OpenRequest {
        node: entry.node,
        flags: libc::O_RDONLY as u32,
    });
    let (_, body) = call(&mut s, 4, Opcode::Open, &body).await;
    let open: OpenReply = dec(&body);

    let read = enc(&ReadRequest {
        node: entry.node,
        fh: open.fh,
        offset: 0,
        size: READ_SIZE as u32,
    });
    for id in 100..100 + BURST {
        send(&mut s, id, Opcode::Read as u16, 0, &read, &[]).await;
    }

    // Read until the server goes away. It may go away as a reset rather than
    // an orderly EOF: closing a socket that still holds unread requests is
    // what makes the kernel send RST, and after a violation there are always
    // unread requests. Either way the connection is gone, and that it is gone
    // with reads unanswered is the assertion.
    let got = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        drain_until_close(&mut s),
    )
    .await
    .expect("server must close the connection on a window overrun");
    // Half the burst is a deliberately generous bound. The reader admits its
    // eight and hits the overrun before a single reply can be written, so the
    // true figure is nearer one read's worth; anything up to half still
    // falsifies a window that was not enforced at all.
    assert!(
        got <= (BURST / 2) as usize * READ_SIZE,
        "the window let too much of the burst through: {got} bytes"
    );
}

/// A READ asking for more than the negotiated maximum is a violation, checked
/// before anything is allocated for it (spec §3.1).
#[tokio::test]
async fn oversize_read_closes_connection() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), b"x").unwrap();
    let addr = start_server_for(dir.path()).await;
    let mut s = TcpStream::connect(addr).await.unwrap();

    // Propose a small I/O size; the settled value is the smaller of the two.
    let (hdr, body) = call(
        &mut s,
        1,
        Opcode::Hello,
        &hello_body(PROTOCOL_VERSION, 8192),
    )
    .await;
    assert_eq!(hdr.op_or_status, STATUS_OK);
    let hello: HelloReply = dec(&body);
    assert_eq!(hello.max_io_size, 8192);
    let attach = enc(&AttachRequest {
        path: path_bytes(dir.path()),
    });
    let (hdr, _) = call(&mut s, 2, Opcode::Attach, &attach).await;
    assert_eq!(hdr.op_or_status, STATUS_OK);

    let body = enc(&ReadRequest {
        node: ROOT_NODE,
        fh: 1,
        offset: 0,
        size: 1 << 20,
    });
    send(&mut s, 3, Opcode::Read as u16, 0, &body, &[]).await;
    assert!(at_eof(&mut s).await, "oversize READ is connection-fatal");
}

/// A WRITE whose data segment is larger than the negotiated maximum dies
/// before the server reads a byte of it.
#[tokio::test]
async fn oversize_write_data_closes_connection() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start_server_for(dir.path()).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    let (hdr, body) = call(
        &mut s,
        1,
        Opcode::Hello,
        &hello_body(PROTOCOL_VERSION, 4096),
    )
    .await;
    assert_eq!(hdr.op_or_status, STATUS_OK);
    let hello: HelloReply = dec(&body);
    assert_eq!(hello.max_io_size, 4096);
    let attach = enc(&AttachRequest {
        path: path_bytes(dir.path()),
    });
    call(&mut s, 2, Opcode::Attach, &attach).await;

    let body = enc(&WriteRequest {
        node: ROOT_NODE,
        fh: 1,
        offset: 0,
    });
    send(&mut s, 3, Opcode::Write as u16, 0, &body, &vec![0u8; 8192]).await;
    assert!(at_eof(&mut s).await, "oversize data is connection-fatal");
}

/// An xattr value is bounded by the max body size, not by the negotiated I/O
/// size — symmetrically with the GETXATTR reply the same session can receive.
#[tokio::test]
async fn a_large_xattr_survives_a_small_negotiated_io_size() {
    const VALUE: usize = 16 << 10;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), b"body").unwrap();
    let addr = start_server_for(dir.path()).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    let (hdr, body) = call(
        &mut s,
        1,
        Opcode::Hello,
        &hello_body(PROTOCOL_VERSION, 4096),
    )
    .await;
    assert_eq!(hdr.op_or_status, STATUS_OK);
    let hello: HelloReply = dec(&body);
    assert_eq!(hello.max_io_size, 4096);
    let attach = enc(&AttachRequest {
        path: path_bytes(dir.path()),
    });
    call(&mut s, 2, Opcode::Attach, &attach).await;
    let body = enc(&LookupRequest {
        parent: ROOT_NODE,
        name: b"x".to_vec(),
    });
    let (_, body) = call(&mut s, 3, Opcode::Lookup, &body).await;
    let entry: Entry = dec(&body);

    let value = vec![b'v'; VALUE];
    let body = enc(&SetxattrRequest {
        node: entry.node,
        name: b"user.big".to_vec(),
        flags: 0,
    });
    send(&mut s, 4, Opcode::Setxattr as u16, 0, &body, &value).await;
    let (hdr, _, _) = recv(&mut s).await;
    assert_eq!(hdr.request_id, 4, "the connection must survive the value");
    assert!(
        hdr.op_or_status < 0xFF00,
        "a legal xattr value is not a protocol violation"
    );
    // Some backing filesystems cap an xattr value well below 64 KiB (ext4 at
    // one block). Their errno is a legitimate answer; what this test is about
    // is that the frame reached the backend at all.
    if hdr.op_or_status == STATUS_OK {
        let body = enc(&GetxattrRequest {
            node: entry.node,
            name: b"user.big".to_vec(),
            size: MAX_BODY_SIZE,
        });
        send(&mut s, 5, Opcode::Getxattr as u16, 0, &body, &[]).await;
        let (hdr, body, data) = recv(&mut s).await;
        assert_eq!((hdr.request_id, hdr.op_or_status), (5, STATUS_OK));
        let reply: XattrReply = dec(&body);
        assert_eq!(reply.size as usize, VALUE);
        assert_eq!(data, value);
    }
}

/// A body the server cannot decode is that request's problem, not the
/// connection's: the frame lengths were honored, so the stream is still in
/// sync and the next request is still answerable.
#[tokio::test]
async fn a_malformed_body_answers_einval_and_keeps_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start_server_for(dir.path()).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    hello_attach(&mut s, dir.path()).await;

    // A LOOKUP body that stops after the parent varint: postcard runs out of
    // input reading the name's length prefix.
    let (hdr, body) = call(&mut s, 3, Opcode::Lookup, &[1u8]).await;
    assert_eq!(hdr.op_or_status, libc::EINVAL as u16);
    assert!(body.is_empty());

    // Still serving.
    let body = enc(&GetattrRequest {
        node: ROOT_NODE,
        fh: None,
    });
    let (hdr, _) = call(&mut s, 4, Opcode::Getattr, &body).await;
    assert_eq!(hdr.op_or_status, STATUS_OK);
}

/// A zero or sub-page `max_io_size` clamps up rather than propagating: the
/// config parser accepts `"0"`, and a zero-length I/O ceiling would make every
/// read and write a violation.
#[tokio::test]
async fn negotiation_floors_a_zero_io_size() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start_server_for(dir.path()).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    let (hdr, body) = call(&mut s, 1, Opcode::Hello, &hello_body(PROTOCOL_VERSION, 0)).await;
    assert_eq!(hdr.op_or_status, STATUS_OK);
    let hello: HelloReply = dec(&body);
    assert_eq!(hello.max_io_size, 4096);
}

/// A second HELLO, or an ATTACH mid-session, is a violation rather than a
/// re-handshake: session state is settled once.
#[tokio::test]
async fn handshake_opcodes_after_attach_close_connection() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start_server_for(dir.path()).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    hello_attach(&mut s, dir.path()).await;
    send(
        &mut s,
        3,
        Opcode::Hello as u16,
        0,
        &hello_body(PROTOCOL_VERSION, 1 << 20),
        &[],
    )
    .await;
    assert!(at_eof(&mut s).await, "a second HELLO is fatal");

    // The other half, on its own connection: re-attaching would mean a second
    // export root under a node table already handed out against the first.
    let mut s = TcpStream::connect(addr).await.unwrap();
    hello_attach(&mut s, dir.path()).await;
    let attach = enc(&AttachRequest {
        path: path_bytes(dir.path()),
    });
    send(&mut s, 3, Opcode::Attach as u16, 0, &attach, &[]).await;
    assert!(at_eof(&mut s).await, "a mid-session ATTACH is fatal");
}

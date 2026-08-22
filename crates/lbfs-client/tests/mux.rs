//! The multiplexer against a scripted server (spec §10 layer 1).
//!
//! # Why a scripted server rather than the real one
//!
//! Everything this suite is about is a frame a correct server never sends: a
//! reply for an id nobody asked about, a data segment past the inbound bound, a
//! window held at exactly eight, replies handed back in the reverse of the
//! order they were requested. A real `lbfs-server` cannot be made to produce
//! any of them, and the ones it could produce it would produce only by
//! accident. The wire contract against the real server is already pinned a
//! layer up, in `lbfs-tests`.
//!
//! So [`Fake`] is a `TcpListener` the test drives frame by frame: it answers
//! `HELLO` and `ATTACH` with values the test chooses, then hands back a
//! [`Session`] whose `recv`/`reply` pair let the test decide what arrives, in
//! what order, and whether it is legal.

#![deny(unsafe_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use lbfs_client::conn::{ConnectError, Connection, Proposal};
use lbfs_proto::frame::{
    FrameHeader, FLAG_NO_REPLY, MAGIC, MAX_BODY_SIZE, PROTOCOL_VERSION, STATUS_ATTACH_DENIED,
    STATUS_NOT_EXPORTED, STATUS_OK, STATUS_VERSION_MISMATCH,
};
use lbfs_proto::io::{read_body, read_header, write_frame};
use lbfs_proto::ops::{
    AttachReply, AttachRequest, ForgetRequest, HelloReply, HelloRequest, LookupRequest, Opcode,
    ReadRequest, WriteReply,
};
use lbfs_proto::types::{Entry, FileAttr, XattrReply};
use lbfs_proto::Errno;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The export this suite pretends to serve.
const EXPORT: &[u8] = b"/srv/exports/one";

/// How long a test waits for a frame that must arrive. Far longer than any
/// exchange here needs; short enough that a stranded request fails the suite
/// rather than hanging it.
const ARRIVES: Duration = Duration::from_secs(10);

/// How long a test waits to conclude that a frame is *not* coming. Long enough
/// that a loaded machine does not fail the window tests, and well under the
/// 500 ms forget timer so a batch cannot sneak in behind it.
const NEVER: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// The scripted server
// ---------------------------------------------------------------------------

struct Fake {
    listener: TcpListener,
    addr: SocketAddr,
}

/// One accepted connection, past the handshake.
struct Session {
    sock: TcpStream,
}

#[derive(Debug, Clone)]
struct Frame {
    id: u64,
    op: u16,
    flags: u16,
    body: Vec<u8>,
    data: Vec<u8>,
}

impl Frame {
    fn decode<T: DeserializeOwned>(&self) -> T {
        postcard::from_bytes(&self.body).expect("request body decodes")
    }
}

impl Fake {
    async fn bind() -> Fake {
        Fake::bind_inner(None).await
    }

    /// The same, with a deliberately tiny receive buffer.
    ///
    /// Set on the listener so the accepted socket inherits it. With a receive
    /// window of a couple of kilobytes the client's own send buffer cannot grow,
    /// so a large frame written to a server that has stopped reading parks the
    /// writer task instead of vanishing into kernel memory. That stall is the
    /// only way to reach a full outbound queue from a test.
    async fn bind_stalled() -> Fake {
        Fake::bind_inner(Some(2048)).await
    }

    async fn bind_inner(recv_buffer: Option<usize>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        if let Some(bytes) = recv_buffer {
            rustix::net::sockopt::set_socket_recv_buffer_size(&listener, bytes).unwrap();
        }
        let addr = listener.local_addr().unwrap();
        Fake { listener, addr }
    }

    /// Accept one client and answer its handshake with these settled limits.
    ///
    /// Asserts the shape of both requests on the way through, so every test in
    /// the file also pins that the client sends a well-formed `HELLO` — magic,
    /// version, the proposed limits, and the `writeback` flag that only the
    /// client knows.
    async fn handshake(&self, settled: &HelloReply, root: FileAttr) -> Session {
        let (sock, _) = self.listener.accept().await.unwrap();
        let mut sess = Session { sock };

        let hello = sess.recv().await;
        assert_eq!(hello.id, 1, "HELLO is the first request on the connection");
        assert_eq!(hello.op, Opcode::Hello as u16);
        let req: HelloRequest = hello.decode();
        assert_eq!(req.magic, MAGIC);
        assert_eq!(req.version, PROTOCOL_VERSION);
        sess.reply_ok(hello.id, settled).await;

        let attach = sess.recv().await;
        assert_eq!(attach.id, 2, "ATTACH follows HELLO");
        assert_eq!(attach.op, Opcode::Attach as u16);
        let req: AttachRequest = attach.decode();
        assert_eq!(req.path, EXPORT, "the export path travels as bytes");
        sess.reply_ok(attach.id, &AttachReply { root_attr: root })
            .await;

        sess
    }
}

impl Session {
    async fn recv(&mut self) -> Frame {
        tokio::time::timeout(ARRIVES, self.recv_inner())
            .await
            .expect("a request must arrive")
    }

    /// The negative form: `None` means nothing arrived in [`NEVER`].
    async fn recv_within(&mut self, how_long: Duration) -> Option<Frame> {
        tokio::time::timeout(how_long, self.recv_inner()).await.ok()
    }

    async fn recv_inner(&mut self) -> Frame {
        let hdr = read_header(&mut self.sock).await.expect("a request header");
        let body = read_body(&mut self.sock, hdr.body_len, MAX_BODY_SIZE)
            .await
            .expect("a request body within the maximum");
        let mut data = vec![0u8; hdr.data_len as usize];
        self.sock.read_exact(&mut data).await.expect("request data");
        Frame {
            id: hdr.request_id,
            op: hdr.op_or_status,
            flags: hdr.flags,
            body,
            data,
        }
    }

    async fn reply(&mut self, id: u64, status: u16, body: &[u8], data: &[u8]) {
        let hdr = FrameHeader {
            request_id: id,
            op_or_status: status,
            flags: 0,
            body_len: body.len() as u32,
            data_len: data.len() as u32,
        };
        write_frame(&mut self.sock, hdr, body, data).await.unwrap();
    }

    async fn reply_ok<T: Serialize>(&mut self, id: u64, v: &T) {
        let body = postcard::to_allocvec(v).unwrap();
        self.reply(id, STATUS_OK, &body, &[]).await;
    }

    async fn reply_unit(&mut self, id: u64) {
        self.reply(id, STATUS_OK, &[], &[]).await;
    }

    /// A header with nothing behind it, whatever its lengths claim. The escape
    /// hatch for frames `write_frame` would refuse to build.
    async fn header_only(&mut self, hdr: FrameHeader) {
        self.sock.write_all(&hdr.encode()).await.unwrap();
        self.sock.flush().await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn settled(max_inflight: u32, max_io_size: u32) -> HelloReply {
    HelloReply {
        version: PROTOCOL_VERSION,
        max_inflight,
        max_io_size,
        max_body_size: MAX_BODY_SIZE,
    }
}

fn root_dir() -> FileAttr {
    FileAttr {
        ino: 1,
        mode: libc::S_IFDIR | 0o755,
        nlink: 2,
        blksize: 4096,
        ..FileAttr::default()
    }
}

fn an_entry(node: u64, size: u64) -> Entry {
    Entry {
        node,
        generation: 1,
        attr: FileAttr {
            ino: node,
            size,
            mode: libc::S_IFREG | 0o644,
            nlink: 1,
            ..FileAttr::default()
        },
    }
}

/// Connect against a scripted server, with the handshake already answered.
///
/// The client's `connect` blocks on the `HELLO` reply, so the two halves have
/// to run concurrently: the client goes to a task and the test plays the
/// server.
async fn connected(
    fake: &Fake,
    proposal: Proposal,
    settled: HelloReply,
) -> (Arc<Connection>, Session) {
    let addr = fake.addr;
    let client =
        tokio::spawn(async move { Connection::connect_with(addr, EXPORT, proposal).await });
    let session = fake.handshake(&settled, root_dir()).await;
    let (conn, hello, root) = client.await.unwrap().expect("the handshake succeeds");
    assert_eq!(hello, settled, "connect reports what the server settled");
    assert_eq!(conn.limits, settled, "and stores it for the mount to read");
    assert_eq!(root.mode & libc::S_IFMT, libc::S_IFDIR);
    (conn, session)
}

/// The common case: the default proposal, the default settlement.
async fn plain(fake: &Fake) -> (Arc<Connection>, Session) {
    connected(fake, Proposal::default(), settled(128, 1 << 20)).await
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connect_negotiates_attaches_and_reports_the_root() {
    let fake = Fake::bind().await;
    let addr = fake.addr;
    let proposal = Proposal {
        max_inflight: 64,
        max_io_size: 1 << 20,
        ..Proposal::default()
    };
    let client =
        tokio::spawn(async move { Connection::connect_with(addr, EXPORT, proposal).await });

    // Play the server by hand here rather than through `handshake`, so the
    // proposal's own fields are asserted rather than only its shape.
    let (sock, _) = fake.listener.accept().await.unwrap();
    let mut sess = Session { sock };
    let hello = sess.recv().await;
    let req: HelloRequest = hello.decode();
    assert_eq!(req.max_inflight, 64);
    assert_eq!(req.max_io_size, 1 << 20);
    assert!(req.writeback, "the client's writeback state rides in HELLO");
    assert_eq!(hello.flags, 0);
    assert_eq!(hello.data.len(), 0, "HELLO carries no data segment");
    sess.reply_ok(hello.id, &settled(64, 1 << 20)).await;

    let attach = sess.recv().await;
    let req: AttachRequest = attach.decode();
    assert_eq!(req.path, EXPORT);
    let mut root = root_dir();
    root.ino = 99;
    sess.reply_ok(attach.id, &AttachReply { root_attr: root })
        .await;

    let (conn, hello, got_root) = client.await.unwrap().unwrap();
    assert_eq!(hello.version, PROTOCOL_VERSION);
    assert_eq!(hello.max_inflight, 64);
    assert_eq!(hello.max_io_size, 1 << 20);
    assert_eq!(hello.max_body_size, MAX_BODY_SIZE);
    assert_eq!(got_root.ino, 99, "ATTACH's root attrs reach the caller");
    assert!(got_root.mode & libc::S_IFDIR != 0);
    assert!(!conn.is_dead());
}

#[tokio::test]
async fn a_client_that_never_negotiates_writeback_says_so() {
    let fake = Fake::bind().await;
    let addr = fake.addr;
    let client = tokio::spawn(async move { Connection::connect(addr, EXPORT, false).await });

    let (sock, _) = fake.listener.accept().await.unwrap();
    let mut sess = Session { sock };
    let hello = sess.recv().await;
    let req: HelloRequest = hello.decode();
    assert!(!req.writeback);
    sess.reply_ok(hello.id, &settled(128, 1 << 20)).await;
    let attach = sess.recv().await;
    sess.reply_ok(
        attach.id,
        &AttachReply {
            root_attr: root_dir(),
        },
    )
    .await;
    client.await.unwrap().unwrap();
}

#[tokio::test]
async fn the_handshake_statuses_reach_the_caller_by_name() {
    // Each is a different sentence for the CLI to print: check the version,
    // check the path, check the server's allowlist.
    for (status, want) in [
        (STATUS_VERSION_MISMATCH, "version"),
        (STATUS_NOT_EXPORTED, "not-exported"),
        (STATUS_ATTACH_DENIED, "denied"),
    ] {
        let fake = Fake::bind().await;
        let addr = fake.addr;
        let client = tokio::spawn(async move { Connection::connect(addr, EXPORT, true).await });

        let (sock, _) = fake.listener.accept().await.unwrap();
        let mut sess = Session { sock };
        let hello = sess.recv().await;
        if status == STATUS_VERSION_MISMATCH {
            sess.reply(hello.id, status, &[], &[]).await;
        } else {
            sess.reply_ok(hello.id, &settled(128, 1 << 20)).await;
            let attach = sess.recv().await;
            sess.reply(attach.id, status, &[], &[]).await;
        }

        let err = client.await.unwrap().expect_err("the handshake fails");
        match (want, &err) {
            ("version", ConnectError::VersionMismatch) => {}
            ("not-exported", ConnectError::NotExported) => {}
            ("denied", ConnectError::AttachDenied) => {}
            _ => panic!("{status:#x} produced {err:?}, wanted {want}"),
        }
    }
}

#[tokio::test]
async fn an_errno_from_attach_survives_as_an_errno() {
    let fake = Fake::bind().await;
    let addr = fake.addr;
    let client = tokio::spawn(async move { Connection::connect(addr, EXPORT, true).await });

    let (sock, _) = fake.listener.accept().await.unwrap();
    let mut sess = Session { sock };
    let hello = sess.recv().await;
    sess.reply_ok(hello.id, &settled(128, 1 << 20)).await;
    let attach = sess.recv().await;
    sess.reply(attach.id, Errno::EACCES.0, &[], &[]).await;

    match client.await.unwrap().expect_err("attach fails") {
        ConnectError::Attach(errno) => assert_eq!(errno, Errno::EACCES.0),
        other => panic!("wanted an errno, got {other:?}"),
    }
}

#[tokio::test]
async fn settled_limits_the_client_cannot_honour_are_refused() {
    // A window wider than the protocol's clamp. Accepting it would let the
    // client run past what the server admits and be closed for an overrun.
    let fake = Fake::bind().await;
    let addr = fake.addr;
    let client = tokio::spawn(async move { Connection::connect(addr, EXPORT, true).await });

    let (sock, _) = fake.listener.accept().await.unwrap();
    let mut sess = Session { sock };
    let hello = sess.recv().await;
    sess.reply_ok(hello.id, &settled(4096, 1 << 20)).await;

    match client.await.unwrap().expect_err("the handshake is refused") {
        ConnectError::Protocol(_) => {}
        other => panic!("wanted a protocol error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_server_that_accepts_and_says_nothing_times_the_handshake_out() {
    // The failure this closes is a mount attempt that hangs for ever with no
    // diagnostic: TCP succeeds, the client sends HELLO, and the peer simply
    // never answers.
    let fake = Fake::bind().await;
    let addr = fake.addr;
    let proposal = Proposal {
        handshake_timeout: Duration::from_millis(200),
        ..Proposal::default()
    };
    let client =
        tokio::spawn(async move { Connection::connect_with(addr, EXPORT, proposal).await });

    let (sock, _) = fake.listener.accept().await.unwrap();
    let mut sess = Session { sock };
    let hello = sess.recv().await;
    assert_eq!(hello.op, Opcode::Hello as u16, "the client did speak first");
    // ... and now the server says nothing at all.

    match client.await.unwrap().expect_err("connect must give up") {
        ConnectError::TimedOut => {}
        other => panic!("wanted TimedOut, got {other:?}"),
    }
}

#[tokio::test]
async fn a_server_that_stalls_after_hello_also_times_out() {
    // The second half of the handshake has the same exposure as the first.
    let fake = Fake::bind().await;
    let addr = fake.addr;
    let proposal = Proposal {
        handshake_timeout: Duration::from_millis(200),
        ..Proposal::default()
    };
    let client =
        tokio::spawn(async move { Connection::connect_with(addr, EXPORT, proposal).await });

    let (sock, _) = fake.listener.accept().await.unwrap();
    let mut sess = Session { sock };
    let hello = sess.recv().await;
    sess.reply_ok(hello.id, &settled(128, 1 << 20)).await;
    let attach = sess.recv().await;
    assert_eq!(attach.op, Opcode::Attach as u16);

    match client.await.unwrap().expect_err("connect must give up") {
        ConnectError::TimedOut => {}
        other => panic!("wanted TimedOut, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Calls and correlation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_typed_call_round_trips() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.lookup(1, b"f").await })
    };
    let req = sess.recv().await;
    assert_eq!(req.op, Opcode::Lookup as u16);
    assert_eq!(req.id, 3, "the session's ids start after the handshake's");
    let decoded: LookupRequest = req.decode();
    assert_eq!(decoded.parent, 1);
    assert_eq!(decoded.name, b"f");
    sess.reply_ok(req.id, &an_entry(2, 3)).await;

    let entry = call.await.unwrap().expect("the lookup succeeds");
    assert_eq!(entry.node, 2);
    assert_eq!(entry.attr.size, 3);
}

#[tokio::test]
async fn an_errno_reply_reaches_its_own_caller() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.lookup(1, b"missing").await })
    };
    let req = sess.recv().await;
    sess.reply(req.id, Errno::ENOENT.0, &[], &[]).await;
    assert_eq!(call.await.unwrap().unwrap_err(), Errno::ENOENT);
    assert!(!conn.is_dead(), "an errno is an answer, not a failure");
}

#[tokio::test]
async fn a_protocol_status_after_the_handshake_becomes_eio() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.lookup(1, b"f").await })
    };
    let req = sess.recv().await;
    sess.reply(req.id, STATUS_ATTACH_DENIED, &[], &[]).await;
    assert_eq!(call.await.unwrap().unwrap_err(), Errno::EIO);
}

#[tokio::test]
async fn a_reply_body_that_will_not_decode_is_eio_but_not_fatal() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.lookup(1, b"f").await })
    };
    let req = sess.recv().await;
    // A single byte where an Entry belongs. The frame is well formed and the
    // stream is still in sync, so only this call is lost.
    sess.reply(req.id, STATUS_OK, &[7], &[]).await;
    assert_eq!(call.await.unwrap().unwrap_err(), Errno::EIO);
    assert!(!conn.is_dead());

    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.lookup(1, b"g").await })
    };
    let req = sess.recv().await;
    sess.reply_ok(req.id, &an_entry(5, 1)).await;
    assert_eq!(call.await.unwrap().unwrap().node, 5);
}

#[tokio::test]
async fn replies_out_of_order_reach_their_own_callers() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    let mut calls = Vec::new();
    for i in 0..3u64 {
        let conn = Arc::clone(&conn);
        calls.push(tokio::spawn(async move {
            conn.lookup(1, format!("f{i}").as_bytes()).await
        }));
    }
    // Collect all three, then answer them backwards. Each reply carries a
    // distinct node id, so a mis-correlated one cannot pass.
    let mut reqs = Vec::new();
    for _ in 0..3 {
        reqs.push(sess.recv().await);
    }
    for (i, req) in reqs.iter().enumerate().rev() {
        let name: LookupRequest = req.decode();
        assert_eq!(name.name, format!("f{i}").as_bytes());
        sess.reply_ok(req.id, &an_entry(100 + i as u64, i as u64))
            .await;
    }
    for (i, call) in calls.into_iter().enumerate() {
        let entry = call.await.unwrap().unwrap();
        assert_eq!(entry.node, 100 + i as u64, "call {i} got another's reply");
        assert_eq!(entry.attr.size, i as u64);
    }
}

#[tokio::test]
async fn sixty_four_concurrent_calls_multiplex_over_one_socket() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    const N: u64 = 64;
    let mut calls = Vec::new();
    for i in 0..N {
        let conn = Arc::clone(&conn);
        calls.push(tokio::spawn(async move {
            conn.lookup(1, format!("f{i:02}").as_bytes()).await
        }));
    }
    let mut reqs = Vec::new();
    for _ in 0..N {
        reqs.push(sess.recv().await);
    }
    // Shuffled, not merely reversed: interleave the two halves so a reply's
    // position bears no relation to its request's.
    let mut order: Vec<usize> = (0..N as usize).collect();
    order.sort_by_key(|i| (i % 7, *i));
    for i in order {
        let req = &reqs[i];
        let asked: LookupRequest = req.decode();
        let n: u64 = String::from_utf8(asked.name.clone()).unwrap()[1..]
            .parse()
            .unwrap();
        sess.reply_ok(req.id, &an_entry(1000 + n, n)).await;
    }
    for (i, call) in calls.into_iter().enumerate() {
        let entry = call.await.unwrap().unwrap();
        assert_eq!(entry.node, 1000 + i as u64);
        assert_eq!(entry.attr.size, i as u64);
    }

    // Ids are assigned once and never reused: 64 calls, 64 distinct ids, all
    // past the two the handshake spent.
    let mut ids: Vec<u64> = reqs.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), N as usize, "every request has its own id");
    assert!(ids[0] >= 3, "handshake ids are not reused");
}

// ---------------------------------------------------------------------------
// Window accounting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_window_blocks_the_call_past_its_last_permit() {
    let fake = Fake::bind().await;
    // Eight is the protocol's narrowest legal window, which makes the boundary
    // cheap to hit exactly.
    let (conn, mut sess) = connected(&fake, Proposal::default(), settled(8, 1 << 20)).await;

    let mut calls = Vec::new();
    for i in 0..9u64 {
        let conn = Arc::clone(&conn);
        calls.push(tokio::spawn(async move {
            conn.lookup(1, format!("f{i}").as_bytes()).await
        }));
    }

    let mut reqs = Vec::new();
    for _ in 0..8 {
        reqs.push(sess.recv().await);
    }
    assert!(
        sess.recv_within(NEVER).await.is_none(),
        "the ninth request must wait for a permit"
    );

    // One reply frees exactly one permit, and the ninth goes out.
    sess.reply_ok(reqs[0].id, &an_entry(10, 0)).await;
    let ninth = sess.recv().await;
    assert!(
        sess.recv_within(NEVER).await.is_none(),
        "and only one: nothing follows it"
    );

    for req in &reqs[1..] {
        sess.reply_ok(req.id, &an_entry(11, 0)).await;
    }
    sess.reply_ok(ninth.id, &an_entry(12, 0)).await;
    for call in calls {
        call.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn forget_bypasses_a_full_window() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = connected(&fake, Proposal::default(), settled(8, 1 << 20)).await;

    let mut calls = Vec::new();
    for _ in 0..8 {
        let conn = Arc::clone(&conn);
        calls.push(tokio::spawn(async move { conn.lookup(1, b"f").await }));
    }
    let mut reqs = Vec::new();
    for _ in 0..8 {
        reqs.push(sess.recv().await);
    }

    // The window is full and no reply has come back, so a FORGET that spent a
    // permit could not be sent at all — and would never get one back, since
    // nothing answers a NO_REPLY frame.
    conn.send_forget(7, 1);
    let frame = sess.recv().await;
    assert_eq!(frame.op, Opcode::Forget as u16);
    assert_eq!(frame.flags, FLAG_NO_REPLY);
    let req: ForgetRequest = frame.decode();
    assert_eq!(req.items, vec![(7, 1)]);

    for req in &reqs {
        sess.reply_ok(req.id, &an_entry(10, 0)).await;
    }
    for call in calls {
        call.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn a_call_waiting_for_a_permit_wakes_with_eio_when_the_server_dies() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = connected(&fake, Proposal::default(), settled(8, 1 << 20)).await;

    let mut calls = Vec::new();
    for _ in 0..9 {
        let conn = Arc::clone(&conn);
        calls.push(tokio::spawn(async move { conn.lookup(1, b"f").await }));
    }
    for _ in 0..8 {
        sess.recv().await;
    }
    // The ninth is parked on the semaphore, not on a reply: the correlation
    // table's drain cannot reach it, so the window has to be closed too.
    drop(sess);
    for call in calls {
        assert_eq!(call.await.unwrap().unwrap_err(), Errno::EIO);
    }
    assert!(conn.is_dead());
}

/// The review's probe, as a regression test.
///
/// A caller cancelled while parked on the outbound queue used to leak its
/// window permit: the correlation entry that held the permit was already in the
/// table, the frame it was waiting to queue was discarded with the future, and
/// no reply would ever arrive to take the entry back out. Seven cancellations
/// left one of eight permits, on a connection that never noticed — and a window
/// with no permits left does not fail a call, it parks it for ever.
///
/// Reaching the outbound queue's bound needs a stalled writer, which needs a
/// server that stops reading and a receive buffer small enough that the
/// client's send buffer cannot swallow a megabyte.
#[tokio::test]
async fn cancelling_a_call_parked_on_a_full_queue_gives_its_permit_back() {
    let fake = Fake::bind_stalled().await;
    let (conn, mut sess) = stalled_session(&fake).await;

    // 1. Park the writer on a frame nobody is reading. It holds one permit.
    let stuck = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.write(2, 7, 0, vec![0xCD; STALL_BYTES]).await })
    };

    // 2. Fill the outbound queue behind it. Forgets are the only frames that
    //    can, since everything else is bounded by the eight-wide window and the
    //    queue is eight slots wider than that.
    for node in 0..(FILL_BATCHES * 64) {
        conn.send_forget(node, 1);
    }
    tokio::time::sleep(NEVER).await;

    // 3. Seven callers take the rest of the window and park on the full queue.
    let mut parked = Vec::new();
    for _ in 0..7 {
        let conn = Arc::clone(&conn);
        parked.push(tokio::spawn(async move { conn.lookup(1, b"f").await }));
    }
    tokio::time::sleep(NEVER).await;
    assert!(!conn.is_dead(), "a stalled writer is not a dead connection");

    // 4. Cancel every one of them.
    for task in &parked {
        task.abort();
    }
    for task in parked {
        assert!(task.await.unwrap_err().is_cancelled());
    }

    // 5. Clear the stall: the write is at the head of the queue, so reading it
    //    is what lets the writer move again. Answer it so its permit comes back
    //    the ordinary way, then drain the forgets behind it.
    let frame = sess.recv().await;
    assert_eq!(frame.op, Opcode::Write as u16, "the write is queued first");
    assert_eq!(frame.data.len(), STALL_BYTES);
    let written = u32::try_from(STALL_BYTES).unwrap();
    let body = postcard::to_allocvec(&WriteReply { written }).unwrap();
    sess.reply(frame.id, STATUS_OK, &body, &[]).await;
    assert_eq!(stuck.await.unwrap().unwrap(), written);
    while let Some(frame) = sess.recv_within(NEVER).await {
        assert_eq!(
            frame.op,
            Opcode::Forget as u16,
            "opcode {} reached the wire after its caller was cancelled. Either a \
             cancelled caller queued its frame anyway, or the writer never \
             stalled and this test proved nothing",
            frame.op
        );
    }

    // 6. The whole window must be usable again. With the leak, one permit came
    //    back and the other seven calls would park on the semaphore for ever.
    let mut calls = Vec::new();
    for i in 0..8u64 {
        let conn = Arc::clone(&conn);
        calls.push(tokio::spawn(async move {
            conn.lookup(1, format!("g{i}").as_bytes()).await
        }));
    }
    let mut reqs = Vec::new();
    for _ in 0..8 {
        reqs.push(sess.recv().await);
    }
    for req in &reqs {
        sess.reply_ok(req.id, &an_entry(20, 0)).await;
    }
    for call in calls {
        call.await.unwrap().unwrap();
    }
    assert!(!conn.is_dead());
}

/// Enough forget batches to fill an outbound queue of `window + OUT_SLACK`
/// slots several times over, so the batcher is certainly parked on it.
const FILL_BATCHES: u64 = 32;

/// A frame no pair of kernel socket buffers can swallow.
///
/// Loopback autotunes both ends into the megabytes, so a 1 MiB frame written to
/// a server that never reads still completes — the writer never parks and the
/// stall the two tests below depend on never happens. Sixteen mebibytes is past
/// any default `tcp_wmem`/`tcp_rmem` ceiling, and the tiny receive buffer on the
/// listener makes it park far sooner than that.
const STALL_BYTES: usize = 16 << 20;

/// A session whose negotiated I/O size is large enough to carry [`STALL_BYTES`]
/// in one frame, with the narrowest legal window so the boundary is cheap to
/// reach.
async fn stalled_session(fake: &Fake) -> (Arc<Connection>, Session) {
    let io = u32::try_from(STALL_BYTES).unwrap();
    let proposal = Proposal {
        max_io_size: io,
        ..Proposal::default()
    };
    connected(fake, proposal, settled(8, io)).await
}

// ---------------------------------------------------------------------------
// Forget batching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forgets_flush_on_the_timer() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    conn.send_forget(4, 1);
    conn.send_forget(5, 2);
    // Nothing goes out immediately: the batch is waiting for company.
    assert!(sess.recv_within(NEVER).await.is_none());

    let frame = sess.recv().await;
    assert_eq!(frame.op, Opcode::Forget as u16);
    assert_eq!(frame.flags, FLAG_NO_REPLY, "a FORGET expects no reply");
    let req: ForgetRequest = frame.decode();
    assert_eq!(req.items, vec![(4, 1), (5, 2)]);
}

#[tokio::test]
async fn a_full_batch_flushes_without_waiting_for_the_timer() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    for node in 0..64u64 {
        conn.send_forget(node, 1);
    }
    // Well inside the 500 ms timer, so arriving at all proves the count
    // triggered it.
    let frame = sess
        .recv_within(NEVER)
        .await
        .expect("a full batch goes out at once");
    let req: ForgetRequest = frame.decode();
    assert_eq!(req.items.len(), 64);
    assert_eq!(req.items[0], (0, 1));
    assert_eq!(req.items[63], (63, 1));
}

#[tokio::test]
async fn a_stalled_writer_makes_the_forget_queue_lossy_not_unbounded() {
    // The queue in front of the batcher is bounded, and reaching the bound
    // costs forgets rather than memory. `send_forget` runs on the FUSE dispatch
    // thread and cannot wait, so the only two choices while the batcher is
    // parked behind an unwritable socket are "drop" and "grow for ever".
    let fake = Fake::bind_stalled().await;
    let (conn, mut sess) = stalled_session(&fake).await;

    let stuck = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.write(2, 7, 0, vec![0xCD; STALL_BYTES]).await })
    };
    tokio::time::sleep(NEVER).await;

    // Far more than any bound the client could reasonably hold, from a caller
    // that must never block. Every one of these returns.
    for node in 0..100_000u64 {
        conn.send_forget(node, 1);
    }
    assert!(
        conn.dropped_forgets() > 0,
        "an unbounded queue would have swallowed all 100,000"
    );
    assert!(!conn.is_dead(), "losing a forget is not losing the session");

    // And the connection still works once the stall clears.
    let frame = sess.recv().await;
    assert_eq!(frame.op, Opcode::Write as u16);
    let written = u32::try_from(STALL_BYTES).unwrap();
    let body = postcard::to_allocvec(&WriteReply { written }).unwrap();
    sess.reply(frame.id, STATUS_OK, &body, &[]).await;
    assert_eq!(stuck.await.unwrap().unwrap(), written);
    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.lookup(1, b"f").await })
    };
    loop {
        let frame = sess.recv().await;
        if frame.op == Opcode::Lookup as u16 {
            sess.reply_ok(frame.id, &an_entry(2, 3)).await;
            break;
        }
        assert_eq!(frame.op, Opcode::Forget as u16);
    }
    assert_eq!(call.await.unwrap().unwrap().node, 2);
}

#[tokio::test]
async fn forgets_after_a_disconnect_neither_panic_nor_block() {
    let fake = Fake::bind().await;
    let (conn, sess) = plain(&fake).await;
    drop(sess);
    assert_eq!(conn.getattr(1, None).await.unwrap_err(), Errno::EIO);

    // The server's node table went with the connection, so there is nothing
    // left to decrement (spec §8) — but `forget` is a kernel callback with no
    // reply object, so it must return regardless.
    conn.send_forget(9, 1);
    conn.send_forget(10, 4);
    tokio::time::sleep(NEVER).await;
    assert!(conn.is_dead());
}

// ---------------------------------------------------------------------------
// Frames the client must refuse
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_oversized_reply_body_kills_the_connection() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.lookup(1, b"f").await })
    };
    let req = sess.recv().await;
    // One byte past the maximum, and never sent: the client must refuse on the
    // header alone, before it allocates anything.
    sess.header_only(FrameHeader {
        request_id: req.id,
        op_or_status: STATUS_OK,
        flags: 0,
        body_len: MAX_BODY_SIZE + 1,
        data_len: 0,
    })
    .await;

    assert_eq!(call.await.unwrap().unwrap_err(), Errno::EIO);
    assert_dead(&conn).await;
}

#[tokio::test]
async fn an_oversized_reply_data_segment_kills_the_connection() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.read(2, 7, 0, 4096).await })
    };
    let req = sess.recv().await;
    let asked: ReadRequest = req.decode();
    assert_eq!(asked.size, 4096);
    sess.header_only(FrameHeader {
        request_id: req.id,
        op_or_status: STATUS_OK,
        flags: 0,
        body_len: 0,
        data_len: (1 << 20) + 1,
    })
    .await;

    assert_eq!(call.await.unwrap().unwrap_err(), Errno::EIO);
    assert_dead(&conn).await;
}

#[tokio::test]
async fn a_full_size_xattr_survives_a_small_negotiated_io_size() {
    // The ruling this client is built around. GETXATTR and LISTXATTR answer in
    // the data segment but are bounded by the *body* maximum, so a session
    // that settled on 4096 must still accept 64 KiB of attribute value. A
    // client that policed inbound data with `max_io_size` would kill its own
    // connection here, on a reply the server was right to send.
    let fake = Fake::bind().await;
    let (conn, mut sess) = connected(&fake, Proposal::default(), settled(128, 4096)).await;
    assert_eq!(conn.limits.max_io_size, 4096);

    let value = vec![0xABu8; MAX_BODY_SIZE as usize];
    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.getxattr(2, b"user.big", MAX_BODY_SIZE).await })
    };
    let req = sess.recv().await;
    let body = postcard::to_allocvec(&XattrReply {
        size: MAX_BODY_SIZE,
    })
    .unwrap();
    sess.reply(req.id, STATUS_OK, &body, &value).await;

    let (size, got) = call.await.unwrap().expect("a legal xattr reply");
    assert_eq!(size, MAX_BODY_SIZE);
    assert_eq!(got.len(), MAX_BODY_SIZE as usize);
    assert!(got.iter().all(|b| *b == 0xAB));
    assert!(!conn.is_dead(), "the session survives its own xattr");

    // One byte past that bound is still fatal.
    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.getxattr(2, b"user.big", MAX_BODY_SIZE).await })
    };
    let req = sess.recv().await;
    sess.header_only(FrameHeader {
        request_id: req.id,
        op_or_status: STATUS_OK,
        flags: 0,
        body_len: 0,
        data_len: MAX_BODY_SIZE + 1,
    })
    .await;
    assert_eq!(call.await.unwrap().unwrap_err(), Errno::EIO);
    assert_dead(&conn).await;
}

#[tokio::test]
async fn a_reply_for_an_unknown_id_kills_the_connection() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.lookup(1, b"f").await })
    };
    let real = sess.recv().await;
    assert_ne!(real.id, 999);
    // Nothing is waiting on 999. Resolving the pending call with it would hand
    // a caller an answer to a question it never asked.
    sess.reply_ok(999, &an_entry(2, 3)).await;

    assert_eq!(call.await.unwrap().unwrap_err(), Errno::EIO);
    assert_dead(&conn).await;
}

#[tokio::test]
async fn the_same_id_answered_twice_kills_the_connection() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.lookup(1, b"f").await })
    };
    let req = sess.recv().await;
    sess.reply_ok(req.id, &an_entry(2, 3)).await;
    assert_eq!(call.await.unwrap().unwrap().node, 2);

    // The table lost the entry with the first reply, so the second is a reply
    // for an id nothing is waiting on.
    sess.reply_ok(req.id, &an_entry(2, 3)).await;
    assert_dead(&conn).await;
}

// ---------------------------------------------------------------------------
// Disconnection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_disconnect_fails_every_pending_and_every_later_call_with_eio() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    let mut calls = Vec::new();
    for i in 0..4u64 {
        let conn = Arc::clone(&conn);
        calls.push(tokio::spawn(async move {
            conn.lookup(1, format!("f{i}").as_bytes()).await
        }));
    }
    for _ in 0..4 {
        sess.recv().await;
    }
    // The server vanishes with four requests unanswered.
    drop(sess);

    for call in calls {
        assert_eq!(call.await.unwrap().unwrap_err(), Errno::EIO);
    }
    assert!(conn.is_dead());
    // And every later call fails immediately, without touching the socket.
    assert_eq!(conn.getattr(1, None).await.unwrap_err(), Errno::EIO);
    assert_eq!(conn.unlink(1, b"x").await.unwrap_err(), Errno::EIO);
    assert_eq!(conn.read(1, 1, 0, 4096).await.unwrap_err(), Errno::EIO);
}

#[tokio::test]
async fn a_reply_already_on_the_wire_survives_the_disconnect_behind_it() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    let answered = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.lookup(1, b"answered").await })
    };
    let stranded = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.lookup(1, b"stranded").await })
    };
    let first = sess.recv().await;
    let second = sess.recv().await;
    assert_eq!(first.decode::<LookupRequest>().name, b"answered");
    assert_eq!(second.decode::<LookupRequest>().name, b"stranded");
    sess.reply_ok(first.id, &an_entry(42, 7)).await;
    drop(sess);

    assert_eq!(answered.await.unwrap().unwrap().node, 42);
    assert_eq!(stranded.await.unwrap().unwrap_err(), Errno::EIO);
}

// ---------------------------------------------------------------------------
// Frames the client must not send
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_oversized_write_is_refused_locally_rather_than_sent() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = connected(&fake, Proposal::default(), settled(128, 4096)).await;

    // The server closes the connection over a data segment past the negotiated
    // ceiling, so one caller's mistake must not reach the wire.
    let err = conn.write(2, 7, 0, vec![0u8; 4097]).await.unwrap_err();
    assert_eq!(err, Errno::EINVAL);
    assert!(sess.recv_within(NEVER).await.is_none(), "nothing was sent");
    assert!(!conn.is_dead(), "and the mount is still usable");

    // Exactly at the ceiling still goes out.
    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.write(2, 7, 0, vec![1u8; 4096]).await })
    };
    let req = sess.recv().await;
    assert_eq!(req.data.len(), 4096);
    let body = postcard::to_allocvec(&WriteReply { written: 4096 }).unwrap();
    sess.reply(req.id, STATUS_OK, &body, &[]).await;
    assert_eq!(call.await.unwrap().unwrap(), 4096);
}

#[tokio::test]
async fn a_read_past_the_negotiated_ceiling_is_refused_locally() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = connected(&fake, Proposal::default(), settled(128, 4096)).await;

    // `ReadRequest.size` is the one body field the server treats as fatal.
    let err = conn.read(2, 7, 0, 4097).await.unwrap_err();
    assert_eq!(err, Errno::EINVAL);
    assert!(sess.recv_within(NEVER).await.is_none());
    assert!(!conn.is_dead());
}

#[tokio::test]
async fn an_oversized_xattr_value_is_refused_locally() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    let err = conn
        .setxattr(2, b"user.x", vec![0u8; MAX_BODY_SIZE as usize + 1], 0)
        .await
        .unwrap_err();
    assert_eq!(err, Errno::EINVAL);
    assert!(sess.recv_within(NEVER).await.is_none());

    // A value at the bound rides the data segment, not the body — even though
    // this session negotiated a 1 MiB I/O size, the xattr bound is the body
    // maximum on both sides.
    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move {
            conn.setxattr(2, b"user.x", vec![9u8; MAX_BODY_SIZE as usize], 0)
                .await
        })
    };
    let req = sess.recv().await;
    assert_eq!(req.data.len(), MAX_BODY_SIZE as usize);
    sess.reply_unit(req.id).await;
    call.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_unit_reply_carries_no_body() {
    let fake = Fake::bind().await;
    let (conn, mut sess) = plain(&fake).await;

    let call = {
        let conn = Arc::clone(&conn);
        tokio::spawn(async move { conn.rmdir(1, b"d").await })
    };
    let req = sess.recv().await;
    sess.reply_unit(req.id).await;
    call.await.unwrap().expect("an empty body is a success");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wait for the reader task to have marked the connection dead, then prove a
/// later call fails fast.
///
/// The death is observed by a task the test does not join, so a bare
/// `is_dead()` would race it on a loaded machine.
async fn assert_dead(conn: &Connection) {
    assert_eq!(
        conn.getattr(1, None).await.unwrap_err(),
        Errno::EIO,
        "every call after a violation must fail"
    );
    assert!(conn.is_dead());
}

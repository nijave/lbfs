//! A raw-frame client and an in-process server, for the protocol suite.
//!
//! This crate is a test harness and nothing else: it holds no product code and
//! ships nowhere. What lives here rather than inside a single `#[test]` file is
//! the machinery every case needs — a server on an OS-assigned port over a
//! fresh tempdir, and a client that speaks frames rather than FUSE.
//!
//! # Why a hand-rolled client
//!
//! The suite's job is the wire contract (spec §10 layer 2), so it must be able
//! to send frames the real client never would: a `body_len` past the maximum,
//! a data segment on an opcode that takes none, a truncated postcard body, an
//! opcode before the handshake. [`TestClient::frame`] and
//! [`TestClient::header_only`] are the escape hatches for exactly that, and the
//! typed [`TestClient::call`] path sits on top of them so a legal request is
//! still one line.
//!
//! Request bodies are always built by serializing the structs in
//! `lbfs_proto::ops`, never by hand-assembling bytes. A hand-built `HELLO`
//! would have gone stale the day the handshake grew its `writeback` field; a
//! serialized one cannot.

#![deny(unsafe_code)]

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use lbfs_proto::frame::{
    FrameHeader, DEFAULT_MAX_INFLIGHT, DEFAULT_MAX_IO_SIZE, FLAG_NO_REPLY, MAGIC, MAX_BODY_SIZE,
    PROTOCOL_VERSION, STATUS_OK,
};
use lbfs_proto::io::{read_body, read_header, write_frame};
use lbfs_proto::ops::{
    AttachReply, AttachRequest, CreateRequest, ForgetRequest, GetattrRequest, HelloReply,
    HelloRequest, LookupRequest, MkdirRequest, Opcode, OpenRequest, OpendirRequest, ReadRequest,
    ReaddirRequest, ReleaseRequest, ReleasedirRequest, RmdirRequest, UnlinkRequest, WriteRequest,
};
use lbfs_proto::types::{Fh, FileAttr, NodeId};
use lbfs_server::config::{Allowlist, Config, FsyncPolicy};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How long any one frame exchange may take before the test gives up.
///
/// A bug that strands a request — a permit never released, a reply never
/// queued — would otherwise hang the suite instead of failing it. The number is
/// far larger than any operation here needs and far smaller than "forever".
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// The largest reply data segment this harness will allocate for.
///
/// A reply's `body_len` has `MAX_BODY_SIZE` to check against; its `data_len`
/// has no protocol constant of its own, only whatever the session negotiated.
/// Trusting the header outright would turn a server that reported a nonsense
/// length into an allocation that takes the test runner down instead of a test
/// that fails. Two megabytes sits comfortably above the 1 MiB ceiling any case
/// here settles on, so a legitimate reply never meets it.
const MAX_REPLY_DATA: u32 = 2 << 20;

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// A server serving one tempdir on an OS-assigned port.
///
/// The tempdir is owned here so the export outlives every client in the test
/// and is removed when the test ends. The server task is not joined: the accept
/// loop only ends when the listener fails, and a test that has made its
/// assertions has nothing to wait for.
pub struct TestServer {
    pub addr: SocketAddr,
    export: tempfile::TempDir,
}

impl TestServer {
    /// The common case: honor durability, the default window, a 1 MiB ceiling.
    pub async fn start() -> TestServer {
        TestServer::with(
            FsyncPolicy::Honor,
            DEFAULT_MAX_INFLIGHT,
            DEFAULT_MAX_IO_SIZE,
        )
        .await
    }

    /// A server whose durability policy and negotiated ceilings are the test's
    /// subject rather than its background.
    pub async fn with(fsync: FsyncPolicy, max_inflight: u32, max_io_size: u32) -> TestServer {
        let export = tempfile::tempdir().unwrap();
        let addr = serve(
            vec![resolved(export.path())],
            fsync,
            max_inflight,
            max_io_size,
        )
        .await;
        TestServer { addr, export }
    }

    /// The exported directory, as this process sees it.
    pub fn path(&self) -> &Path {
        self.export.path()
    }

    /// A path inside the export, for setting a case up out of band.
    pub fn join(&self, name: &str) -> std::path::PathBuf {
        self.export.path().join(name)
    }

    /// A client that has completed HELLO and ATTACH against this export.
    pub async fn attached(&self) -> TestClient {
        TestClient::connect_and_attach(self.addr, self.path()).await
    }

    /// The same, with the handshake values the test cares about.
    pub async fn attached_with(
        &self,
        max_inflight: u32,
        max_io_size: u32,
        writeback: bool,
    ) -> TestClient {
        TestClient::connect_and_attach_with(
            self.addr,
            self.path(),
            max_inflight,
            max_io_size,
            writeback,
        )
        .await
    }

    /// A connected socket that has said nothing yet.
    pub async fn connect(&self) -> TestClient {
        TestClient::connect(self.addr).await
    }
}

/// Start a server with an explicit allowlist, for the attach-policy cases.
///
/// Port 0 rather than the protocol's 9423: the suite must not collide with a
/// server running on the developer's machine, nor with a sibling test.
pub async fn serve(
    allowed_paths: Vec<String>,
    fsync: FsyncPolicy,
    max_inflight: u32,
    max_io_size: u32,
) -> SocketAddr {
    let cfg = Config {
        listen: "127.0.0.1:0".to_string(),
        allowed_paths,
        max_inflight,
        max_io_size,
        fsync,
    };
    let allow = Allowlist::new(&cfg.allowed_paths).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = lbfs_server::rpc::serve(listener, Arc::new(cfg), Arc::new(allow)).await;
    });
    addr
}

/// A path as the kernel will report it for the descriptor the server opens.
///
/// The allowlist is matched against the resolved name, so a pattern built from
/// an unresolved tempdir path would be denied on any system where `/tmp` is a
/// symlink.
pub fn resolved(p: &Path) -> String {
    p.canonicalize().unwrap().to_str().unwrap().to_string()
}

/// A path as the wire carries it: bytes, never text.
pub fn path_bytes(p: &Path) -> Vec<u8> {
    p.as_os_str().as_bytes().to_vec()
}

pub fn enc<T: Serialize>(v: &T) -> Vec<u8> {
    postcard::to_allocvec(v).unwrap()
}

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

/// One reply frame, unpacked.
///
/// `status` is `STATUS_OK`, a raw errno, or one of the handshake statuses; the
/// accessors below say which of those the caller expected, so a failure names
/// the request rather than a bare number.
#[derive(Debug, Clone)]
pub struct Reply {
    pub id: u64,
    pub status: u16,
    pub body: Vec<u8>,
    pub data: Vec<u8>,
}

impl Reply {
    /// The decoded body of a reply that must have succeeded.
    pub fn ok<T: DeserializeOwned>(&self) -> T {
        self.expect_ok();
        postcard::from_bytes(&self.body)
            .unwrap_or_else(|e| panic!("request {} body does not decode: {e}", self.id))
    }

    /// A success that carries nothing — the shape of every `unit` reply.
    pub fn ok_unit(&self) {
        self.expect_ok();
        assert!(
            self.body.is_empty(),
            "request {} should answer with an empty body, got {:?}",
            self.id,
            self.body
        );
    }

    pub fn expect_ok(&self) {
        assert_eq!(
            self.status, STATUS_OK,
            "request {} answered status {} (errno {}) instead of OK",
            self.id, self.status, self.status
        );
    }

    /// Assert the reply is exactly this errno, and that it carries no body.
    pub fn expect_errno(&self, want: i32) {
        assert_eq!(
            self.status, want as u16,
            "request {} answered {} where errno {want} was owed",
            self.id, self.status
        );
        assert!(
            self.body.is_empty(),
            "an error reply carries no body, got {:?}",
            self.body
        );
        assert!(self.data.is_empty(), "an error reply carries no data");
    }

    pub fn is_ok(&self) -> bool {
        self.status == STATUS_OK
    }

    pub fn is_errno(&self, want: i32) -> bool {
        self.status == want as u16
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// A client that speaks frames.
///
/// Request ids are assigned here and checked on the way back, so a test that
/// calls [`TestClient::call`] cannot silently accept somebody else's answer.
/// Pipelining tests use [`TestClient::begin`] and [`TestClient::recv`] instead
/// and correlate the ids themselves, which is the whole point of those tests.
pub struct TestClient {
    sock: TcpStream,
    next_id: u64,
    seen: BTreeSet<u16>,
    settled: Option<HelloReply>,
    root_attr: Option<FileAttr>,
}

impl TestClient {
    pub async fn connect(addr: SocketAddr) -> TestClient {
        let sock = TcpStream::connect(addr).await.unwrap();
        TestClient {
            sock,
            next_id: 1,
            seen: BTreeSet::new(),
            settled: None,
            root_attr: None,
        }
    }

    /// HELLO then ATTACH, both expected to succeed.
    pub async fn connect_and_attach(addr: SocketAddr, path: &Path) -> TestClient {
        TestClient::connect_and_attach_with(
            addr,
            path,
            DEFAULT_MAX_INFLIGHT,
            DEFAULT_MAX_IO_SIZE,
            false,
        )
        .await
    }

    pub async fn connect_and_attach_with(
        addr: SocketAddr,
        path: &Path,
        max_inflight: u32,
        max_io_size: u32,
        writeback: bool,
    ) -> TestClient {
        let mut c = TestClient::connect(addr).await;
        let settled: HelloReply = c
            .hello(&HelloRequest {
                magic: MAGIC,
                version: PROTOCOL_VERSION,
                max_inflight,
                max_io_size,
                writeback,
            })
            .await
            .ok();
        let attach: AttachReply = c.attach(path).await.ok();
        c.settled = Some(settled);
        c.root_attr = Some(attach.root_attr);
        c
    }

    /// What the handshake settled. Panics before the handshake has run.
    pub fn settled(&self) -> &HelloReply {
        self.settled.as_ref().expect("HELLO has been answered")
    }

    /// The export root's attributes, as ATTACH reported them.
    pub fn root_attr(&self) -> &FileAttr {
        self.root_attr.as_ref().expect("ATTACH has been answered")
    }

    /// Every opcode this client has sent, so a matrix test can prove it left
    /// none out.
    pub fn opcodes_seen(&self) -> &BTreeSet<u16> {
        &self.seen
    }

    // --- Frames ------------------------------------------------------------

    /// Write one frame exactly as described. The escape hatch for illegal
    /// frames: nothing here is validated against the protocol.
    pub async fn frame(&mut self, id: u64, op: u16, flags: u16, body: &[u8], data: &[u8]) {
        let hdr = FrameHeader {
            request_id: id,
            op_or_status: op,
            flags,
            body_len: body.len() as u32,
            data_len: data.len() as u32,
        };
        write_frame(&mut self.sock, hdr, body, data).await.unwrap();
    }

    /// Write a header with nothing behind it, whatever its lengths claim.
    ///
    /// Separate from [`TestClient::frame`] because `write_frame` refuses a
    /// header that disagrees with the buffers, which is exactly what a test of
    /// an oversize `body_len` must be able to violate.
    pub async fn header_only(&mut self, hdr: FrameHeader) {
        self.sock.write_all(&hdr.encode()).await.unwrap();
        self.sock.flush().await.unwrap();
    }

    /// Queue a request without waiting for its reply; returns its id.
    pub async fn begin<R: Serialize>(&mut self, op: Opcode, req: &R) -> u64 {
        self.begin_data(op, req, &[]).await
    }

    pub async fn begin_data<R: Serialize>(&mut self, op: Opcode, req: &R, data: &[u8]) -> u64 {
        let id = self.take_id(op);
        self.frame(id, op as u16, 0, &enc(req), data).await;
        id
    }

    /// Send a body the server cannot decode, under a legal frame.
    pub async fn begin_raw_body(&mut self, op: Opcode, body: &[u8]) -> u64 {
        let id = self.take_id(op);
        self.frame(id, op as u16, 0, body, &[]).await;
        id
    }

    fn take_id(&mut self, op: Opcode) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.seen.insert(op as u16);
        id
    }

    /// Read the next reply, whatever it answers.
    pub async fn recv(&mut self) -> Reply {
        tokio::time::timeout(IO_TIMEOUT, self.recv_inner())
            .await
            .expect("a reply must arrive")
    }

    async fn recv_inner(&mut self) -> Reply {
        let hdr = read_header(&mut self.sock)
            .await
            .expect("reply header arrives");
        // Bounded by the protocol's maximum rather than the header's claim: a
        // server that answered with a larger body would be handing its client
        // a frame the client is obliged to treat as fatal.
        let body = read_body(&mut self.sock, hdr.body_len, MAX_BODY_SIZE)
            .await
            .expect("reply body is within MAX_BODY_SIZE");
        assert!(
            hdr.data_len <= MAX_REPLY_DATA,
            "reply {} claims a {}-byte data segment, past this harness's {MAX_REPLY_DATA}-byte bound",
            hdr.request_id,
            hdr.data_len
        );
        let mut data = vec![0u8; hdr.data_len as usize];
        self.sock
            .read_exact(&mut data)
            .await
            .expect("reply data arrives");
        Reply {
            id: hdr.request_id,
            status: hdr.op_or_status,
            body,
            data,
        }
    }

    /// One request, one reply, correlated.
    pub async fn call<R: Serialize>(&mut self, op: Opcode, req: &R) -> Reply {
        self.call_data(op, req, &[]).await
    }

    pub async fn call_data<R: Serialize>(&mut self, op: Opcode, req: &R, data: &[u8]) -> Reply {
        let id = self.begin_data(op, req, data).await;
        let reply = self.recv().await;
        assert_eq!(reply.id, id, "a reply must carry its request's id");
        reply
    }

    /// Whether the server has ended the connection, however it ended it.
    ///
    /// A violation closes a socket that still holds unread request bytes, and
    /// the kernel answers that with RST rather than FIN — so a read error
    /// counts as closed exactly as an orderly EOF does. The distinction the
    /// test cares about is "gone" against "still serving".
    pub async fn closed(&mut self) -> bool {
        let mut byte = [0u8; 1];
        match tokio::time::timeout(IO_TIMEOUT, self.sock.read(&mut byte)).await {
            Ok(Ok(0)) | Ok(Err(_)) => true,
            Ok(Ok(_)) => false,
            Err(_) => false,
        }
    }

    // --- Handshake ---------------------------------------------------------

    pub async fn hello(&mut self, req: &HelloRequest) -> Reply {
        self.call(Opcode::Hello, req).await
    }

    pub async fn attach(&mut self, path: &Path) -> Reply {
        self.call(
            Opcode::Attach,
            &AttachRequest {
                path: path_bytes(path),
            },
        )
        .await
    }

    // --- Sugar for the ops every case needs --------------------------------
    //
    // Only the ops that appear in more than one test live here; the rest are
    // spelled out at their call sites, where the request struct is part of what
    // the test is pinning.

    pub async fn lookup(&mut self, parent: NodeId, name: &[u8]) -> Reply {
        self.call(
            Opcode::Lookup,
            &LookupRequest {
                parent,
                name: name.to_vec(),
            },
        )
        .await
    }

    pub async fn getattr(&mut self, node: NodeId) -> Reply {
        self.call(Opcode::Getattr, &GetattrRequest { node, fh: None })
            .await
    }

    pub async fn mkdir(&mut self, parent: NodeId, name: &[u8], mode: u32) -> Reply {
        self.call(
            Opcode::Mkdir,
            &MkdirRequest {
                parent,
                name: name.to_vec(),
                mode,
            },
        )
        .await
    }

    pub async fn create(&mut self, parent: NodeId, name: &[u8], mode: u32, flags: i32) -> Reply {
        self.call(
            Opcode::Create,
            &CreateRequest {
                parent,
                name: name.to_vec(),
                mode,
                flags: flags as u32,
            },
        )
        .await
    }

    pub async fn open(&mut self, node: NodeId, flags: i32) -> Reply {
        self.call(
            Opcode::Open,
            &OpenRequest {
                node,
                flags: flags as u32,
            },
        )
        .await
    }

    pub async fn read(&mut self, node: NodeId, fh: Fh, offset: u64, size: u32) -> Reply {
        self.call(
            Opcode::Read,
            &ReadRequest {
                node,
                fh,
                offset,
                size,
            },
        )
        .await
    }

    pub async fn write(&mut self, node: NodeId, fh: Fh, offset: u64, data: &[u8]) -> Reply {
        self.call_data(Opcode::Write, &WriteRequest { node, fh, offset }, data)
            .await
    }

    pub async fn release(&mut self, node: NodeId, fh: Fh) -> Reply {
        self.call(Opcode::Release, &ReleaseRequest { node, fh })
            .await
    }

    pub async fn unlink(&mut self, parent: NodeId, name: &[u8]) -> Reply {
        self.call(
            Opcode::Unlink,
            &UnlinkRequest {
                parent,
                name: name.to_vec(),
            },
        )
        .await
    }

    pub async fn rmdir(&mut self, parent: NodeId, name: &[u8]) -> Reply {
        self.call(
            Opcode::Rmdir,
            &RmdirRequest {
                parent,
                name: name.to_vec(),
            },
        )
        .await
    }

    pub async fn opendir(&mut self, node: NodeId) -> Reply {
        self.call(Opcode::Opendir, &OpendirRequest { node }).await
    }

    pub async fn readdir(&mut self, node: NodeId, dh: Fh, offset: u64, max_bytes: u32) -> Reply {
        self.call(
            Opcode::Readdir,
            &ReaddirRequest {
                node,
                dh,
                offset,
                max_bytes,
            },
        )
        .await
    }

    /// READDIRPLUS shares [`ReaddirRequest`] with READDIR — pinning that is
    /// part of the point.
    pub async fn readdirplus(
        &mut self,
        node: NodeId,
        dh: Fh,
        offset: u64,
        max_bytes: u32,
    ) -> Reply {
        self.call(
            Opcode::Readdirplus,
            &ReaddirRequest {
                node,
                dh,
                offset,
                max_bytes,
            },
        )
        .await
    }

    pub async fn releasedir(&mut self, node: NodeId, dh: Fh) -> Reply {
        self.call(Opcode::Releasedir, &ReleasedirRequest { node, dh })
            .await
    }

    /// FORGET carries `FLAG_NO_REPLY` and takes no window permit, so there is
    /// nothing to read back and nothing to correlate.
    pub async fn forget(&mut self, items: Vec<(NodeId, u64)>) {
        let id = self.take_id(Opcode::Forget);
        self.frame(
            id,
            Opcode::Forget as u16,
            FLAG_NO_REPLY,
            &enc(&ForgetRequest { items }),
            &[],
        )
        .await;
    }
}

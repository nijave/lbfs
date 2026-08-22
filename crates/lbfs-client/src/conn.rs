//! One socket, many requests: the client half of the wire protocol.
//!
//! ```text
//!            ┌──────── call() ────────┐
//!            │ permit → id → waiter   │──▶ mpsc<Outbound> ──▶ writer task ──▶ socket
//!            └───────────▲────────────┘
//!                        │ oneshot                     correlation table
//!                        │                             ┌───────────────┐
//!  socket ──▶ reader task ─────────────────────────────│ id → waiter   │
//!                                                      └───────────────┘
//!  send_forget() ──▶ mpsc<(node, nlookup)> ──▶ batcher ──▶ mpsc<Outbound>
//! ```
//!
//! One task reads, one task writes, and a correlation table in between pairs a
//! reply's `request_id` with the caller waiting on it. Replies may arrive in
//! any order (spec §3.1), so a slow `READ` never delays the `GETATTR` issued
//! behind it — the same property the server's session has, viewed from the
//! other end of the socket.
//!
//! # Four invariants worth stating plainly
//!
//! * **The window permit lives in the correlation table, not in the caller.**
//!   A permit released when a caller's future is dropped would let the client
//!   put one more request on the wire than the server admitted, and the server
//!   answers an overrun by closing the connection. Parking the permit next to
//!   the oneshot means it is released exactly when the reply is taken off the
//!   table, whether or not anybody is still waiting for it.
//! * **`FORGET` spends no permit.** It carries `FLAG_NO_REPLY`, so no reply
//!   ever comes back to return the permit — a client that charged one would
//!   leak a window slot per forgotten node until the window was gone. The
//!   server admits forgets outside the window for the mirror-image reason.
//! * **Inbound lengths are checked before anything is allocated, and the two
//!   segments are bounded by different numbers.** `body_len` by
//!   [`MAX_BODY_SIZE`]; `data_len` by `max(negotiated max_io_size,
//!   MAX_BODY_SIZE)`, because `GETXATTR` and `LISTXATTR` answer in the data
//!   segment under the *body* bound rather than the I/O bound. A client that
//!   policed inbound data with `max_io_size` alone would kill its own
//!   connection on a legal 64 KiB xattr over a session that settled on 4096.
//! * **A dead connection stays dead.** There is no reconnect in v1 (spec §7):
//!   node ids, handles and lookup counts are session state the server drops
//!   with the socket, so pretending otherwise would hand the caller a handle
//!   that names nothing. Every pending caller gets `EIO`, every later call
//!   gets `EIO` immediately, and the mount stays unmountable-clean.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use lbfs_proto::frame::{
    FrameHeader, DEFAULT_MAX_INFLIGHT, DEFAULT_MAX_IO_SIZE, FLAG_NO_REPLY, MAGIC, MAX_BODY_SIZE,
    PROTOCOL_VERSION, STATUS_ATTACH_DENIED, STATUS_NOT_EXPORTED, STATUS_OK,
    STATUS_VERSION_MISMATCH, WINDOW_CLAMP,
};
use lbfs_proto::io::{read_body, read_header, write_frame, IoError};
use lbfs_proto::ops::{
    AttachReply, AttachRequest, CopyFileRangeReply, CopyFileRangeRequest, CreateReply,
    CreateRequest, FallocateRequest, FlushRequest, ForgetRequest, FsyncRequest, FsyncdirRequest,
    GetattrRequest, GetxattrRequest, HelloReply, HelloRequest, LinkRequest, ListxattrRequest,
    LookupRequest, LseekReply, LseekRequest, MkdirRequest, Opcode, OpenReply, OpenRequest,
    OpendirReply, OpendirRequest, ReadRequest, ReaddirReply, ReaddirRequest, ReaddirplusReply,
    ReadlinkReply, ReadlinkRequest, ReleaseRequest, ReleasedirRequest, RemovexattrRequest,
    RenameRequest, RmdirRequest, SetattrRequest, SetxattrRequest, StatfsRequest, SymlinkRequest,
    UnlinkRequest, WriteReply, WriteRequest,
};
use lbfs_proto::types::{Entry, Fh, FileAttr, NodeId, SetattrArgs, StatfsReply, XattrReply};
use lbfs_proto::Errno;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

/// The smallest I/O ceiling this client will accept from a server.
///
/// Mirrors `lbfs_server::rpc::MIN_IO_SIZE`. The server floors the negotiated
/// value here, so a smaller one means the peer is not the protocol's server and
/// nothing it says about lengths can be trusted.
const MIN_IO_SIZE: u32 = 4096;

/// Keepalive, matching the server's side of the same connection (spec §8).
///
/// Without it a server that lost power leaves every FUSE request parked on a
/// socket that will never answer; ~25 s to detection turns that into `EIO`.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(10);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
const KEEPALIVE_COUNT: u32 = 3;

/// How many forgets ride in one frame, and how long the first of them waits
/// for company.
///
/// Batching exists because the kernel emits a `FORGET` per evicted inode and a
/// frame per inode would swamp a metadata-heavy workload; the timer exists
/// because the last few in a batch must not wait for a 64th that never comes.
/// 64 items is roughly a kilobyte of body — nowhere near [`MAX_BODY_SIZE`].
const FORGET_BATCH: usize = 64;
const FORGET_INTERVAL: Duration = Duration::from_millis(500);

/// Slack above the window on the outbound queue.
///
/// The queue is bounded by the window because that is what bounds requests in
/// flight, plus a little room for the `FORGET` frames that ride outside the
/// window. Past that the batcher waits, which is the correct backpressure: a
/// client that cannot write cannot usefully keep queueing.
const OUT_SLACK: usize = 8;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a mount never got off the ground.
///
/// Typed rather than an `io::Error` string because the three handshake statuses
/// are the difference between "check the path", "check the server's allowlist"
/// and "upgrade one side": spec §8 asks the CLI to say which, and a caller
/// cannot recover a status from a formatted message.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("the server does not speak protocol version {PROTOCOL_VERSION}")]
    VersionMismatch,
    #[error("the server does not export that path")]
    NotExported,
    #[error("the server refused access to that export")]
    AttachDenied,
    #[error("the server could not open that export: errno {0}")]
    Attach(u16),
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
}

impl From<IoError> for ConnectError {
    fn from(e: IoError) -> ConnectError {
        match e {
            IoError::Io(e) => ConnectError::Io(e),
            IoError::Protocol(why) => ConnectError::Protocol(why),
        }
    }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// What this client asks the server for. The server proposes back and the
/// smaller of the two wins, bounded by what the protocol allows either way.
#[derive(Debug, Clone, Copy)]
pub struct Proposal {
    pub max_inflight: u32,
    pub max_io_size: u32,
    /// Whether this mount will run with `FUSE_WRITEBACK_CACHE`.
    ///
    /// Not negotiable and not a server option: it says whose kernel owns the
    /// page cache and the file size, which changes how the server reads an
    /// `OPEN`'s flags. Only the client knows it, so it travels in `HELLO`.
    pub writeback: bool,
}

impl Default for Proposal {
    fn default() -> Self {
        Proposal {
            max_inflight: DEFAULT_MAX_INFLIGHT,
            max_io_size: DEFAULT_MAX_IO_SIZE,
            // Spec §7: on by default, because letting the kernel aggregate
            // small writes is the largest single win for build workloads.
            writeback: true,
        }
    }
}

/// One frame of the handshake, read before the reader task exists.
struct HandshakeReply {
    status: u16,
    body: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// A reply, unpacked from its frame.
struct Reply {
    status: u16,
    body: Vec<u8>,
    data: Vec<u8>,
}

/// A request on its way to the socket.
struct Outbound {
    id: u64,
    op: u16,
    flags: u16,
    body: Vec<u8>,
    data: Vec<u8>,
}

/// One caller waiting for one `request_id`.
struct Waiter {
    tx: oneshot::Sender<Reply>,
    /// The window permit this request is spending, held here rather than by
    /// the caller so that dropping a `call` future cannot return a permit for
    /// a request the server still has in flight.
    _permit: OwnedSemaphorePermit,
}

/// The correlation table, which is also where the connection's life is
/// recorded.
///
/// A separate `AtomicBool` would race: a caller could pass the check and insert
/// a waiter into a table that the reader had already drained, and then wait
/// forever. Making death a state *of the table* means registering and dying are
/// ordered by the same lock.
enum Table {
    Live(HashMap<u64, Waiter>),
    Dead,
}

struct Shared {
    table: Mutex<Table>,
    /// A fast, advisory copy of `Table::Dead` for the `call` fast path and for
    /// [`Connection::is_dead`]. Authority lives in the table.
    dead: AtomicBool,
    /// Monotonically increasing, never reused within a connection (spec §3.1).
    /// Shared with the forget batcher so no two frames can carry one id.
    next_id: AtomicU64,
    window: Arc<Semaphore>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Table> {
        // Poison-tolerant: nothing here can panic while the lock is held, and
        // turning a stranger's panic into an unusable mount would be worse
        // than continuing with a table that is structurally intact.
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// Take a slot for `id`. `false` means the connection died first.
    fn try_register(&self, id: u64, waiter: Waiter) -> bool {
        match &mut *self.lock() {
            Table::Live(pending) => {
                let clash = pending.insert(id, waiter);
                debug_assert!(clash.is_none(), "request ids are never reused");
                true
            }
            Table::Dead => false,
        }
    }

    /// Give up a slot whose request never reached the socket.
    fn drop_waiter(&self, id: u64) {
        if let Table::Live(pending) = &mut *self.lock() {
            pending.remove(&id);
        }
    }

    /// Hand a reply to whoever is waiting for it. `false` means nobody was —
    /// a protocol violation, since ids are never reused and the table only
    /// loses an entry when its reply arrives.
    fn try_complete(&self, id: u64, reply: Reply) -> bool {
        let waiter = match &mut *self.lock() {
            Table::Live(pending) => pending.remove(&id),
            // Already dead: the caller has its `EIO` and this reply is moot.
            Table::Dead => return true,
        };
        match waiter {
            // A send failure means the caller stopped waiting. The permit
            // rides in the waiter, so it is released here either way.
            Some(waiter) => {
                let _ = waiter.tx.send(reply);
                true
            }
            None => false,
        }
    }

    /// End the connection: every pending caller gets `EIO`, and so does every
    /// later one. Idempotent — whoever notices first wins.
    fn kill(&self, why: &str) {
        let pending = match std::mem::replace(&mut *self.lock(), Table::Dead) {
            Table::Live(pending) => pending,
            Table::Dead => return,
        };
        self.dead.store(true, Ordering::Release);
        // Wakes callers parked waiting for a permit, which would otherwise
        // wait on a window that can never open again.
        self.window.close();
        if pending.is_empty() {
            tracing::debug!(reason = why, "connection closed");
        } else {
            tracing::warn!(
                reason = why,
                pending = pending.len(),
                "connection lost; failing pending requests with EIO"
            );
        }
        // Dropping the waiters drops their oneshot senders, which is what each
        // caller reads as `EIO`.
        drop(pending);
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// A live session with one server: TCP, handshake settled, export attached.
///
/// Cheap to share — every method takes `&self` — and meant to be held in an
/// `Arc` by the FUSE bridge, which spawns one task per callback.
pub struct Connection {
    shared: Arc<Shared>,
    out_tx: mpsc::Sender<Outbound>,
    forget_tx: mpsc::UnboundedSender<(NodeId, u64)>,
    /// Aborted on drop. The writer and the batcher stop on their own when
    /// their channels close, and they must, so that queued frames still reach
    /// the socket; the reader has nothing to finish and would otherwise sit on
    /// a `read` until the peer happened to close.
    reader: JoinHandle<()>,
    /// What the handshake settled, verbatim from the server.
    pub limits: HelloReply,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Deliberately shallow: the settled limits and whether the session is still
/// alive. The correlation table's contents are another caller's business, and
/// printing them would mean taking its lock from a formatter.
impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("limits", &self.limits)
            .field("dead", &self.is_dead())
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Connect, shake hands, attach the export.
    ///
    /// Returns the connection, the settled limits, and the export root's
    /// attributes — `ATTACH` already reports them, so a mount need not spend a
    /// `GETATTR` on the one inode it is guaranteed to want.
    pub async fn connect(
        addr: SocketAddr,
        export_path: &[u8],
        writeback: bool,
    ) -> Result<(Arc<Connection>, HelloReply, FileAttr), ConnectError> {
        Connection::connect_with(
            addr,
            export_path,
            Proposal {
                writeback,
                ..Proposal::default()
            },
        )
        .await
    }

    /// The same, for a caller that wants to propose its own limits.
    pub async fn connect_with(
        addr: SocketAddr,
        export_path: &[u8],
        proposal: Proposal,
    ) -> Result<(Arc<Connection>, HelloReply, FileAttr), ConnectError> {
        let mut sock = TcpStream::connect(addr).await?;
        configure_socket(&sock)?;

        // Ids 1 and 2 belong to the handshake; the session's counter starts
        // after them so no id is ever reused on this connection.
        let settled = hello(&mut sock, &proposal).await?;
        let root_attr = attach(&mut sock, export_path).await?;

        let (read_half, write_half) = sock.into_split();
        let shared = Arc::new(Shared {
            table: Mutex::new(Table::Live(HashMap::new())),
            dead: AtomicBool::new(false),
            next_id: AtomicU64::new(3),
            window: Arc::new(Semaphore::new(settled.max_inflight as usize)),
        });
        let (out_tx, out_rx) = mpsc::channel(settled.max_inflight as usize + OUT_SLACK);
        let (forget_tx, forget_rx) = mpsc::unbounded_channel();

        let reader = tokio::spawn(reader_task(
            read_half,
            Arc::clone(&shared),
            inbound_data_bound(&settled),
        ));
        tokio::spawn(writer_task(write_half, out_rx, Arc::clone(&shared)));
        tokio::spawn(forget_task(forget_rx, out_tx.clone(), Arc::clone(&shared)));

        tracing::info!(
            %addr,
            max_inflight = settled.max_inflight,
            max_io_size = settled.max_io_size,
            writeback = proposal.writeback,
            "attached"
        );
        let conn = Arc::new(Connection {
            shared,
            out_tx,
            forget_tx,
            reader,
            limits: settled.clone(),
        });
        Ok((conn, settled, root_attr))
    }

    /// Whether the connection has failed. Once true, never false again.
    pub fn is_dead(&self) -> bool {
        self.shared.is_dead()
    }

    /// One request, one reply, correlated.
    ///
    /// `data` is the frame's data segment — bytes for a `WRITE` or a
    /// `SETXATTR` value, empty for everything else. The returned `Vec` is the
    /// reply's data segment, which only `READ`, `GETXATTR` and `LISTXATTR`
    /// fill.
    pub async fn call<Req: Serialize, Rep: DeserializeOwned>(
        &self,
        op: Opcode,
        req: &Req,
        data: Vec<u8>,
    ) -> Result<(Rep, Vec<u8>), Errno> {
        let (body, data) = self.call_raw(op, encode(op, req)?, data).await?;
        match postcard::from_bytes(&body) {
            Ok(rep) => Ok((rep, data)),
            Err(e) => {
                // The frame was well formed and the stream is still in sync,
                // so this is one bad answer rather than a dead connection —
                // the same call the server makes for a body it cannot decode.
                tracing::error!(?op, error = %e, "reply body does not decode");
                Err(Errno::EIO)
            }
        }
    }

    /// The same, for the many operations whose success carries no body.
    ///
    /// Decoding `()` from an empty body would work, but saying so directly
    /// keeps the postcard round trip out of the hot path for two thirds of the
    /// opcode table.
    async fn call_unit<Req: Serialize>(&self, op: Opcode, req: &Req) -> Result<(), Errno> {
        self.call_raw(op, encode(op, req)?, Vec::new()).await?;
        Ok(())
    }

    /// Everything a call does that does not depend on the body's type.
    async fn call_raw(
        &self,
        op: Opcode,
        body: Vec<u8>,
        data: Vec<u8>,
    ) -> Result<(Vec<u8>, Vec<u8>), Errno> {
        if self.shared.is_dead() {
            return Err(Errno::EIO);
        }
        // Outbound lengths are checked here for one reason: every one of them
        // is connection-fatal at the server (spec §3.1), so a client that sent
        // one would answer a single oversized request by killing the whole
        // mount. `EINVAL` to the one caller is the containable failure. Both
        // limits mirror the server's, and the FUSE layer's own ceilings should
        // keep them out of reach.
        if body.len() > MAX_BODY_SIZE as usize {
            tracing::error!(?op, len = body.len(), "request body exceeds MAX_BODY_SIZE");
            return Err(Errno::EINVAL);
        }
        if data.len() > outbound_data_limit(op, self.limits.max_io_size) as usize {
            tracing::error!(
                ?op,
                len = data.len(),
                "request data segment is over the limit"
            );
            return Err(Errno::EINVAL);
        }

        // The permit is taken before the id so that a caller parked on a full
        // window does not burn ids while it waits. A closed semaphore is the
        // connection having died underneath us.
        let permit = Arc::clone(&self.shared.window)
            .acquire_owned()
            .await
            .map_err(|_| Errno::EIO)?;
        let id = self.shared.next_id();
        let (tx, rx) = oneshot::channel();
        if !self.shared.try_register(
            id,
            Waiter {
                tx,
                _permit: permit,
            },
        ) {
            return Err(Errno::EIO);
        }
        let queued = self
            .out_tx
            .send(Outbound {
                id,
                op: op as u16,
                flags: 0,
                body,
                data,
            })
            .await;
        if queued.is_err() {
            // No writer left, so this request will never be answered. Take the
            // slot back rather than leaving a waiter — and its permit — parked
            // for the life of the process.
            self.shared.drop_waiter(id);
            self.shared.kill("the writer task is gone");
            return Err(Errno::EIO);
        }

        // A dropped sender is the drain in `Shared::kill`, which is `EIO` by
        // construction: it only runs when the connection is over.
        let reply = rx.await.map_err(|_| Errno::EIO)?;
        match reply.status {
            STATUS_OK => Ok((reply.body, reply.data)),
            errno @ 1..=4095 => Err(Errno(errno)),
            status => {
                // Protocol statuses belong to the handshake, and 4096..0xFF00
                // is not a Linux errno at all. Either way the server said
                // something this client cannot pass to the kernel.
                tracing::error!(?op, status, "server answered an unusable status");
                Err(Errno::EIO)
            }
        }
    }

    /// Drop a lookup count, fire and forget.
    ///
    /// Synchronous on purpose: the kernel's `forget` callback has no reply
    /// object and no way to wait, so this only queues. The batcher turns a
    /// burst of evictions into one frame.
    pub fn send_forget(&self, node: NodeId, nlookup: u64) {
        if self.forget_tx.send((node, nlookup)).is_err() {
            // The connection is gone, and with it the whole node table: there
            // is nothing left to decrement (spec §8).
            tracing::debug!(node, nlookup, "connection is gone; dropping FORGET");
        }
    }

    // -----------------------------------------------------------------------
    // Typed calls — one per opcode, in the order of the opcode table.
    //
    // Dull by design. Each is a decode-free wrapper that names the request
    // struct and the reply type, so the FUSE bridge above never has to spell
    // out a turbofish or remember which reply rides in the data segment.
    // -----------------------------------------------------------------------

    pub async fn lookup(&self, parent: NodeId, name: &[u8]) -> Result<Entry, Errno> {
        let (entry, _) = self
            .call(
                Opcode::Lookup,
                &LookupRequest {
                    parent,
                    name: name.to_vec(),
                },
                Vec::new(),
            )
            .await?;
        Ok(entry)
    }

    pub async fn getattr(&self, node: NodeId, fh: Option<Fh>) -> Result<FileAttr, Errno> {
        let (attr, _) = self
            .call(Opcode::Getattr, &GetattrRequest { node, fh }, Vec::new())
            .await?;
        Ok(attr)
    }

    pub async fn setattr(&self, node: NodeId, args: SetattrArgs) -> Result<FileAttr, Errno> {
        let (attr, _) = self
            .call(Opcode::Setattr, &SetattrRequest { node, args }, Vec::new())
            .await?;
        Ok(attr)
    }

    pub async fn readlink(&self, node: NodeId) -> Result<Vec<u8>, Errno> {
        let (reply, _): (ReadlinkReply, _) = self
            .call(Opcode::Readlink, &ReadlinkRequest { node }, Vec::new())
            .await?;
        Ok(reply.target)
    }

    pub async fn symlink(
        &self,
        parent: NodeId,
        name: &[u8],
        target: &[u8],
    ) -> Result<Entry, Errno> {
        let (entry, _) = self
            .call(
                Opcode::Symlink,
                &SymlinkRequest {
                    parent,
                    name: name.to_vec(),
                    target: target.to_vec(),
                },
                Vec::new(),
            )
            .await?;
        Ok(entry)
    }

    pub async fn mkdir(&self, parent: NodeId, name: &[u8], mode: u32) -> Result<Entry, Errno> {
        let (entry, _) = self
            .call(
                Opcode::Mkdir,
                &MkdirRequest {
                    parent,
                    name: name.to_vec(),
                    mode,
                },
                Vec::new(),
            )
            .await?;
        Ok(entry)
    }

    pub async fn unlink(&self, parent: NodeId, name: &[u8]) -> Result<(), Errno> {
        self.call_unit(
            Opcode::Unlink,
            &UnlinkRequest {
                parent,
                name: name.to_vec(),
            },
        )
        .await
    }

    pub async fn rmdir(&self, parent: NodeId, name: &[u8]) -> Result<(), Errno> {
        self.call_unit(
            Opcode::Rmdir,
            &RmdirRequest {
                parent,
                name: name.to_vec(),
            },
        )
        .await
    }

    pub async fn rename(
        &self,
        parent: NodeId,
        name: &[u8],
        newparent: NodeId,
        newname: &[u8],
        flags: u32,
    ) -> Result<(), Errno> {
        self.call_unit(
            Opcode::Rename,
            &RenameRequest {
                parent,
                name: name.to_vec(),
                newparent,
                newname: newname.to_vec(),
                flags,
            },
        )
        .await
    }

    pub async fn link(
        &self,
        node: NodeId,
        newparent: NodeId,
        newname: &[u8],
    ) -> Result<Entry, Errno> {
        let (entry, _) = self
            .call(
                Opcode::Link,
                &LinkRequest {
                    node,
                    newparent,
                    newname: newname.to_vec(),
                },
                Vec::new(),
            )
            .await?;
        Ok(entry)
    }

    pub async fn open(&self, node: NodeId, flags: u32) -> Result<Fh, Errno> {
        let (reply, _): (OpenReply, _) = self
            .call(Opcode::Open, &OpenRequest { node, flags }, Vec::new())
            .await?;
        Ok(reply.fh)
    }

    pub async fn create(
        &self,
        parent: NodeId,
        name: &[u8],
        mode: u32,
        flags: u32,
    ) -> Result<(Entry, Fh), Errno> {
        let (reply, _): (CreateReply, _) = self
            .call(
                Opcode::Create,
                &CreateRequest {
                    parent,
                    name: name.to_vec(),
                    mode,
                    flags,
                },
                Vec::new(),
            )
            .await?;
        Ok((reply.entry, reply.fh))
    }

    /// A read's bytes come back in the data segment, so there is no body to
    /// decode — the reply is the payload.
    pub async fn read(
        &self,
        node: NodeId,
        fh: Fh,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, Errno> {
        // The one request whose *body* names a length the server must allocate
        // for, and one it treats as fatal when it is too large. Refused here
        // for the same reason as the outbound frame limits: one failed read
        // beats a dead mount. The FUSE layer keeps reads under this ceiling by
        // negotiating `max_read` from the same number.
        if size > self.limits.max_io_size {
            tracing::error!(size, max = self.limits.max_io_size, "READ over the ceiling");
            return Err(Errno::EINVAL);
        }
        let req = ReadRequest {
            node,
            fh,
            offset,
            size,
        };
        let (_, data) = self
            .call_raw(Opcode::Read, encode(Opcode::Read, &req)?, Vec::new())
            .await?;
        Ok(data)
    }

    pub async fn write(
        &self,
        node: NodeId,
        fh: Fh,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<u32, Errno> {
        let (reply, _): (WriteReply, _) = self
            .call(Opcode::Write, &WriteRequest { node, fh, offset }, data)
            .await?;
        Ok(reply.written)
    }

    pub async fn flush(&self, node: NodeId, fh: Fh) -> Result<(), Errno> {
        self.call_unit(Opcode::Flush, &FlushRequest { node, fh })
            .await
    }

    pub async fn release(&self, node: NodeId, fh: Fh) -> Result<(), Errno> {
        self.call_unit(Opcode::Release, &ReleaseRequest { node, fh })
            .await
    }

    pub async fn fsync(&self, node: NodeId, fh: Fh, datasync: bool) -> Result<(), Errno> {
        self.call_unit(Opcode::Fsync, &FsyncRequest { node, fh, datasync })
            .await
    }

    pub async fn fallocate(
        &self,
        node: NodeId,
        fh: Fh,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> Result<(), Errno> {
        self.call_unit(
            Opcode::Fallocate,
            &FallocateRequest {
                node,
                fh,
                offset,
                length,
                mode,
            },
        )
        .await
    }

    pub async fn lseek(
        &self,
        node: NodeId,
        fh: Fh,
        offset: u64,
        whence: u32,
    ) -> Result<u64, Errno> {
        let (reply, _): (LseekReply, _) = self
            .call(
                Opcode::Lseek,
                &LseekRequest {
                    node,
                    fh,
                    offset,
                    whence,
                },
                Vec::new(),
            )
            .await?;
        Ok(reply.offset)
    }

    /// Takes the request struct whole: seven parameters is where an argument
    /// list stops being readable, and the proto already names them.
    pub async fn copy_file_range(&self, req: &CopyFileRangeRequest) -> Result<u64, Errno> {
        let (reply, _): (CopyFileRangeReply, _) =
            self.call(Opcode::CopyFileRange, req, Vec::new()).await?;
        Ok(reply.copied)
    }

    pub async fn opendir(&self, node: NodeId) -> Result<Fh, Errno> {
        let (reply, _): (OpendirReply, _) = self
            .call(Opcode::Opendir, &OpendirRequest { node }, Vec::new())
            .await?;
        Ok(reply.dh)
    }

    pub async fn readdir(
        &self,
        node: NodeId,
        dh: Fh,
        offset: u64,
        max_bytes: u32,
    ) -> Result<ReaddirReply, Errno> {
        let (reply, _) = self
            .call(
                Opcode::Readdir,
                &self.page_request(node, dh, offset, max_bytes),
                Vec::new(),
            )
            .await?;
        Ok(reply)
    }

    /// Shares [`ReaddirRequest`] with `READDIR`; only the reply differs.
    pub async fn readdirplus(
        &self,
        node: NodeId,
        dh: Fh,
        offset: u64,
        max_bytes: u32,
    ) -> Result<ReaddirplusReply, Errno> {
        let (reply, _) = self
            .call(
                Opcode::Readdirplus,
                &self.page_request(node, dh, offset, max_bytes),
                Vec::new(),
            )
            .await?;
        Ok(reply)
    }

    /// Clamp a directory page to what a reply frame can legally carry.
    ///
    /// Asking for more would invite an answer this client is obliged to treat
    /// as fatal. The server clamps too; neither is the other's excuse.
    fn page_request(&self, node: NodeId, dh: Fh, offset: u64, max_bytes: u32) -> ReaddirRequest {
        ReaddirRequest {
            node,
            dh,
            offset,
            max_bytes: max_bytes.min(self.limits.max_body_size.min(MAX_BODY_SIZE)),
        }
    }

    pub async fn releasedir(&self, node: NodeId, dh: Fh) -> Result<(), Errno> {
        self.call_unit(Opcode::Releasedir, &ReleasedirRequest { node, dh })
            .await
    }

    pub async fn fsyncdir(&self, node: NodeId, dh: Fh, datasync: bool) -> Result<(), Errno> {
        self.call_unit(Opcode::Fsyncdir, &FsyncdirRequest { node, dh, datasync })
            .await
    }

    pub async fn statfs(&self, node: NodeId) -> Result<StatfsReply, Errno> {
        let (reply, _) = self
            .call(Opcode::Statfs, &StatfsRequest { node }, Vec::new())
            .await?;
        Ok(reply)
    }

    /// FUSE's two-phase xattr read: `size == 0` asks only for the length, and
    /// the value comes back empty. The returned `u32` is the true length in
    /// both phases, so a caller can answer `ERANGE` for itself.
    pub async fn getxattr(
        &self,
        node: NodeId,
        name: &[u8],
        size: u32,
    ) -> Result<(u32, Vec<u8>), Errno> {
        let (reply, data): (XattrReply, _) = self
            .call(
                Opcode::Getxattr,
                &GetxattrRequest {
                    node,
                    name: name.to_vec(),
                    size,
                },
                Vec::new(),
            )
            .await?;
        Ok((reply.size, data))
    }

    pub async fn setxattr(
        &self,
        node: NodeId,
        name: &[u8],
        value: Vec<u8>,
        flags: u32,
    ) -> Result<(), Errno> {
        let req = SetxattrRequest {
            node,
            name: name.to_vec(),
            flags,
        };
        self.call_raw(Opcode::Setxattr, encode(Opcode::Setxattr, &req)?, value)
            .await?;
        Ok(())
    }

    pub async fn listxattr(&self, node: NodeId, size: u32) -> Result<(u32, Vec<u8>), Errno> {
        let (reply, data): (XattrReply, _) = self
            .call(
                Opcode::Listxattr,
                &ListxattrRequest { node, size },
                Vec::new(),
            )
            .await?;
        Ok((reply.size, data))
    }

    pub async fn removexattr(&self, node: NodeId, name: &[u8]) -> Result<(), Errno> {
        self.call_unit(
            Opcode::Removexattr,
            &RemovexattrRequest {
                node,
                name: name.to_vec(),
            },
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Handshake, step by step
// ---------------------------------------------------------------------------

async fn hello(sock: &mut TcpStream, proposal: &Proposal) -> Result<HelloReply, ConnectError> {
    let req = HelloRequest {
        magic: MAGIC,
        version: PROTOCOL_VERSION,
        max_inflight: proposal.max_inflight,
        max_io_size: proposal.max_io_size,
        writeback: proposal.writeback,
    };
    let reply = exchange(sock, 1, Opcode::Hello, &req).await?;
    match reply.status {
        STATUS_OK => {}
        STATUS_VERSION_MISMATCH => return Err(ConnectError::VersionMismatch),
        status => {
            tracing::error!(status, "HELLO answered with an unexpected status");
            return Err(ConnectError::Protocol("HELLO was refused"));
        }
    }
    let settled: HelloReply = postcard::from_bytes(&reply.body)
        .map_err(|_| ConnectError::Protocol("malformed HELLO reply body"))?;
    check_settled(proposal, &settled)?;
    Ok(settled)
}

/// Refuse settled limits this client cannot honour.
///
/// Not paranoia about a hostile peer — that waits for mTLS — but about a peer
/// that is not this protocol's server at all. Every value here is one the
/// client will later enforce frame lengths against, so accepting a nonsense
/// one turns a bad handshake into a mount that dies on its first real request,
/// with nothing in the log to say why.
fn check_settled(proposal: &Proposal, settled: &HelloReply) -> Result<(), ConnectError> {
    if settled.version != PROTOCOL_VERSION {
        return Err(ConnectError::Protocol(
            "the server settled on a different protocol version",
        ));
    }
    if settled.max_inflight < WINDOW_CLAMP.0 || settled.max_inflight > WINDOW_CLAMP.1 {
        return Err(ConnectError::Protocol(
            "the settled window is outside the protocol's clamp",
        ));
    }
    // The server takes the smaller of the two proposals and then applies the
    // clamp, so it may settle *above* a proposal that was below the floor.
    if settled.max_inflight > proposal.max_inflight.max(WINDOW_CLAMP.0) {
        return Err(ConnectError::Protocol(
            "the server settled on a larger window than this client proposed",
        ));
    }
    if settled.max_io_size < MIN_IO_SIZE {
        return Err(ConnectError::Protocol(
            "the settled I/O size is below the protocol's floor",
        ));
    }
    if settled.max_io_size > proposal.max_io_size.max(MIN_IO_SIZE) {
        return Err(ConnectError::Protocol(
            "the server settled on a larger I/O size than this client proposed",
        ));
    }
    // Bounds directory pages and xattr values. A server claiming more than the
    // protocol's maximum would have this client asking for pages it must then
    // treat as fatal on the way back.
    if settled.max_body_size == 0 || settled.max_body_size > MAX_BODY_SIZE {
        return Err(ConnectError::Protocol(
            "the settled body size is outside the protocol's bound",
        ));
    }
    Ok(())
}

async fn attach(sock: &mut TcpStream, export_path: &[u8]) -> Result<FileAttr, ConnectError> {
    let req = AttachRequest {
        path: export_path.to_vec(),
    };
    let reply = exchange(sock, 2, Opcode::Attach, &req).await?;
    match reply.status {
        STATUS_OK => {}
        STATUS_NOT_EXPORTED => return Err(ConnectError::NotExported),
        STATUS_ATTACH_DENIED => return Err(ConnectError::AttachDenied),
        errno @ 1..=4095 => return Err(ConnectError::Attach(errno)),
        status => {
            tracing::error!(status, "ATTACH answered with an unexpected status");
            return Err(ConnectError::Protocol("ATTACH was refused"));
        }
    }
    let attached: AttachReply = postcard::from_bytes(&reply.body)
        .map_err(|_| ConnectError::Protocol("malformed ATTACH reply body"))?;
    Ok(attached.root_attr)
}

/// One request and its reply, written and read inline.
///
/// The handshake is strictly sequential — there is nothing to correlate and no
/// reader task yet — so this predates every piece of machinery above it.
async fn exchange<T: Serialize>(
    sock: &mut TcpStream,
    id: u64,
    op: Opcode,
    req: &T,
) -> Result<HandshakeReply, ConnectError> {
    let body = postcard::to_allocvec(req)
        .map_err(|_| ConnectError::Protocol("encoding a handshake request failed"))?;
    let hdr = FrameHeader {
        request_id: id,
        op_or_status: op as u16,
        flags: 0,
        body_len: body.len() as u32,
        data_len: 0,
    };
    write_frame(sock, hdr, &body, &[]).await?;

    let hdr = read_header(sock).await?;
    if hdr.request_id != id {
        return Err(ConnectError::Protocol(
            "a handshake reply carries the wrong request id",
        ));
    }
    if hdr.data_len != 0 {
        return Err(ConnectError::Protocol(
            "a handshake reply carries no data segment",
        ));
    }
    let body = read_body(sock, hdr.body_len, MAX_BODY_SIZE).await?;
    Ok(HandshakeReply {
        status: hdr.op_or_status,
        body,
    })
}

/// `TCP_NODELAY` plus keepalive, the mirror of the server's own socket setup.
///
/// Nagle would hold a small request back waiting for a companion that a
/// request/reply protocol never sends, putting up to 40 ms on every metadata
/// round trip.
fn configure_socket(sock: &TcpStream) -> io::Result<()> {
    use rustix::net::sockopt;
    sock.set_nodelay(true)?;
    sockopt::set_socket_keepalive(sock, true)?;
    sockopt::set_tcp_keepidle(sock, KEEPALIVE_IDLE)?;
    sockopt::set_tcp_keepintvl(sock, KEEPALIVE_INTERVAL)?;
    sockopt::set_tcp_keepcnt(sock, KEEPALIVE_COUNT)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

/// Why the reader stopped.
enum End {
    /// The peer hung up, or the socket failed. Ordinary, at unmount.
    Closed,
    /// The peer sent something the protocol does not allow. The connection is
    /// already unusable: the stream is either desynchronized or the peer is
    /// not answering the requests this client made.
    Violation(&'static str),
}

async fn reader_task(mut sock: OwnedReadHalf, shared: Arc<Shared>, max_data: u32) {
    match read_loop(&mut sock, &shared, max_data).await {
        End::Closed => shared.kill("the server closed the connection"),
        End::Violation(why) => {
            tracing::error!(why, "protocol violation from the server; closing");
            shared.kill(why);
        }
    }
}

async fn read_loop(sock: &mut OwnedReadHalf, shared: &Shared, max_data: u32) -> End {
    loop {
        let hdr = match read_header(sock).await {
            Ok(hdr) => hdr,
            Err(e) => {
                tracing::debug!(error = %e, "reply stream ended");
                return End::Closed;
            }
        };
        // Both checks precede any allocation for the frame they describe
        // (spec §3.1). The two bounds differ: see the module invariants.
        if hdr.body_len > MAX_BODY_SIZE {
            return End::Violation("reply body_len exceeds MAX_BODY_SIZE");
        }
        if hdr.data_len > max_data {
            return End::Violation("reply data_len exceeds the inbound bound");
        }
        let body = match read_body(sock, hdr.body_len, MAX_BODY_SIZE).await {
            Ok(body) => body,
            Err(_) => return End::Closed,
        };
        let mut data = vec![0u8; hdr.data_len as usize];
        if sock.read_exact(&mut data).await.is_err() {
            return End::Closed;
        }
        let reply = Reply {
            status: hdr.op_or_status,
            body,
            data,
        };
        // An id nobody is waiting for is fatal rather than ignorable: ids are
        // never reused, so either the server invented one or it answered a
        // request twice. Resolving somebody else's future on that basis would
        // hand a caller another caller's data.
        if !shared.try_complete(hdr.request_id, reply) {
            return End::Violation("reply for a request id nothing is waiting on");
        }
    }
}

/// The sole writer. Every byte this client sends leaves through here, which is
/// what keeps two concurrent requests from interleaving their frames.
async fn writer_task(
    mut sock: OwnedWriteHalf,
    mut rx: mpsc::Receiver<Outbound>,
    shared: Arc<Shared>,
) {
    while let Some(frame) = rx.recv().await {
        let hdr = FrameHeader {
            request_id: frame.id,
            op_or_status: frame.op,
            flags: frame.flags,
            body_len: frame.body.len() as u32,
            data_len: frame.data.len() as u32,
        };
        if let Err(e) = write_frame(&mut sock, hdr, &frame.body, &frame.data).await {
            tracing::debug!(error = %e, "request write failed");
            shared.kill("the socket could not be written");
            return;
        }
    }
    // The channel closed, so the `Connection` is gone. Dropping the write half
    // sends the FIN that tells the server this session is over.
}

/// Collect forgets into frames.
///
/// The first item of a batch starts a clock; the batch goes out when it is full
/// or when that clock runs out, whichever comes first. There is no idle timer,
/// so a mount that is forgetting nothing costs nothing.
async fn forget_task(
    mut rx: mpsc::UnboundedReceiver<(NodeId, u64)>,
    out: mpsc::Sender<Outbound>,
    shared: Arc<Shared>,
) {
    loop {
        let Some(first) = rx.recv().await else {
            return; // the Connection was dropped, and with it the session
        };
        let mut batch = vec![first];
        let deadline = tokio::time::Instant::now() + FORGET_INTERVAL;
        let mut closed = false;
        while batch.len() < FORGET_BATCH {
            tokio::select! {
                item = rx.recv() => match item {
                    Some(item) => batch.push(item),
                    None => { closed = true; break; }
                },
                () = tokio::time::sleep_until(deadline) => break,
            }
        }
        if !flush_forgets(batch, &out, &shared).await || closed {
            return;
        }
    }
}

/// Send one `FORGET` frame. `false` means the writer is gone.
async fn flush_forgets(
    items: Vec<(NodeId, u64)>,
    out: &mpsc::Sender<Outbound>,
    shared: &Shared,
) -> bool {
    let body = match postcard::to_allocvec(&ForgetRequest { items }) {
        Ok(body) => body,
        Err(e) => {
            // Unreachable for a vector of integer pairs; dropping the batch
            // leaks lookup counts until the session ends, which beats a panic
            // in a detached task.
            tracing::error!(error = %e, "encoding a FORGET batch failed");
            return true;
        }
    };
    debug_assert!(body.len() <= MAX_BODY_SIZE as usize);
    let queued = out
        .send(Outbound {
            id: shared.next_id(),
            op: Opcode::Forget as u16,
            // No reply will come back, which is exactly why this frame spends
            // no window permit: there would be nothing to return it.
            flags: FLAG_NO_REPLY,
            body,
            data: Vec::new(),
        })
        .await;
    queued.is_ok()
}

// ---------------------------------------------------------------------------
// Small shared rules
// ---------------------------------------------------------------------------

/// The largest data segment this client may *send* on an opcode.
///
/// The exact mirror of the server's `data_limit`, and it has to be: every
/// number here is one the server treats as connection-fatal. `WRITE` rides the
/// negotiated I/O budget because that number *is* `max_write`; a `SETXATTR`
/// value rides the body maximum, because an xattr is not I/O and does not
/// travel on the I/O budget.
fn outbound_data_limit(op: Opcode, max_io_size: u32) -> u32 {
    match op {
        Opcode::Write => max_io_size,
        Opcode::Setxattr => MAX_BODY_SIZE,
        _ => 0,
    }
}

/// The largest data segment this client will *accept*.
///
/// `max(negotiated, MAX_BODY_SIZE)` rather than the negotiated size alone:
/// `GETXATTR` and `LISTXATTR` answer in the data segment but are bounded by the
/// body maximum, so a session that settled on 4096 can still legally be handed
/// 64 KiB of attribute value. Bounding by the smaller number would make the
/// client kill its own connection on a reply the server was right to send.
fn inbound_data_bound(settled: &HelloReply) -> u32 {
    settled.max_io_size.max(MAX_BODY_SIZE)
}

fn encode<T: Serialize>(op: Opcode, req: &T) -> Result<Vec<u8>, Errno> {
    postcard::to_allocvec(req).map_err(|e| {
        tracing::error!(?op, error = %e, "encoding a request failed");
        Errno::EIO
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settled(max_io_size: u32) -> HelloReply {
        HelloReply {
            version: PROTOCOL_VERSION,
            max_inflight: 128,
            max_io_size,
            max_body_size: MAX_BODY_SIZE,
        }
    }

    #[test]
    fn only_bulk_opcodes_may_carry_data_outbound() {
        // The mirror of the server's `data_limit`. If these two ever disagree
        // the client sends a frame the server closes the connection over.
        assert_eq!(outbound_data_limit(Opcode::Write, 1 << 20), 1 << 20);
        assert_eq!(
            outbound_data_limit(Opcode::Setxattr, 1 << 20),
            MAX_BODY_SIZE
        );
        assert_eq!(outbound_data_limit(Opcode::Lookup, 1 << 20), 0);
        assert_eq!(outbound_data_limit(Opcode::Read, 1 << 20), 0);
        // A small negotiated I/O size bounds WRITE and nothing else.
        assert_eq!(outbound_data_limit(Opcode::Write, 4096), 4096);
        assert_eq!(outbound_data_limit(Opcode::Setxattr, 4096), MAX_BODY_SIZE);
    }

    #[test]
    fn inbound_data_never_drops_below_the_body_maximum() {
        // The ruling this client exists to obey: a 4096-byte I/O session still
        // receives xattr values up to the body maximum.
        assert_eq!(inbound_data_bound(&settled(4096)), MAX_BODY_SIZE);
        assert_eq!(inbound_data_bound(&settled(MAX_BODY_SIZE)), MAX_BODY_SIZE);
        assert_eq!(inbound_data_bound(&settled(1 << 20)), 1 << 20);
    }

    #[test]
    fn settled_limits_outside_the_protocol_are_refused() {
        let proposal = Proposal::default();
        assert!(check_settled(&proposal, &settled(1 << 20)).is_ok());

        let below_floor = HelloReply {
            max_io_size: 2048,
            ..settled(0)
        };
        assert!(check_settled(&proposal, &below_floor).is_err());

        let over_ask = HelloReply {
            max_io_size: (1 << 20) + 1,
            ..settled(0)
        };
        assert!(check_settled(&proposal, &over_ask).is_err());

        let wide_window = HelloReply {
            max_inflight: WINDOW_CLAMP.1 + 1,
            ..settled(1 << 20)
        };
        assert!(check_settled(&proposal, &wide_window).is_err());

        let over_window = HelloReply {
            max_inflight: 129,
            ..settled(1 << 20)
        };
        assert!(check_settled(&proposal, &over_window).is_err());

        let wrong_version = HelloReply {
            version: PROTOCOL_VERSION + 1,
            ..settled(1 << 20)
        };
        assert!(check_settled(&proposal, &wrong_version).is_err());

        let huge_body = HelloReply {
            max_body_size: MAX_BODY_SIZE + 1,
            ..settled(1 << 20)
        };
        assert!(check_settled(&proposal, &huge_body).is_err());
    }

    #[test]
    fn a_floored_proposal_may_settle_above_what_it_asked_for() {
        // The server floors a sub-page ask up to 4096 and clamps a tiny window
        // up to 8; a client that treated its own proposal as a ceiling would
        // refuse the correct answer.
        let tiny = Proposal {
            max_inflight: 1,
            max_io_size: 1,
            writeback: false,
        };
        let answer = HelloReply {
            version: PROTOCOL_VERSION,
            max_inflight: WINDOW_CLAMP.0,
            max_io_size: MIN_IO_SIZE,
            max_body_size: MAX_BODY_SIZE,
        };
        assert!(check_settled(&tiny, &answer).is_ok());
    }
}

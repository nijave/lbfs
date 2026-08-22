//! The wire side of the server: accept loop, per-connection session, and the
//! frame plumbing between them.
//!
//! # Shape of a session
//!
//! ```text
//!            ┌──────────── read loop ────────────┐
//!  socket ──▶│ header → limits → window → body   │──▶ tokio::spawn(dispatch)
//!            └───────────────────────────────────┘             │
//!                       ▲                                      ▼
//!                       │  Notify (writer died)          mpsc<OutFrame>
//!                       │                                      │
//!            ┌──────────┴────── writer task ─────────────┐     │
//!  socket ◀──│ sole writer; releases the window permit   │◀────┘
//!            └───────────────────────────────────────────┘
//! ```
//!
//! One task reads, one task writes, and one task per in-flight request sits in
//! between. Out-of-order completion falls out of that: a `READ` waiting on a
//! disk never blocks the `GETATTR` behind it, and the client re-associates
//! answers by `request_id` (spec §4).
//!
//! # Three invariants worth stating plainly
//!
//! * **Nothing is allocated on an unchecked length.** Body, data segment, and
//!   the `size` of a `READ` are all compared against the negotiated maxima
//!   *before* a buffer exists to hold them. A length past the maximum is
//!   connection-fatal, not an error reply — the protocol has no in-band
//!   recovery, and a client that miscounted has already desynchronized the
//!   stream (spec §3.1).
//! * **The window permit outlives the reply.** It is released by the writer
//!   after the reply is on the socket, not by the handler after it queues one.
//!   The client is entitled to send a new request the instant it sees an
//!   answer, and releasing earlier or later than the socket write would make
//!   the two sides disagree about the window — in one direction a spurious
//!   violation, in the other an unenforced limit.
//! * **A reply that was produced gets sent.** `LOOKUP`, `MKDIR`, `CREATE` and
//!   friends hand the client a lookup count the moment they succeed;
//!   swallowing that reply would strand the count for the life of the session.
//!   So the session drains the writer before it returns, and the only thing
//!   that discards a produced reply is the connection dying underneath it.

pub mod dispatch;

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Once};
use std::time::Duration;

use lbfs_proto::frame::{
    FrameHeader, FLAG_NO_REPLY, MAGIC, MAX_BODY_SIZE, PROTOCOL_VERSION, STATUS_ATTACH_DENIED,
    STATUS_NOT_EXPORTED, STATUS_OK, STATUS_VERSION_MISMATCH, WINDOW_CLAMP,
};
use lbfs_proto::io::{read_body, read_header, write_frame, IoError};
use lbfs_proto::ops::{
    AttachReply, AttachRequest, ForgetRequest, HelloReply, HelloRequest, Opcode, ReadRequest,
};
use lbfs_proto::types::ROOT_NODE;
use lbfs_proto::Errno;
use tokio::io::AsyncReadExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify, OwnedSemaphorePermit, Semaphore};

use crate::config::{Allowlist, AttachError, Config};
use crate::fs::local::buffers::BufferPool;
use crate::fs::local::uring::UringExecutor;
use crate::fs::local::LocalFs;
use crate::fs::FileSystem;
use dispatch::{dispatch, DataPayload};

/// The smallest I/O ceiling the server will settle on.
///
/// `parse_size` accepts `"0"` and a client may propose anything, so the
/// negotiated value is floored rather than trusted: a sub-page maximum would
/// make ordinary page-sized reads and writes protocol violations, turning a
/// typo in a config file into a mount that dies on first use.
pub const MIN_IO_SIZE: u32 = 4096;

/// One ring lane, deep enough that the in-flight window (1024 at most) queues
/// in the lane's backlog rather than being refused.
const URING_THREADS: usize = 1;
const URING_ENTRIES: u32 = 256;

/// Detect a peer that vanished without a FIN in roughly half a minute, so a
/// session's node table and open descriptors are not held forever by a client
/// that was power-cycled (spec §8).
const KEEPALIVE_IDLE: Duration = Duration::from_secs(10);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
const KEEPALIVE_COUNT: u32 = 3;

/// Pause before retrying an accept that failed for a reason time can fix.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// Server-wide state
// ---------------------------------------------------------------------------

/// What every session on this server shares.
///
/// The io_uring lanes and the buffer pool are per-server rather than
/// per-connection: a lane is an OS thread, and a pool that resets on every
/// disconnect would allocate a megabyte a request for the first while of each
/// new mount. Everything a session must *not* share — the node table, the
/// handle tables, the negotiated limits, the export root — lives in the
/// session's own `LocalFs` instead.
pub struct Server {
    cfg: Arc<Config>,
    allow: Arc<Allowlist>,
    uring: UringExecutor,
    pool: BufferPool,
    /// The server's own I/O ceiling, floored. Also the size of a pooled
    /// buffer, which is what makes any negotiated maximum fit in one.
    max_io_size: u32,
}

impl Server {
    pub fn new(cfg: Arc<Config>, allow: Arc<Allowlist>) -> io::Result<Arc<Server>> {
        init_process()?;
        let max_io_size = cfg.max_io_size.max(MIN_IO_SIZE);
        let uring = UringExecutor::new(URING_THREADS, URING_ENTRIES)?;
        // Two buffers per in-flight request: one can be in the ring while its
        // successor is being read off the socket. `max(1)` because a pool that
        // retains nothing would allocate and free a full-size buffer on every
        // single request.
        let pooled = (2 * cfg.max_inflight as usize).max(1);
        Ok(Arc::new(Server {
            cfg,
            allow,
            uring,
            pool: BufferPool::new(max_io_size as usize, pooled),
            max_io_size,
        }))
    }
}

/// Process-level setup that must happen before the first client is served.
///
/// * **`umask(0)`.** A mode arriving from a client has already been through
///   *that* machine's umask; applying the server's on top would silently strip
///   bits the client asked for, and a file created over lbfs would come out
///   with different permissions than the same call on a local disk. virtiofsd
///   and every other passthrough server do the same. Once per process, since
///   this is global state and a second call would race a concurrent `open`.
/// * **The `/proc` probe.** `LocalFs` reopens `O_PATH` descriptors through
///   `/proc/self/fd/N` for chmod, utimens, truncate and xattrs, and `ATTACH`
///   reads the same path to verify the export root. Without `/proc` mounted
///   none of that works, so a server started in a namespace that lacks it must
///   refuse to start rather than answer `ENOENT` to a client's `chmod` an hour
///   later.
fn init_process() -> io::Result<()> {
    static UMASK: Once = Once::new();
    UMASK.call_once(|| {
        rustix::process::umask(rustix::fs::Mode::empty());
    });
    probe_proc()
}

fn probe_proc() -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let root = std::fs::File::open("/")?;
    let link = std::fs::read_link(format!("/proc/self/fd/{}", root.as_raw_fd()))?;
    if link != Path::new("/") {
        return Err(io::Error::other(
            "/proc/self/fd does not resolve descriptors; is /proc mounted?",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Accept loop
// ---------------------------------------------------------------------------

/// Serve until the listener fails unrecoverably.
///
/// The listener is passed in already bound so a caller — a test, or a future
/// socket-activated launch — can choose the address, including `127.0.0.1:0`
/// for an OS-assigned port.
pub async fn serve(
    listener: TcpListener,
    cfg: Arc<Config>,
    allow: Arc<Allowlist>,
) -> io::Result<()> {
    accept_loop(listener, Server::new(cfg, allow)?).await
}

/// The same, for a caller that already built the shared state.
pub async fn serve_with(listener: TcpListener, server: Arc<Server>) -> io::Result<()> {
    accept_loop(listener, server).await
}

async fn accept_loop(listener: TcpListener, server: Arc<Server>) -> io::Result<()> {
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            // Out of descriptors, out of buffers, or a connection that died
            // during the handshake: the listener is still good, so back off
            // briefly rather than taking the whole server down with one
            // client's bad luck.
            Err(e) if transient_accept_error(&e) => {
                tracing::warn!(error = %e, "accept failed; retrying");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
            Err(e) => return Err(e),
        };
        if let Err(e) = configure_socket(&sock) {
            tracing::warn!(%peer, error = %e, "socket options failed; dropping connection");
            continue;
        }
        tracing::debug!(%peer, "connection accepted");
        let server = Arc::clone(&server);
        tokio::spawn(async move { run_session(sock, server).await });
    }
}

fn transient_accept_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionAborted | io::ErrorKind::Interrupted | io::ErrorKind::OutOfMemory
    ) || matches!(
        e.raw_os_error(),
        Some(libc::EMFILE) | Some(libc::ENFILE) | Some(libc::ENOBUFS) | Some(libc::EPROTO)
    )
}

/// `TCP_NODELAY` plus keepalive (spec §3.1, §8).
///
/// Nagle would hold a small reply back waiting for a companion that a
/// request/response protocol never sends, adding up to 40 ms to every metadata
/// round trip. Keepalive is the other half: without it a client that lost
/// power leaves its session — node table, open files, pooled buffers — resident
/// until the server restarts.
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
// Session
// ---------------------------------------------------------------------------

/// What the handshake settled, for the life of the connection.
#[derive(Debug, Clone, Copy)]
struct Limits {
    max_inflight: u32,
    max_io_size: u32,
    /// The client's `FUSE_WRITEBACK_CACHE` state, which decides how `LocalFs`
    /// reads `OPEN` flags. Not negotiated: only the client knows it.
    writeback: bool,
}

#[derive(Debug, thiserror::Error)]
enum SessionError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    #[error("encoding a reply failed: {0}")]
    Encode(postcard::Error),
}

impl From<IoError> for SessionError {
    fn from(e: IoError) -> SessionError {
        match e {
            IoError::Io(e) => SessionError::Io(e),
            IoError::Protocol(why) => SessionError::Protocol(why),
        }
    }
}

/// Handshake, attach, then serve frames until the connection ends.
pub async fn run_session(sock: TcpStream, server: Arc<Server>) {
    let peer = sock.peer_addr().ok();
    match session(sock, server).await {
        Ok(()) => tracing::debug!(?peer, "session closed"),
        // A violation is the client's bug and the connection is already gone;
        // it is logged loudly enough to debug a client against, and never
        // answered - the protocol has no in-band error recovery (spec §3.1).
        Err(e @ SessionError::Protocol(_)) => tracing::warn!(?peer, error = %e, "session aborted"),
        Err(e) => tracing::debug!(?peer, error = %e, "session ended"),
    }
}

async fn session(mut sock: TcpStream, server: Arc<Server>) -> Result<(), SessionError> {
    let Some(limits) = hello(&mut sock, &server).await? else {
        return Ok(());
    };
    let Some(fs) = attach(&mut sock, &server, limits).await? else {
        return Ok(());
    };
    serve_requests(sock, server, limits, fs).await
}

/// Step 1: settle the protocol version and the limits (spec §3.2).
///
/// `Ok(None)` means the client was rejected and told why; the caller closes.
async fn hello(sock: &mut TcpStream, server: &Server) -> Result<Option<Limits>, SessionError> {
    let hdr = read_header(sock).await?;
    if hdr.op_or_status != Opcode::Hello as u16 {
        return Err(SessionError::Protocol("first frame must be HELLO"));
    }
    if hdr.data_len != 0 {
        return Err(SessionError::Protocol("HELLO carries no data segment"));
    }
    let body = read_body(sock, hdr.body_len, MAX_BODY_SIZE).await?;
    let req: HelloRequest =
        postcard::from_bytes(&body).map_err(|_| SessionError::Protocol("malformed HELLO body"))?;

    if req.magic != MAGIC || req.version != PROTOCOL_VERSION {
        tracing::info!(
            version = req.version,
            "rejecting client: version or magic mismatch"
        );
        // Empty body, like every other non-OK status: a client that cannot
        // agree on the version cannot be assumed to parse this version's
        // reply struct either.
        reply(sock, hdr.request_id, STATUS_VERSION_MISMATCH, &[]).await?;
        return Ok(None);
    }

    // Each side proposes, the smaller wins, and the result is bounded by what
    // the protocol allows regardless of what either side asked for.
    let limits = Limits {
        max_inflight: req
            .max_inflight
            .min(server.cfg.max_inflight)
            .clamp(WINDOW_CLAMP.0, WINDOW_CLAMP.1),
        max_io_size: req.max_io_size.min(server.max_io_size).max(MIN_IO_SIZE),
        writeback: req.writeback,
    };
    let body = encode(&HelloReply {
        version: PROTOCOL_VERSION,
        max_inflight: limits.max_inflight,
        max_io_size: limits.max_io_size,
        max_body_size: MAX_BODY_SIZE,
    })?;
    reply(sock, hdr.request_id, STATUS_OK, &body).await?;
    Ok(Some(limits))
}

/// Step 2: open the export root, verify it, and report its attributes.
async fn attach(
    sock: &mut TcpStream,
    server: &Server,
    limits: Limits,
) -> Result<Option<Arc<dyn FileSystem>>, SessionError> {
    let hdr = read_header(sock).await?;
    if hdr.op_or_status != Opcode::Attach as u16 {
        return Err(SessionError::Protocol("second frame must be ATTACH"));
    }
    if hdr.data_len != 0 {
        return Err(SessionError::Protocol("ATTACH carries no data segment"));
    }
    let body = read_body(sock, hdr.body_len, MAX_BODY_SIZE).await?;
    let req: AttachRequest =
        postcard::from_bytes(&body).map_err(|_| SessionError::Protocol("malformed ATTACH body"))?;
    // A path is bytes, not text: `OsStr::from_bytes` is the only conversion
    // that round-trips a filename the backing filesystem actually holds.
    let requested = Path::new(OsStr::from_bytes(&req.path));

    // Blocking work, deliberately inline: one `open` and one `readlink`, once
    // per connection, before anything is being served.
    let root = match server.allow.open_export(requested) {
        Ok(fd) => fd,
        Err(e) => {
            tracing::info!(path = %requested.display(), error = %e, "attach refused");
            let status = match e {
                AttachError::NotExported => STATUS_NOT_EXPORTED,
                AttachError::Denied => STATUS_ATTACH_DENIED,
            };
            reply(sock, hdr.request_id, status, &[]).await?;
            return Ok(None);
        }
    };

    let fs = match LocalFs::from_root_fd(
        root,
        server.cfg.fsync,
        limits.writeback,
        server.uring.clone(),
        server.pool.clone(),
    ) {
        Ok(fs) => Arc::new(fs) as Arc<dyn FileSystem>,
        Err(e) => {
            tracing::warn!(path = %requested.display(), error = %e, "export root unusable");
            reply(sock, hdr.request_id, Errno::from_io(&e).0, &[]).await?;
            return Ok(None);
        }
    };
    let root_attr = match fs.getattr(ROOT_NODE, None).await {
        Ok(attr) => attr,
        Err(e) => {
            reply(sock, hdr.request_id, e.0, &[]).await?;
            return Ok(None);
        }
    };
    let body = encode(&AttachReply { root_attr })?;
    reply(sock, hdr.request_id, STATUS_OK, &body).await?;
    tracing::info!(path = %requested.display(), writeback = limits.writeback, "attached");
    Ok(Some(fs))
}

/// A frame the session writes itself, before the writer task exists.
async fn reply(
    sock: &mut TcpStream,
    request_id: u64,
    status: u16,
    body: &[u8],
) -> Result<(), SessionError> {
    let hdr = FrameHeader {
        request_id,
        op_or_status: status,
        flags: 0,
        body_len: body.len() as u32,
        data_len: 0,
    };
    write_frame(sock, hdr, body, &[]).await?;
    Ok(())
}

fn encode<T: serde::Serialize>(v: &T) -> Result<Vec<u8>, SessionError> {
    postcard::to_allocvec(v).map_err(SessionError::Encode)
}

// ---------------------------------------------------------------------------
// Request loop
// ---------------------------------------------------------------------------

/// One reply on its way to the socket.
struct OutFrame {
    request_id: u64,
    status: u16,
    body: Vec<u8>,
    data: Option<DataPayload>,
    /// The in-flight window permit this request consumed.
    ///
    /// Carried here rather than dropped by the handler so that the writer
    /// releases it *after* the reply bytes are on the socket. The client may
    /// send its next request the moment it reads an answer; a permit released
    /// only after that request arrives would look like a window overrun and
    /// kill a perfectly well-behaved connection.
    permit: Option<OwnedSemaphorePermit>,
}

/// Everything the read loop needs, once the handshake has settled it.
struct Session {
    server: Arc<Server>,
    limits: Limits,
    fs: Arc<dyn FileSystem>,
    /// Sender for the session itself; handler tasks get clones.
    tx: mpsc::Sender<OutFrame>,
    window: Arc<Semaphore>,
}

async fn serve_requests(
    sock: TcpStream,
    server: Arc<Server>,
    limits: Limits,
    fs: Arc<dyn FileSystem>,
) -> Result<(), SessionError> {
    let (mut reader, writer_half) = sock.into_split();
    // Bounded by the window: a reply cannot be queued for a request that was
    // never admitted, so this can never be the thing that grows without limit.
    let (tx, rx) = mpsc::channel::<OutFrame>(limits.max_inflight as usize);
    let socket_dead = Arc::new(Notify::new());
    let writer = tokio::spawn(writer_task(writer_half, rx, Arc::clone(&socket_dead)));
    let session = Session {
        server,
        limits,
        fs,
        tx,
        window: Arc::new(Semaphore::new(limits.max_inflight as usize)),
    };

    let result = read_loop(&session, &mut reader, &socket_dead).await;

    // Drop the session's own sender, then wait for the writer. Handler tasks
    // still hold clones, so this drains every reply that was produced before
    // the socket closes - see the module's third invariant.
    drop(session);
    if let Err(e) = writer.await {
        tracing::error!(error = %e, "writer task failed");
    }
    result
}

async fn read_loop(
    session: &Session,
    reader: &mut OwnedReadHalf,
    socket_dead: &Notify,
) -> Result<(), SessionError> {
    let (server, limits, fs, tx, window) = (
        &session.server,
        &session.limits,
        &session.fs,
        &session.tx,
        &session.window,
    );
    loop {
        let hdr = tokio::select! {
            got = read_header(reader) => match got {
                Ok(hdr) => hdr,
                // The client hung up. Losing a partially read header with it
                // does not matter: there is nothing left to be in sync with.
                Err(e) if disconnected(&e) => return Ok(()),
                Err(e) => return Err(e.into()),
            },
            // The writer could not write. Reading on would only produce
            // answers with nowhere to go.
            () = socket_dead.notified() => return Ok(()),
        };

        // Every size check happens here, before a byte is allocated for the
        // frame it describes (spec §3.1).
        if hdr.body_len > MAX_BODY_SIZE {
            return Err(SessionError::Protocol("body_len exceeds MAX_BODY_SIZE"));
        }
        let op = Opcode::try_from(hdr.op_or_status)
            .map_err(|_| SessionError::Protocol("unknown opcode"))?;
        if matches!(op, Opcode::Hello | Opcode::Attach) {
            return Err(SessionError::Protocol(
                "HELLO or ATTACH after the handshake",
            ));
        }
        if hdr.data_len > data_limit(op, limits) {
            return Err(SessionError::Protocol(
                "data_len exceeds the limit for this opcode",
            ));
        }

        // FORGET is exempt from the window on purpose. It carries NO_REPLY, so
        // the client has no answer to count against its own accounting and
        // cannot know when a permit would free; charging it one would let a
        // legal batch of forgets, sent while the window is full, look like an
        // overrun. It costs nothing to leave out: forgets run inline here, so
        // no number of them buys the client any concurrency.
        let permit = match op {
            Opcode::Forget => None,
            _ => match Arc::clone(window).try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => return Err(SessionError::Protocol("in-flight window overrun")),
            },
        };

        let body = read_body(reader, hdr.body_len, MAX_BODY_SIZE).await?;
        let data = read_data(reader, op, hdr.data_len as usize, server).await?;

        if op == Opcode::Forget {
            // Inline, because it is a refcount decrement per item and spawning
            // a task for it would cost more than doing it. A malformed batch
            // is fatal rather than ignored: there is no reply to carry an
            // error, and silently dropping it leaks every node it named.
            let req: ForgetRequest = postcard::from_bytes(&body)
                .map_err(|_| SessionError::Protocol("malformed FORGET body"))?;
            if hdr.flags & FLAG_NO_REPLY == 0 {
                tracing::debug!("FORGET without NO_REPLY; answering nothing regardless");
            }
            for (node, nlookup) in req.items {
                fs.forget(node, nlookup).await;
            }
            continue;
        }

        if op == Opcode::Read {
            // The one request whose *body* names a size the server would have
            // to allocate for. Checked here rather than in dispatch because
            // the answer is to close the connection, which dispatch has no way
            // to say. A body that will not decode is left for dispatch to
            // answer with EINVAL.
            if let Ok(req) = postcard::from_bytes::<ReadRequest>(&body) {
                if req.size > limits.max_io_size {
                    return Err(SessionError::Protocol(
                        "READ size exceeds negotiated max_io_size",
                    ));
                }
            }
        }

        let request_id = hdr.request_id;
        let fs = Arc::clone(fs);
        let tx = tx.clone();
        tokio::spawn(async move {
            let (status, body, data) = dispatch(op, &body, data, &fs).await;
            // A send failure means the writer is gone, which means the
            // connection is gone: the reply has nowhere to go and the session
            // is already unwinding.
            let _ = tx
                .send(OutFrame {
                    request_id,
                    status,
                    body,
                    data,
                    permit,
                })
                .await;
        });
    }
}

/// The largest data segment this opcode may carry.
///
/// Zero for everything but the two ops that carry bulk bytes inbound (spec
/// §3.1). `SETXATTR` is bounded by the body maximum, which is the kernel's own
/// `XATTR_SIZE_MAX`, and by the negotiated I/O size as well — no frame gets to
/// exceed what the two sides settled on, whatever else the op allows.
fn data_limit(op: Opcode, limits: &Limits) -> u32 {
    match op {
        Opcode::Write => limits.max_io_size,
        Opcode::Setxattr => MAX_BODY_SIZE.min(limits.max_io_size),
        _ => 0,
    }
}

/// Read the data segment into storage that suits where it is going.
///
/// A short read here is fatal, not a partial write: the bytes are already gone
/// from the stream and the next header would be read out of the middle of a
/// payload.
async fn read_data(
    reader: &mut OwnedReadHalf,
    op: Opcode,
    len: usize,
    server: &Server,
) -> Result<Option<DataPayload>, SessionError> {
    match op {
        Opcode::Write => {
            let mut buf = server.pool.get();
            if len > buf.capacity() {
                // Unreachable while pooled buffers are sized to the server's
                // ceiling and `data_limit` is bounded by the negotiated one,
                // but the alternative to checking is a panic in `set_len`.
                return Err(SessionError::Protocol("data_len exceeds the pooled buffer"));
            }
            reader.read_exact(&mut buf.as_mut_slice()[..len]).await?;
            // The initialized prefix is the contract with `LocalFs::write`: it
            // bounds the write by this length, so it must be the count that
            // was actually read and never the header's claim.
            buf.set_len(len);
            Ok(Some(DataPayload::Pooled(buf)))
        }
        Opcode::Setxattr => {
            // Not pooled: an xattr value is at most 64 KiB and is handed to
            // the syscall as a plain slice, so it has no business evicting a
            // megabyte-sized I/O buffer from the pool.
            let mut value = vec![0u8; len];
            reader.read_exact(&mut value).await?;
            Ok(Some(DataPayload::Owned(value)))
        }
        // `data_limit` already refused a non-zero data segment on anything
        // else, so there is nothing to read.
        _ => Ok(None),
    }
}

/// The sole writer. Everything that answers a client goes through here, which
/// is what keeps two concurrent replies from interleaving their bytes.
async fn writer_task(
    mut sock: OwnedWriteHalf,
    mut rx: mpsc::Receiver<OutFrame>,
    socket_dead: Arc<Notify>,
) {
    while let Some(frame) = rx.recv().await {
        let OutFrame {
            request_id,
            status,
            body,
            data,
            permit,
        } = frame;
        let bytes = data.as_ref().map(DataPayload::as_slice).unwrap_or(&[]);
        let hdr = FrameHeader {
            request_id,
            op_or_status: status,
            flags: 0,
            body_len: body.len() as u32,
            data_len: bytes.len() as u32,
        };
        if let Err(e) = write_frame(&mut sock, hdr, &body, bytes).await {
            tracing::debug!(error = %e, "reply write failed; ending session");
            // Wake the reader; a half-open socket would otherwise leave it
            // blocked on a header that will never be answered.
            socket_dead.notify_one();
            return;
        }
        // Explicit, and in this order on purpose: the window opens up only
        // once the answer it was holding a slot for is on the wire.
        drop(permit);
    }
}

/// Whether a read error is the ordinary end of a connection rather than a
/// fault worth reporting.
fn disconnected(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_probe_succeeds_on_a_normal_system() {
        // The trick `LocalFs` and `ATTACH` are built on. If this fails the
        // server is right to refuse to start.
        probe_proc().unwrap();
    }

    #[test]
    fn only_bulk_opcodes_may_carry_data() {
        let limits = Limits {
            max_inflight: 8,
            max_io_size: 1 << 20,
            writeback: false,
        };
        assert_eq!(data_limit(Opcode::Write, &limits), 1 << 20);
        assert_eq!(data_limit(Opcode::Setxattr, &limits), MAX_BODY_SIZE);
        assert_eq!(data_limit(Opcode::Lookup, &limits), 0);
        assert_eq!(data_limit(Opcode::Read, &limits), 0);
        // A small negotiated I/O size bounds the xattr value too: no frame
        // exceeds what the handshake settled.
        let tight = Limits {
            max_io_size: 4096,
            ..limits
        };
        assert_eq!(data_limit(Opcode::Setxattr, &tight), 4096);
    }
}

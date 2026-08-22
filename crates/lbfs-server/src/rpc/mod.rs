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

/// How long a closing session waits for the replies it already produced.
///
/// The drain exists so an `Entry` a backend handed out reaches the client that
/// owes it a `FORGET`; the bound exists because one handler that never returns
/// — `OPEN` on a peerless FIFO, parked on a blocking thread — would otherwise
/// hold the session task, the socket, and the whole export's node table for as
/// long as the process lives. Thirty seconds is far longer than any operation
/// this server issues and far shorter than "forever".
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

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
/// * **`RLIMIT_NOFILE` raised to its hard ceiling.** The node table holds one
///   `O_PATH` descriptor per node a client still remembers, so this process's
///   soft descriptor limit *is* the largest tree a client may have live at
///   once. At the traditional 1024 that ceiling arrives around the thousandth
///   file, which a build tree — the workload this filesystem exists for —
///   reaches without trying. Worse, a client that hits it cannot clean up
///   through the mount, because `rm -r` needs a descriptor to read the
///   directory; only forgetting the nodes gets the export back. virtiofsd
///   raises the same limit at startup for the same reason.
/// * **The `/proc` probe.** `LocalFs` reopens `O_PATH` descriptors through
///   `/proc/self/fd/N` for chmod, utimens, truncate and xattrs, and `ATTACH`
///   reads the same path to verify the export root. Without `/proc` mounted
///   none of that works, so a server started in a namespace that lacks it must
///   refuse to start rather than answer `ENOENT` to a client's `chmod` an hour
///   later.
fn init_process() -> io::Result<()> {
    static SETUP: Once = Once::new();
    SETUP.call_once(|| {
        rustix::process::umask(rustix::fs::Mode::empty());
        raise_nofile();
    });
    probe_proc()
}

/// Move the soft descriptor limit up to the hard one.
///
/// Not fatal if it fails. A container can be handed a hard limit lower than the
/// soft limit this process would like, and a server that refused to start over
/// it would be trading a workload ceiling for no server at all. The number it
/// settled on goes in the log either way, because when a client does meet
/// `EMFILE` this line is the first thing worth reading.
fn raise_nofile() {
    use rustix::process::{getrlimit, setrlimit, Resource};

    let before = getrlimit(Resource::Nofile);
    let Some(target) = nofile_target(before) else {
        tracing::info!(nofile = %nofile_label(before.current), "descriptor limit already at its ceiling");
        return;
    };
    match setrlimit(Resource::Nofile, target) {
        Ok(()) => tracing::info!(
            nofile = %nofile_label(getrlimit(Resource::Nofile).current),
            was = %nofile_label(before.current),
            "raised the descriptor limit to its hard ceiling"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            nofile = %nofile_label(before.current),
            hard = %nofile_label(before.maximum),
            "could not raise the descriptor limit; large trees may meet EMFILE"
        ),
    }
}

/// The limit to install, or `None` when the soft limit is already the hard one.
///
/// Split out from the syscalls so the decision can be tested on a process whose
/// own limits are whatever the developer's shell happened to set.
fn nofile_target(limit: rustix::process::Rlimit) -> Option<rustix::process::Rlimit> {
    if limit.current == limit.maximum {
        return None;
    }
    Some(rustix::process::Rlimit {
        current: limit.maximum,
        maximum: limit.maximum,
    })
}

/// `None` is `RLIM_INFINITY`, which reads better as a word than as `None`.
fn nofile_label(limit: Option<u64>) -> String {
    limit.map_or_else(|| "unlimited".to_string(), |n| n.to_string())
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
    /// Raised when the socket can no longer carry replies, by whoever finds
    /// out first: the writer on a failed write, or a handler whose reply has
    /// no writer left to take it.
    socket_dead: Arc<Notify>,
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
    let mut writer = tokio::spawn(writer_task(writer_half, rx, Arc::clone(&socket_dead)));
    let session = Session {
        server,
        limits,
        fs,
        tx,
        window: Arc::new(Semaphore::new(limits.max_inflight as usize)),
        socket_dead: Arc::clone(&socket_dead),
    };

    let result = read_loop(&session, &mut reader, &socket_dead).await;

    // Drop the session's own sender, then wait for the writer. Handler tasks
    // still hold clones, so this drains every reply that was produced before
    // the socket closes - see the module's third invariant. Bounded, because
    // a handler that never returns must not turn a closing session into a
    // permanent one.
    drop(session);
    match tokio::time::timeout(DRAIN_TIMEOUT, &mut writer).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(error = %e, "writer task failed"),
        Err(_) => {
            tracing::warn!(
                timeout = ?DRAIN_TIMEOUT,
                "a request never completed; closing the session with replies undelivered"
            );
            // Abort rather than detach: after this long the writer is either
            // idle behind a stuck handler, with everything it had already
            // written, or blocked on a socket the client stopped reading.
            // Neither is worth holding the connection open for, and dropping
            // the write half is what finally tells the client it is over.
            writer.abort();
        }
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
        tokio::spawn(run_request(
            request_id,
            async move { dispatch(op, &body, data, &fs).await },
            tx.clone(),
            permit,
            Arc::clone(&session.socket_dead),
        ));
    }
}

/// Run one request's work, and answer for it even if that work panics.
///
/// The work goes in its own task purely so a panic becomes a `JoinError` here
/// instead of vanishing. A detached handler that panics is the worst
/// failure this layer can have: the client waits forever for a `request_id`
/// that will never be answered, on a socket that is still open and still
/// serving everybody else. And it compounds — the buffer pool and the node
/// table are behind `Mutex`es taken with `.lock().unwrap()`, so a panic while
/// one is held poisons it and every later request panics in the same place.
/// `EIO` per request turns a permanently wedged mount into an error the client
/// can report, which is the same trade `LocalFs`'s `join_errno` already makes
/// for a panicking blocking task.
async fn run_request<F>(
    request_id: u64,
    work: F,
    tx: mpsc::Sender<OutFrame>,
    permit: Option<OwnedSemaphorePermit>,
    socket_dead: Arc<Notify>,
) where
    F: std::future::Future<Output = dispatch::Reply> + Send + 'static,
{
    let (status, body, data) = match tokio::spawn(work).await {
        Ok(reply) => reply,
        Err(e) => {
            tracing::error!(request_id, error = %e, "request handler panicked; answering EIO");
            (Errno::EIO.0, Vec::new(), None)
        }
    };
    let queued = tx
        .send(OutFrame {
            request_id,
            status,
            body,
            data,
            permit,
        })
        .await;
    if queued.is_err() {
        // No writer left to take it, so there is no connection left either.
        // Tell the reader rather than letting it block on a header that can
        // no longer be answered.
        socket_dead.notify_one();
    }
}

/// The largest data segment this opcode may carry.
///
/// Zero for everything but the two ops that carry bulk bytes inbound (spec
/// §3.1). The two are bounded by different numbers on purpose:
///
/// * `WRITE` by the negotiated I/O size, which is what that number *means* —
///   the client asked for `max_write` and this is it.
/// * `SETXATTR` by the max body size, which spec §3.2 names as the bound on
///   xattr values, and *not* also by the negotiated I/O size. Clamping it to
///   both would be asymmetric with the reply side: `GETXATTR` hands back up to
///   `XATTR_SIZE_MAX` in a data segment whatever the session negotiated, so a
///   client that agreed on a 4 KiB I/O size would receive a 16 KiB attribute
///   it could never send back. An xattr is not I/O and does not travel on the
///   I/O budget.
fn data_limit(op: Opcode, limits: &Limits) -> u32 {
    match op {
        Opcode::Write => limits.max_io_size,
        Opcode::Setxattr => MAX_BODY_SIZE,
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
    fn the_descriptor_target_is_the_hard_limit_and_nothing_when_already_there() {
        use rustix::process::Rlimit;

        let raise = |current, maximum| nofile_target(Rlimit { current, maximum });

        // The case that matters: a stock 1024 against a much larger ceiling.
        assert_eq!(
            raise(Some(1024), Some(524_288)),
            Some(Rlimit {
                current: Some(524_288),
                maximum: Some(524_288),
            })
        );
        // An unlimited hard limit means an unlimited soft limit, not a number.
        assert_eq!(
            raise(Some(1024), None),
            Some(Rlimit {
                current: None,
                maximum: None,
            })
        );
        // Already at the ceiling, so no syscall: `setrlimit` would succeed but
        // the log line would claim a raise that did not happen.
        assert_eq!(raise(Some(4096), Some(4096)), None);
        assert_eq!(raise(None, None), None);
        // The hard limit is never touched, in either direction.
        for target in [raise(Some(1), Some(9)), raise(Some(8), Some(9))]
            .into_iter()
            .flatten()
        {
            assert_eq!(target.maximum, Some(9));
        }
    }

    #[test]
    fn init_leaves_the_process_at_its_hard_descriptor_limit() {
        use rustix::process::{getrlimit, Resource};

        // This process shares one limit with every other case in the binary,
        // and `init_process` is behind a `Once`, so by the time this runs the
        // raise may already have happened — or the developer's shell may have
        // handed cargo a soft limit that was the hard limit to begin with.
        // Either way there is nothing left to observe, and saying so is more
        // honest than asserting a tautology.
        let before = getrlimit(Resource::Nofile);
        if before.current == before.maximum {
            eprintln!(
                "skipped: this process already runs at its descriptor ceiling ({})",
                nofile_label(before.current)
            );
            return;
        }

        init_process().unwrap();

        let after = getrlimit(Resource::Nofile);
        assert_eq!(
            after.current, before.maximum,
            "init must leave the soft descriptor limit at the hard one"
        );
        assert_eq!(
            after.maximum, before.maximum,
            "init must not move the hard descriptor limit"
        );
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
        // A small negotiated I/O size bounds WRITE and nothing else: an xattr
        // value is bounded by the body maximum, symmetrically with the
        // GETXATTR reply the same session can already receive.
        let tight = Limits {
            max_io_size: 4096,
            ..limits
        };
        assert_eq!(data_limit(Opcode::Write, &tight), 4096);
        assert_eq!(data_limit(Opcode::Setxattr, &tight), MAX_BODY_SIZE);
    }

    #[tokio::test]
    async fn a_panicking_handler_answers_eio_for_its_own_request() {
        let (tx, mut rx) = mpsc::channel::<OutFrame>(1);
        let dead = Arc::new(Notify::new());
        run_request(
            77,
            async { panic!("a backend that should not have panicked") },
            tx,
            None,
            Arc::clone(&dead),
        )
        .await;
        let frame = rx.recv().await.expect("the request must be answered");
        assert_eq!((frame.request_id, frame.status), (77, Errno::EIO.0));
        assert!(frame.body.is_empty());
    }

    #[tokio::test]
    async fn a_reply_with_no_writer_left_ends_the_session() {
        let (tx, rx) = mpsc::channel::<OutFrame>(1);
        drop(rx); // the writer is gone
        let dead = Arc::new(Notify::new());
        run_request(1, async { (STATUS_OK, Vec::new(), None) }, tx, None, {
            Arc::clone(&dead)
        })
        .await;
        // `notify_one` leaves a permit behind, so the reader's next
        // `notified()` completes even though it was not waiting yet.
        tokio::time::timeout(Duration::from_secs(5), dead.notified())
            .await
            .expect("the reader must be woken");
    }
}

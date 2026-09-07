//! One mount's session with one server, above the connection that carries it.
//!
//! ```text
//!   LbfsFuse ──▶ Session ──▶ Arc<Connection>
//! ```
//!
//! A `Connection` is one socket and dies with it — the fourth invariant in
//! [`crate::conn`], and it does not move. What a mount needs on top of that is
//! an object whose identity survives a socket: the FUSE bridge holds it for the
//! life of the mount, asks it for a connection per callback, and never learns
//! that the socket underneath changed. That object is `Session`, and this is the
//! half of it that exists today.
//!
//! **Nothing here reconnects yet.** The state is written once, at construction,
//! and stays `Live`; a connection that dies stays the current connection and
//! answers `EIO` for ever, exactly as it did when the bridge held it directly.
//! The reconnect supervisor is the next step, and the shape below is what it
//! needs to land without touching the thirty callbacks a second time:
//!
//! * [`Session::current`] is `async` from the start. Today it answers at once;
//!   the supervisor turns `Reconnecting` into a park, and an `fn` here would
//!   mean revisiting every call site to add one `.await`.
//! * The state rides a `watch` channel rather than a mutex, because parking is
//!   what a `watch` receiver already does: wait for the value to change.
//! * The redial data — the address, the export, the proposal and the ticket —
//!   is fixed at construction, so a redial needs nothing the mount would have to
//!   supply again mid-flight.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lbfs_proto::ops::HelloReply;
use lbfs_proto::types::{NodeId, SessionTicket};
use lbfs_proto::Errno;
use tokio::sync::watch;

use crate::conn::{Connection, Proposal};

/// Where this session's traffic goes.
///
/// Only `Live` is ever written today. The other two are the supervisor's to
/// write — `Reconnecting` while it dials, `Dead` when it gives up or the mount
/// shuts down — and they are declared now because `current()`'s three answers
/// are the shape the callbacks are being moved onto. Moving thirty call sites
/// once is the point of declaring them early.
#[allow(dead_code)]
enum State {
    Live(Arc<Connection>),
    /// A redial is in progress. Calls park rather than fail: a request that has
    /// not been sent has no outcome in doubt, so nothing about it needs to fail
    /// (design §3.2).
    Reconnecting,
    /// Over. Every call answers `EIO` and the mount stays unmountable-clean,
    /// which is spec §7's behaviour unchanged.
    Dead,
}

/// A mount's session: the current connection, plus everything a later redial
/// needs.
///
/// Held in an `Arc` by the FUSE bridge, which asks it for a connection inside
/// each task it spawns.
pub struct Session {
    /// The current connection. A channel rather than a lock because the
    /// supervisor's parked callers wait for exactly what a `watch` receiver
    /// waits for: the value changing.
    state: watch::Sender<State>,
    // The redial data. Fixed for the life of the mount, and read by nothing
    // until the supervisor lands — hence the allows, which come off with it.
    #[allow(dead_code)]
    addr: SocketAddr,
    #[allow(dead_code)]
    export: Vec<u8>,
    #[allow(dead_code)]
    proposal: Proposal,
    /// Whatever `ATTACH` handed back, and it is the whole of the authentication
    /// a claim carries. Nothing rotates (design §7.6), so this value serves
    /// every claim this session will ever make.
    #[allow(dead_code)]
    ticket: Option<SessionTicket>,
    /// How long a reconnect may run before the mount dies. Zero disables
    /// reconnection outright, which is today's behaviour for every caller.
    #[allow(dead_code)]
    deadline: Duration,
    /// Forgets dropped by connections this session has already replaced, so the
    /// count `destroy` reports covers the mount rather than its last socket.
    retired_forgets: AtomicU64,
    /// What the first handshake settled.
    ///
    /// The mount reads them once, in `LbfsFuse::init`, and the kernel cannot be
    /// told a new `max_write` or `max_background` afterwards — which is why a
    /// resumed session must present the same numbers (design §3.3).
    pub limits: HelloReply,
}

impl Session {
    /// Wrap a connection that has already handshaken and attached.
    ///
    /// The address, the export and the proposal are the ones that produced
    /// `conn`, and `ticket` is what its `ATTACH` reply carried — `None` for a
    /// server or a caller that wants no retention.
    pub fn new(
        conn: Arc<Connection>,
        addr: SocketAddr,
        export: Vec<u8>,
        proposal: Proposal,
        ticket: Option<SessionTicket>,
        deadline: Duration,
    ) -> Arc<Session> {
        let limits = conn.limits.clone();
        let (state, _) = watch::channel(State::Live(conn));
        Arc::new(Session {
            state,
            addr,
            export,
            proposal,
            ticket,
            deadline,
            retired_forgets: AtomicU64::new(0),
            limits,
        })
    }

    /// The connection to spend this request on, or `EIO` if the mount is over.
    ///
    /// `async` with nothing to await today, deliberately: the supervisor turns
    /// the `Reconnecting` arm into a wait on the state leaving it, and every
    /// caller below is already written to await this.
    ///
    /// A dead connection is not a dead session as far as this method is
    /// concerned — it hands the connection back and the connection answers
    /// `EIO`, which is what the bridge saw before this indirection existed.
    pub async fn current(&self) -> Result<Arc<Connection>, Errno> {
        match &*self.state.borrow() {
            State::Live(conn) => Ok(Arc::clone(conn)),
            // Unreachable until the supervisor writes either one. `EIO` rather
            // than a panic, because a filesystem's answer to "this mount is
            // over" is an errno on both counts.
            State::Reconnecting | State::Dead => Err(Errno::EIO),
        }
    }

    /// Drop a lookup count, fire and forget.
    ///
    /// Synchronous, because the kernel's `forget` callback has no reply object
    /// and runs on the FUSE dispatch thread — the reason
    /// [`Connection::send_forget`] gives, unchanged by the indirection. Which
    /// also means it cannot park: with no connection to hand it to, the forget
    /// is lost and the node stays resident until the session ends.
    pub fn send_forget(&self, node: NodeId, nlookup: u64) {
        match &*self.state.borrow() {
            State::Live(conn) => conn.send_forget(node, nlookup),
            State::Reconnecting | State::Dead => {
                tracing::debug!(node, nlookup, "no connection; dropping FORGET");
            }
        }
    }

    /// How many forgets this mount has thrown away, across every connection it
    /// has had.
    pub fn dropped_forgets(&self) -> u64 {
        let live = match &*self.state.borrow() {
            State::Live(conn) => conn.dropped_forgets(),
            State::Reconnecting | State::Dead => 0,
        };
        self.retired_forgets.load(Ordering::Relaxed) + live
    }
}

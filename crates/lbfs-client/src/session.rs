//! One mount's session with one server, across the connections that carry it.
//!
//! ```text
//!   LbfsFuse ──▶ Session ──▶ Arc<Connection>   (swapped on reconnect)
//!                    │
//!                    └──▶ supervisor: await death → dial → HELLO → RESUME → install
//! ```
//!
//! A `Connection` is one socket and dies with it — the fourth invariant in
//! [`crate::conn`], and it does not move. What a mount needs on top of that is
//! an object whose identity survives a socket: the FUSE bridge holds it for the
//! life of the mount, asks it for a connection per callback, and never learns
//! that the socket underneath changed.
//!
//! # The split this module exists to draw
//!
//! * **In flight at the break → `EIO`, at once.** Those requests belong to the
//!   dead `Connection` and it fails them itself. Nothing here retries one: the
//!   client cannot know whether the server executed it, and a replayed `MKDIR`
//!   answers `EEXIST` for an operation that succeeded (design §3.1).
//! * **Issued during the gap → parked, then run.** A request that has not been
//!   sent has no outcome in doubt, so nothing about it needs to fail
//!   (design §3.2). [`Session::current`] parks it until the supervisor installs
//!   a connection or gives up.
//!
//! The supervisor owns the clock. It writes `Reconnecting` before its first
//! dial and leaves that state inside the deadline whatever happens, which is
//! why the park below carries no timeout of its own — a second clock would be a
//! second answer to a question that already has one.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use lbfs_proto::ops::HelloReply;
use lbfs_proto::types::{NodeId, SessionTicket};
use lbfs_proto::Errno;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::conn::{ConnectError, Connection, Proposal};

/// The first pause between two dials, and the ceiling it doubles to.
///
/// A 200 ms blip costs one retry and a ten-second outage costs a dozen rather
/// than two hundred. The floor matters because most gaps this feature exists
/// for are shorter than a second; the ceiling matters because a client that
/// dials a down server twenty times a second is a client nobody wants in a log.
const BACKOFF_START: Duration = Duration::from_millis(50);
const BACKOFF_CEILING: Duration = Duration::from_secs(1);

/// How long [`Session::shutdown`] waits for its `DETACH` reply.
///
/// Brief on purpose. The session expires by itself when the grace runs out, so
/// a `DETACH` that never lands costs the server a few descriptors for a minute
/// rather than anything permanent — and a client that cannot detach must still
/// exit. This bounds what a server that has stopped answering can add to an
/// unmount.
const DETACH_TIMEOUT: Duration = Duration::from_secs(5);

/// Where this session's traffic goes.
enum State {
    Live(Arc<Connection>),
    /// A redial is in progress. Calls park rather than fail: a request that has
    /// not been sent has no outcome in doubt, so nothing about it needs to fail
    /// (design §3.2).
    ///
    /// Only the supervisor writes it, and it writes it before the first dial,
    /// so a call arriving between the death and that dial parks rather than
    /// seeing a stale `Live`.
    Reconnecting,
    /// Over. Every call answers `EIO` and the mount stays unmountable-clean,
    /// which is spec §7's behaviour unchanged.
    Dead,
}

/// A mount's session: the current connection, plus everything a redial needs.
///
/// Held in an `Arc` by the FUSE bridge, which asks it for a connection inside
/// each task it spawns.
pub struct Session {
    /// The current connection. A channel rather than a lock because the
    /// supervisor's parked callers wait for exactly what a `watch` receiver
    /// waits for: the value changing.
    state: watch::Sender<State>,
    /// The redial data. Fixed for the life of the mount.
    addr: SocketAddr,
    export: Vec<u8>,
    proposal: Proposal,
    /// Whatever `ATTACH` handed back, and it is the whole of the authentication
    /// a claim carries. Nothing rotates (design §7.6), so this value serves
    /// every claim this session will ever make.
    ticket: Option<SessionTicket>,
    /// How long a reconnect may run before the mount dies. Zero disables
    /// reconnection outright, and so does a session with no ticket.
    deadline: Duration,
    /// Forgets dropped by connections this session has already replaced, so the
    /// count `destroy` reports covers the mount rather than its last socket.
    retired_forgets: AtomicU64,
    /// Forgets dropped because they arrived while there was no connection to
    /// take them. Counted apart from the queue-full drops above because it is
    /// the one an operator can act on, by shortening the deadline.
    gap_forgets: AtomicU64,
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
    ///
    /// `deadline` is what the caller asked for, clamped here to what the
    /// server's `HELLO` promised to hold — see [`clamp_deadline`].
    ///
    /// **Spawns the reconnect supervisor**, so a caller that passes a ticket
    /// and a non-zero deadline must be inside a tokio runtime context. A caller
    /// that passes either of the two "off" values needs no runtime and gets no
    /// task: a connection that dies stays the current connection and answers
    /// `EIO` for ever, exactly as it did before this indirection existed.
    pub fn new(
        conn: Arc<Connection>,
        addr: SocketAddr,
        export: Vec<u8>,
        proposal: Proposal,
        ticket: Option<SessionTicket>,
        deadline: Duration,
    ) -> Arc<Session> {
        let limits = conn.limits.clone();
        let deadline = clamp_deadline(
            deadline,
            Duration::from_millis(u64::from(limits.resume_grace_ms)),
        );
        let (state, _) = watch::channel(State::Live(conn));
        let session = Arc::new(Session {
            state,
            addr,
            export,
            proposal,
            ticket,
            deadline,
            retired_forgets: AtomicU64::new(0),
            gap_forgets: AtomicU64::new(0),
            limits,
        });
        // No supervisor unless this session redials, and either half absent
        // means today's behaviour end to end.
        if let (true, Some(ticket)) = (session.reconnects(), session.ticket) {
            tokio::spawn(supervise(Arc::downgrade(&session), ticket));
        }
        session
    }

    /// The connection to spend this request on, or `EIO` if the mount is over.
    ///
    /// `Live` and `Dead` answer at once; anything else parks until the state
    /// can answer, which the supervisor guarantees happens inside the deadline.
    /// No timeout of its own, deliberately — see the module header.
    ///
    /// A dead *connection* is not a dead session. On a session that does not
    /// reconnect it is still the answer: the connection says `EIO` and the
    /// mount stays present until somebody unmounts it, which is spec §7
    /// unchanged. On a session that does, handing it over would be an `EIO` for
    /// a request that has not been sent — exactly the request design §3.2 says
    /// must park — so this waits for the supervisor instead.
    pub async fn current(&self) -> Result<Arc<Connection>, Errno> {
        let reconnects = self.reconnects();
        if let Some(answer) = self.peek(reconnects) {
            return answer;
        }
        // Subscribed after the peek, and correct anyway: `wait_for` reads the
        // current value before it waits, so a state that became answerable in
        // between is seen rather than waited on.
        let mut rx = self.state.subscribe();
        // A closed channel means the sender went away, which cannot happen
        // while this caller holds a reference to the session that owns it.
        // `EIO` rather than a panic regardless: a filesystem's answer to "this
        // mount is over" is an errno either way.
        let Ok(state) = rx.wait_for(|state| answers(state, reconnects)).await else {
            return Err(Errno::EIO);
        };
        match &*state {
            State::Live(conn) => Ok(Arc::clone(conn)),
            State::Reconnecting | State::Dead => Err(Errno::EIO),
        }
    }

    /// The answer, if there is one without waiting.
    ///
    /// Split out so the borrow of the `watch` value ends before any `await`.
    fn peek(&self, reconnects: bool) -> Option<Result<Arc<Connection>, Errno>> {
        let state = self.state.borrow();
        if !answers(&state, reconnects) {
            return None;
        }
        match &*state {
            State::Live(conn) => Some(Ok(Arc::clone(conn))),
            State::Reconnecting | State::Dead => Some(Err(Errno::EIO)),
        }
    }

    /// Whether this session redials at all.
    ///
    /// Both halves are needed: a deadline with no ticket would dial a server
    /// that has nothing to hand back, and a ticket with no deadline is a client
    /// that decided not to wait.
    fn reconnects(&self) -> bool {
        self.ticket.is_some() && !self.deadline.is_zero()
    }

    /// Drop a lookup count, fire and forget.
    ///
    /// Synchronous, because the kernel's `forget` callback has no reply object
    /// and runs on the FUSE dispatch thread — the reason
    /// [`Connection::send_forget`] gives, unchanged by the indirection. Which
    /// also means it cannot park: with no connection to hand it to, the forget
    /// is lost and the node stays resident until the session ends. Bounded by
    /// what the gap can hold and reported at `destroy`, where an operator can
    /// read it against the deadline that produced it (design §9).
    pub fn send_forget(&self, node: NodeId, nlookup: u64) {
        match &*self.state.borrow() {
            State::Live(conn) => conn.send_forget(node, nlookup),
            State::Reconnecting => {
                self.gap_forgets.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(node, nlookup, "reconnecting; dropping FORGET");
            }
            State::Dead => {
                tracing::debug!(node, nlookup, "no connection; dropping FORGET");
            }
        }
    }

    /// How many forgets this mount has thrown away, across every connection it
    /// has had and every gap between them.
    pub fn dropped_forgets(&self) -> u64 {
        let live = match &*self.state.borrow() {
            State::Live(conn) => conn.dropped_forgets(),
            State::Reconnecting | State::Dead => 0,
        };
        self.retired_forgets.load(Ordering::Relaxed)
            + self.gap_forgets.load(Ordering::Relaxed)
            + live
    }

    /// The subset of [`Session::dropped_forgets`] lost because there was no
    /// connection at all.
    pub fn dropped_while_reconnecting(&self) -> u64 {
        self.gap_forgets.load(Ordering::Relaxed)
    }

    /// End the mount: detach the session, stop the supervisor, and fail
    /// everything parked.
    ///
    /// Called after the unmount drain, because the drain flushes writeback and
    /// the `FORGET`s the kernel emits for every evicted inode, and both need
    /// the session. Nothing revives a session marked dead.
    ///
    /// The `DETACH` is what keeps a clean unmount from leaving the server
    /// holding this mount's descriptors for the whole grace (design §7.5). It
    /// goes out only over a live connection of a session that holds a ticket,
    /// and only for as long as [`DETACH_TIMEOUT`] — this is not a caller that
    /// may park behind a redial, and a failure goes to the log and no further:
    /// the session expires by itself, and a client that cannot detach must
    /// still exit.
    pub async fn shutdown(&self) {
        if let (Some(ticket), Some(conn)) = (self.ticket, self.live()) {
            match tokio::time::timeout(DETACH_TIMEOUT, conn.detach(ticket)).await {
                Ok(Ok(())) => tracing::info!(session = ticket.id, "detached the session"),
                Ok(Err(e)) => tracing::warn!(
                    session = ticket.id,
                    errno = e.0,
                    "DETACH was refused; this session's descriptors stay on the \
                     server until its grace runs out"
                ),
                Err(_) => tracing::warn!(
                    session = ticket.id,
                    timeout = ?DETACH_TIMEOUT,
                    "DETACH went unanswered; this session's descriptors stay on \
                     the server until its grace runs out"
                ),
            }
        }
        self.mark_dead();
    }

    /// The current connection if there is a usable one, without waiting for a
    /// redial.
    ///
    /// [`Session::shutdown`] is not a caller that may park: it runs when the
    /// mount is already gone, and a `DETACH` that waited out a reconnect first
    /// would hold the process open for exactly as long as the deadline it is
    /// there to cancel.
    fn live(&self) -> Option<Arc<Connection>> {
        match &*self.state.borrow() {
            State::Live(conn) if !conn.is_dead() => Some(Arc::clone(conn)),
            _ => None,
        }
    }

    /// Take the session out of `Live` and into `Reconnecting`, retiring the
    /// dying connection's dropped-forget count on the way.
    ///
    /// `false` means a shutdown got here first and the supervisor must stop.
    /// The check and the write are one operation under the channel's lock,
    /// which is the only way they cannot race a `shutdown` on another task.
    fn begin(&self) -> bool {
        let mut proceed = false;
        self.state.send_if_modified(|state| match state {
            State::Dead => false,
            State::Reconnecting => {
                // Only this task writes it, so this arm is unreachable; carry
                // on rather than deadlocking a mount over an impossibility.
                proceed = true;
                false
            }
            State::Live(conn) => {
                self.retired_forgets
                    .fetch_add(conn.dropped_forgets(), Ordering::Relaxed);
                *state = State::Reconnecting;
                proceed = true;
                true
            }
        });
        proceed
    }

    /// Make a freshly claimed connection the current one.
    ///
    /// `false` means the mount ended while the dial was in flight, in which
    /// case the new connection drops here — its socket closes, and the server's
    /// grace hands the session back to its reaper.
    fn install(&self, conn: Arc<Connection>) -> bool {
        let mut installed = false;
        self.state.send_if_modified(|state| {
            if matches!(state, State::Dead) {
                return false;
            }
            *state = State::Live(conn);
            installed = true;
            true
        });
        installed
    }

    /// Every call from here on answers `EIO`, and every parked one wakes to it.
    fn mark_dead(&self) {
        self.state.send_if_modified(|state| {
            if matches!(state, State::Dead) {
                return false;
            }
            if let State::Live(conn) = state {
                self.retired_forgets
                    .fetch_add(conn.dropped_forgets(), Ordering::Relaxed);
            }
            *state = State::Dead;
            true
        });
    }

    /// One reconnection, from a dead connection to a live one or to the end of
    /// the mount. `false` means the supervisor must stop.
    async fn reconnect(&self, ticket: SessionTicket) -> bool {
        if !self.begin() {
            return false;
        }
        tracing::warn!(
            addr = %self.addr,
            session = ticket.id,
            deadline = ?self.deadline,
            "the connection died; re-attaching to the session"
        );

        let started = Instant::now();
        let mut backoff = BACKOFF_START;
        // Two ways out of the loop below, and only one of them needs a line of
        // its own: a server that refused has already said why.
        let mut ran_out_of_time = true;
        loop {
            let elapsed = started.elapsed();
            if elapsed >= self.deadline {
                break;
            }
            // The dial gets whatever is left, so a peer that accepts and then
            // says nothing cannot hold the mount past its own deadline.
            let proposal = Proposal {
                handshake_timeout: self.proposal.handshake_timeout.min(self.deadline - elapsed),
                ..self.proposal
            };
            match Connection::resume(self.addr, &self.export, proposal, ticket).await {
                Ok((conn, _settled, _root)) => return self.install(conn),
                // The server answered, and its answer cannot change: it has no
                // such session, or it has one this handshake does not match, or
                // it does not speak this protocol. A fresh `ATTACH` is not the
                // fallback — it would start the node counter over underneath a
                // kernel still holding this session's ids (design §5).
                Err(
                    e @ (ConnectError::NoSession
                    | ConnectError::SessionMismatch
                    | ConnectError::VersionMismatch),
                ) => {
                    tracing::error!(
                        addr = %self.addr,
                        session = ticket.id,
                        error = %e,
                        "the server will not resume this session; the mount is over"
                    );
                    ran_out_of_time = false;
                    break;
                }
                // Everything else is worth coming back for. `SESSION_BUSY` is
                // the server holding the session and not yet having noticed the
                // old socket (design §6.3); a refused dial, a reset and a
                // timeout are a transport that has not settled yet.
                Err(e) => {
                    tracing::debug!(
                        addr = %self.addr,
                        error = %e,
                        ?backoff,
                        "the claim did not land; trying again"
                    );
                }
            }
            let left = self.deadline.saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            tokio::time::sleep(backoff.min(left)).await;
            backoff = (backoff * 2).min(BACKOFF_CEILING);
        }

        if ran_out_of_time {
            tracing::error!(
                addr = %self.addr,
                waited = ?started.elapsed(),
                "gave up re-attaching; the mount answers EIO until it is unmounted"
            );
        }
        self.mark_dead();
        false
    }
}

/// Bound a caller's reconnect deadline by what the server promised to hold.
///
/// Three-quarters of the advertised grace, and both halves of that matter. A
/// client still dialling for a session the reaper already dropped is burning
/// time on a guaranteed refusal; and a clamp that could land *on* the grace
/// would leave it dialling at the exact moment the reaper fires (design §8.2).
///
/// A zero grace is a server that retains nothing — a `resume_grace = "0"`
/// configuration, or a client that never asked — and it leaves nothing to wait
/// for.
fn clamp_deadline(asked: Duration, grace: Duration) -> Duration {
    asked.min(grace / 4 * 3)
}

/// Whether a state can answer a call now, or a caller has to wait for the next
/// one.
///
/// The subtle arm is `Live` over a connection that has already died. The
/// supervisor learns of a death through [`Connection::closed`], which is a task
/// wake-up later than the death itself, and the FUSE bridge is issuing calls
/// the whole time. A caller handed that connection in the meantime would take
/// an `EIO` for a request that never went anywhere — the request design §3.2
/// promises to park — so on a session that reconnects, a dead `Live` is not an
/// answer. On a session that does not, it is the only answer there will ever
/// be.
fn answers(state: &State, reconnects: bool) -> bool {
    match state {
        State::Live(conn) => !reconnects || !conn.is_dead(),
        State::Reconnecting => false,
        State::Dead => true,
    }
}

/// The reconnect supervisor: one task per session, for the life of the mount.
///
/// It holds a [`Weak`] rather than an `Arc` on purpose. The mount's own
/// reference is what the session's life is measured by, and a task that kept it
/// alive would outlive the mount it exists to serve — dialling a server nobody
/// is waiting for, and holding the runtime open past the unmount.
async fn supervise(weak: Weak<Session>, ticket: SessionTicket) {
    loop {
        let (conn, mut changed) = {
            let Some(session) = weak.upgrade() else {
                return;
            };
            let rx = session.state.subscribe();
            let conn = match &*session.state.borrow() {
                State::Live(conn) => Arc::clone(conn),
                // `Dead` ends the mount, and only this task writes
                // `Reconnecting`: either one means there is nothing left to
                // watch.
                State::Reconnecting | State::Dead => return,
            };
            (conn, rx)
        };
        // Two ways to stop waiting: the connection dies, which is the reason
        // this task exists, or the state changes underneath it — a `shutdown`,
        // or the mount dropping its `Session` and closing the channel. Without
        // the second arm a shutdown mid-mount would leave this parked on a
        // healthy socket for as long as the process lived.
        tokio::select! {
            () = conn.closed() => {}
            _ = changed.changed() => {}
        }
        drop(conn);

        let Some(session) = weak.upgrade() else {
            return;
        };
        if !session.reconnect(ticket).await {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deadline_clamps_to_three_quarters_of_the_advertised_grace() {
        let grace = Duration::from_secs(60);
        // Under the clamp the ask stands: ten seconds is what the flag
        // defaults to and what the drills are timed against.
        assert_eq!(
            clamp_deadline(Duration::from_secs(10), grace),
            Duration::from_secs(10)
        );
        // Over it, the server's promise wins — and with room to spare, so the
        // last dial cannot land on the moment the reaper fires.
        assert_eq!(
            clamp_deadline(Duration::from_secs(120), grace),
            Duration::from_secs(45)
        );
        assert_eq!(
            clamp_deadline(grace, grace),
            Duration::from_secs(45),
            "a deadline equal to the grace still gets a margin"
        );
        // A server that retains nothing leaves nothing to wait for.
        assert_eq!(
            clamp_deadline(Duration::from_secs(10), Duration::ZERO),
            Duration::ZERO
        );
    }
}

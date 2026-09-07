//! Sessions that outlive their sockets (session-resumption design §8.1).
//!
//! `ATTACH` mints an entry here, a dying session task releases it, `RESUME`
//! claims it back, `DETACH` and the reaper remove it. The registry is generic
//! over the payload and over an opaque shape guard, so its tests need no
//! filesystem and no handshake: the rpc layer instantiates it with
//! `Arc<dyn FileSystem>` and its settled `Limits`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lbfs_proto::types::SessionTicket;
use rustix::rand::{getrandom, GetRandomFlags};

/// What a claim comes back with. The three refusals map one-to-one onto the
/// protocol statuses `STATUS_NO_SESSION`, `STATUS_SESSION_BUSY` and
/// `STATUS_SESSION_MISMATCH`.
#[derive(Debug)]
pub enum Claim<T> {
    Ok { payload: T, epoch: u64 },
    NoSession,
    Busy,
    Mismatch,
}

enum State {
    Attached,
    Idle { deadline: Instant },
}

struct Entry<T, G> {
    payload: T,
    /// The negotiated shape, stored at mint. Opaque to the registry: a claim
    /// must present an equal value, and that comparison is everything the
    /// registry knows about handshakes.
    guard: G,
    secret: [u8; 16],
    /// The single home of the epoch. `State::Attached` carries no copy.
    epoch: u64,
    state: State,
}

struct Inner<T, G> {
    entries: HashMap<u64, Entry<T, G>>,
    /// Never reused inside one process, so a stale ticket can never land on
    /// somebody else's session by id alone.
    next_id: u64,
}

pub struct Registry<T, G: PartialEq> {
    inner: Mutex<Inner<T, G>>,
    grace: Duration,
    max_sessions: usize,
}

impl<T: Clone, G: PartialEq> Registry<T, G> {
    pub fn new(grace: Duration, max_sessions: usize) -> Self {
        Registry {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                next_id: 1,
            }),
            grace,
            max_sessions,
        }
    }

    /// Register a fresh session, `Attached` with epoch `0`.
    ///
    /// `None` past `max_sessions`, which the caller treats as "no ticket"
    /// rather than as an error: retention is best-effort, and an attach
    /// without a ticket gets today's die-with-the-socket behaviour.
    pub fn mint(&self, payload: T, guard: G) -> Option<(SessionTicket, u64)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.entries.len() >= self.max_sessions {
            return None;
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let secret = fresh_secret();
        inner.entries.insert(
            id,
            Entry {
                payload,
                guard,
                secret,
                epoch: 0,
                state: State::Attached,
            },
        );
        let grace_ms = u32::try_from(self.grace.as_millis()).unwrap_or(u32::MAX);
        Some((
            SessionTicket {
                id,
                secret,
                grace_ms,
            },
            0,
        ))
    }

    /// Verify a ticket and hand the session to a new socket.
    ///
    /// Checked in this order: an unknown id or a secret that fails
    /// [`SessionTicket::secret_eq`] gives [`Claim::NoSession`];
    /// `State::Attached` gives [`Claim::Busy`]; an `Idle` entry past its
    /// deadline gives [`Claim::NoSession`], because expiry is the clock's fact
    /// rather than the reaper's schedule; a guard that differs from the stored
    /// one gives [`Claim::Mismatch`].
    ///
    /// The secret comparison runs in constant time: a short-circuiting `==`
    /// leaks the length of the matching prefix through timing, which turns
    /// 2^128 guesses into 16 × 256.
    ///
    /// An attached session is refused rather than stolen. A steal is the
    /// session-hijack primitive in a protocol with no authentication, and the
    /// case it would serve — a half-open socket whose server has not yet
    /// noticed the death — resolves itself inside the keepalive budget; the
    /// client retries and the second claim succeeds.
    ///
    /// **A refusal of any kind mutates nothing** — not the secret, the state,
    /// the epoch, or the deadline — so a corrected claim succeeds and a wrong
    /// one cannot extend retention. Only success changes the entry: bump the
    /// epoch, set `Attached`, return a clone of the payload with the new
    /// epoch.
    pub fn claim(&self, ticket: &SessionTicket, guard: &G) -> Claim<T> {
        let mut inner = self.inner.lock().unwrap();
        let entry = match inner.entries.get_mut(&ticket.id) {
            Some(entry) if ticket.secret_eq(&entry.secret) => entry,
            // An unknown id and a wrong secret answer identically, so a
            // guesser learns nothing from the difference.
            _ => return Claim::NoSession,
        };
        match entry.state {
            State::Attached => return Claim::Busy,
            State::Idle { deadline } if deadline <= Instant::now() => return Claim::NoSession,
            State::Idle { .. } => {}
        }
        if entry.guard != *guard {
            return Claim::Mismatch;
        }
        entry.epoch += 1;
        entry.state = State::Attached;
        Claim::Ok {
            payload: entry.payload.clone(),
            epoch: entry.epoch,
        }
    }

    /// A session task hands back its epoch at teardown.
    ///
    /// Flips to `Idle { deadline: now + grace }` only when the state is
    /// `Attached` and the entry's epoch equals the argument. Any other state
    /// does nothing: a mismatch means another socket already claimed the
    /// session, and a superseded task must not hand a live session back to
    /// the reaper.
    pub fn release(&self, id: u64, epoch: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.entries.get_mut(&id) {
            if matches!(entry.state, State::Attached) && entry.epoch == epoch {
                entry.state = State::Idle {
                    deadline: Instant::now() + self.grace,
                };
            }
        }
    }

    /// `DETACH`: verify the secret, then remove the entry.
    ///
    /// `false` means nothing was removed — an unknown id or a wrong secret —
    /// because a client that cannot prove ownership must not be able to
    /// destroy somebody else's session.
    pub fn drop_session(&self, ticket: &SessionTicket) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.entries.get(&ticket.id) {
            Some(entry) if ticket.secret_eq(&entry.secret) => {
                inner.entries.remove(&ticket.id);
                true
            }
            _ => false,
        }
    }

    /// Remove every `Idle` entry past its deadline.
    ///
    /// Returns the payloads rather than dropping them under the lock, so the
    /// caller can free them off the runtime — dropping a retained filesystem
    /// closes every descriptor it holds, and that is blocking work.
    pub fn reap_expired(&self) -> Vec<T> {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let expired: Vec<u64> = inner
            .entries
            .iter()
            .filter_map(|(id, entry)| match entry.state {
                State::Idle { deadline } if deadline <= now => Some(*id),
                _ => None,
            })
            .collect();
        expired
            .into_iter()
            .map(|id| inner.entries.remove(&id).unwrap().payload)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 16 bytes from `getrandom(2)`, looped until every byte fills.
///
/// The call reports how many bytes it wrote, and a short read taken as
/// complete would be a silently weak secret.
fn fresh_secret() -> [u8; 16] {
    let mut secret = [0u8; 16];
    let mut filled = 0;
    while filled < secret.len() {
        match getrandom(&mut secret[filled..], GetRandomFlags::empty()) {
            Ok(n) => filled += n,
            // Interrupted while blocking on early-boot entropy: retry, like
            // every other EINTR.
            Err(rustix::io::Errno::INTR) => continue,
            // Unreachable on a kernel this server runs on (the syscall
            // predates every io_uring opcode §5.3 needs); a secret this
            // process cannot mint is a session it must not retain.
            Err(e) => panic!("getrandom(2) failed: {e}"),
        }
    }
    secret
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const GRACE: Duration = Duration::from_secs(60);

    fn registry() -> Registry<u32, u32> {
        Registry::new(GRACE, 4)
    }

    /// The deadline an idle entry carries, read straight off the state. Tests
    /// live in the registry's own module exactly so refusals can be checked
    /// against the entry rather than inferred from timing.
    fn idle_deadline(r: &Registry<u32, u32>, id: u64) -> std::time::Instant {
        match r.inner.lock().unwrap().entries.get(&id).unwrap().state {
            State::Idle { deadline } => deadline,
            State::Attached => panic!("entry {id} is attached, not idle"),
        }
    }

    #[test]
    fn mint_issues_a_fresh_id_and_a_fresh_secret() {
        let r = registry();
        let (t1, e1) = r.mint(1, 0).unwrap();
        let (t2, e2) = r.mint(2, 0).unwrap();
        assert_ne!(t1.id, t2.id, "ids are never reused inside one process");
        assert_ne!(t1.secret, t2.secret, "secrets differ between mints");
        assert_eq!(e1, 0, "an entry starts at epoch 0");
        assert_eq!(e2, 0);
    }

    #[test]
    fn a_claim_on_an_attached_session_answers_busy() {
        let r = registry();
        let (t, _) = r.mint(7, 0).unwrap();
        assert!(matches!(r.claim(&t, &0), Claim::Busy));
    }

    #[test]
    fn release_with_the_right_epoch_makes_the_session_claimable() {
        let r = registry();
        let (t, epoch) = r.mint(7, 0).unwrap();
        r.release(t.id, epoch);
        match r.claim(&t, &0) {
            Claim::Ok { payload, epoch } => {
                assert_eq!(payload, 7, "the claim returns the payload");
                assert_eq!(epoch, 1, "a successful claim bumps the epoch");
            }
            _ => panic!("an idle session must be claimable"),
        }
    }

    #[test]
    fn a_wrong_secret_answers_no_session_and_the_entry_survives() {
        let r = registry();
        let (t, epoch) = r.mint(7, 0).unwrap();
        r.release(t.id, epoch);
        let mut wrong = t;
        wrong.secret[3] ^= 1;
        assert!(matches!(r.claim(&wrong, &0), Claim::NoSession));
        assert!(
            matches!(r.claim(&t, &0), Claim::Ok { .. }),
            "a corrected claim afterwards succeeds"
        );
    }

    #[test]
    fn an_unknown_id_answers_no_session() {
        let r = registry();
        let (mut t, epoch) = r.mint(7, 0).unwrap();
        r.release(t.id, epoch);
        t.id += 1;
        assert!(matches!(r.claim(&t, &0), Claim::NoSession));
    }

    /// The epoch rule: a superseded session task must not hand a live session
    /// back to the reaper.
    #[test]
    fn a_release_with_a_stale_epoch_does_not_re_idle_a_claimed_entry() {
        let r = registry();
        let (t, first) = r.mint(7, 0).unwrap();
        r.release(t.id, first);
        let second = match r.claim(&t, &0) {
            Claim::Ok { epoch, .. } => epoch,
            _ => panic!("the idle entry must be claimable"),
        };
        assert_ne!(first, second, "the claim bumps the epoch");

        // The old socket's teardown arrives late, carrying the old epoch.
        r.release(t.id, first);
        assert!(
            matches!(r.claim(&t, &0), Claim::Busy),
            "the entry stays attached to the socket that claimed it"
        );

        // The current epoch still releases it.
        r.release(t.id, second);
        assert!(matches!(r.claim(&t, &0), Claim::Ok { .. }));
    }

    #[test]
    fn reap_expired_drops_the_expired_and_keeps_the_rest() {
        let r: Registry<u32, u32> = Registry::new(Duration::from_millis(40), 4);
        let (t1, e1) = r.mint(1, 0).unwrap();
        let (t2, e2) = r.mint(2, 0).unwrap();
        r.release(t1.id, e1);
        std::thread::sleep(Duration::from_millis(50));
        r.release(t2.id, e2);

        let dropped = r.reap_expired();
        assert_eq!(
            dropped,
            vec![1],
            "the reaper returns the payloads it dropped, so the caller can free them off the runtime"
        );
        assert_eq!(r.len(), 1);
        assert!(
            matches!(r.claim(&t2, &0), Claim::Ok { .. }),
            "the entry inside its deadline survives the sweep"
        );
        assert!(matches!(r.claim(&t1, &0), Claim::NoSession));
    }

    #[test]
    fn drop_session_removes_the_entry() {
        let r = registry();
        let (t, epoch) = r.mint(7, 0).unwrap();
        assert!(r.drop_session(&t));
        r.release(t.id, epoch);
        assert!(matches!(r.claim(&t, &0), Claim::NoSession));
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn mint_past_the_cap_returns_none() {
        let r: Registry<u32, u32> = Registry::new(GRACE, 2);
        r.mint(1, 0).unwrap();
        r.mint(2, 0).unwrap();
        // "No ticket", not an error: the caller attaches without retention.
        assert!(r.mint(3, 0).is_none());
    }

    #[test]
    fn a_mismatched_guard_refuses_and_mutates_nothing() {
        let r = registry();
        let (t, epoch) = r.mint(7, 5).unwrap();
        r.release(t.id, epoch);
        let before = idle_deadline(&r, t.id);

        assert!(matches!(r.claim(&t, &6), Claim::Mismatch));
        assert_eq!(
            idle_deadline(&r, t.id),
            before,
            "a refused claim must not extend retention"
        );
        assert!(
            matches!(r.claim(&t, &5), Claim::Ok { .. }),
            "the matching guard straight afterwards succeeds"
        );
    }

    /// Expiry is the clock's fact, not the reaper's schedule.
    #[test]
    fn a_claim_past_the_deadline_answers_no_session_before_the_reaper_runs() {
        let r: Registry<u32, u32> = Registry::new(Duration::from_millis(20), 4);
        let (t, epoch) = r.mint(7, 0).unwrap();
        r.release(t.id, epoch);
        std::thread::sleep(Duration::from_millis(30));
        assert!(matches!(r.claim(&t, &0), Claim::NoSession));
        assert_eq!(r.len(), 1, "removal stays the reaper's job");
    }
}

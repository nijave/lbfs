# Session Resumption Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Status:** Not started. Written 2026-08-28 alongside
`docs/superpowers/specs/2026-08-28-session-resumption-design.md`, which holds
the reasoning this plan executes. Read that document first; this one assumes
its rulings and does not re-argue them.

**Two coordination facts before Task 1.**

- **A second change is in flight on the same files.** The forced-sync control
  message (spec §11 fast-follow 2) lives on `feat/forced-sync-control`. Read at
  `95a866f` on 2026-08-28, it turns `FLAG_FORCE_SYNC_RESERVED` into a live
  `FLAG_FORCE_SYNC`, carries the bit through dispatch, and honours it in
  `LocalFs` — and it touches `crates/lbfs-proto/src/frame.rs`,
  `crates/lbfs-server/src/rpc/mod.rs`, `crates/lbfs-server/src/rpc/dispatch.rs`,
  `tests/src/lib.rs` and `tests/tests/protocol.rs`.

  At that commit it **does not** move `PROTOCOL_VERSION` and adds **no**
  opcode, so the two changes conflict only textually, in `frame.rs` and in the
  server's read loop. Whichever lands second rebases. Two things to confirm
  rather than assume when that happens: that the branch still leaves the
  version at `2` — a live flag on `FSYNC` is a wire-behaviour change that could
  yet earn a bump of its own, and two independent bumps to `3` produce a tree
  where one version number means two wire formats — and that
  `FLAG_FORCE_SYNC_RESERVED` has become `FLAG_FORCE_SYNC`, which is the name
  this plan's Global Constraints must then leave alone.
- **This plan bumps the protocol version to `3`.** Both ends deploy together
  (spec §11, "Noted and deferred"), so the cost is a lock-step deploy and
  nothing else.

**Goal:** A mount survives a transport failure that lasts seconds. The server
holds the session — node table, open file handles, open directory snapshots —
for a bounded grace after the socket dies; the client re-attaches to it with a
ticket; every request that was in flight at the break still fails `EIO`.

**Architecture:** The server grows a `SessionRegistry` beside its uring executor
and buffer pool, holding `Arc<dyn FileSystem>` values that outlive the
connections that made them. `ATTACH` mints a ticket into it; a new opcode
`RESUME` claims one back; a new opcode `DETACH` drops one; a reaper task
expires the rest. The client grows one object above `Connection` — a `Session`
that owns the current connection, swaps it after a reconnect, and parks calls
issued while the swap is in progress. `Connection` itself keeps every invariant
it has today, including "a dead connection stays dead".

**Tech Stack:** Rust (edition 2021), tokio 1, fuser 0.18.0 (ABI 7.40,
exact-pinned), io-uring 0.7, rustix 1 (the `rand` feature joins the existing
list, for `getrandom`), postcard 1.1 + serde/serde_bytes, libc, tracing,
tempfile; Linux 7.0 guests under libvirt.

**Spec:** `docs/superpowers/specs/2026-08-20-lbfs-design.md` and
`docs/superpowers/specs/2026-08-28-session-resumption-design.md`

## Global Constraints

- Frame header: exactly 24 bytes, little-endian, layout per spec §3.1.
  **Unchanged by this plan.**
- **Frame flag bit 1 belongs to the forced-sync fast-follow. No task here reads
  it, writes it, or renames its constant** — `FLAG_FORCE_SYNC_RESERVED` before
  that branch lands, `FLAG_FORCE_SYNC` after.
- Protocol magic `LBFS`; version moves `2` → `3`, exact match on both ends.
  Task 2 owns that move and no other task touches the number.
- Status field: `0` OK, `1..=4095` Linux errno, `>= 0xFF00` protocol statuses.
  Three new protocol statuses, contiguous after the existing three.
- Defaults: port `9423`, window `128` (clamp 8..=1024), max body `64 KiB`.
- Names, symlink targets, xattr names and values travel as byte strings — never
  `String`.
- Bulk data never passes through postcard. `RESUME` and `DETACH` carry no data
  segment, so `data_limit` and `outbound_data_limit` both keep answering `0`
  for them.
- The RPC layer reaches storage only through the `FileSystem` trait (spec
  §5.1). The registry holds `Arc<dyn FileSystem>` and calls nothing on it
  except `getattr(ROOT_NODE)` on a claim; `LocalFs` gains no knowledge that
  sessions can outlive sockets.
- Every task ends green: `make check` (fmt --check, clippy `-D warnings`,
  tests) passes before every commit. Run `cargo fmt --all` first.
- Any task touching the client, the server or the protocol also passes
  `make test-loopback` before its commit — the repository's own gate table in
  `AGENTS.md` says so, and every task here qualifies from Task 2 on.
- TDD: write the failing test first for every behavior.
- No `unsafe` outside `crates/lbfs-server/src/fs/local/uring.rs`.
- Commit after every task with the exact paths staged (no blanket `git add .`).
- **Do not touch the VM pair** until Task 13. Tasks 1-12 verify at the loopback
  level.

---

## Design and Context

Read the design document first. This section holds only what the plan needs on
top of it: where in the tree each ruling lands, and the three sequencing traps.

### 1. The four objects a session owns, and where they live

`ATTACH` builds one `LocalFs` per connection
(`crates/lbfs-server/src/rpc/mod.rs`, `fn attach`, the `LocalFs::from_root_fd`
call). That object owns:

| State | Field | What the client holds |
|---|---|---|
| Node table | `LocalFs::nodes` | `NodeId` + `generation`, one `O_PATH` fd per node, `nlookup` |
| Open files | `LocalFs::files` | `Fh` → `FileHandle { node, fd }` |
| Open directories | `LocalFs::dirs` | `Dh` → `Arc<DirHandle>` with its snapshot and cookie map |
| Negotiated shape | `Limits` in `rpc`, `LocalFs::writeback` | window, I/O size, whose kernel owns file size |

`serve_requests` drops the `Arc<dyn FileSystem>` when it returns, and every
descriptor closes with it. Retention means one thing mechanically: a clone of
that `Arc` lives in a server-wide map instead.

### 2. Why the client cannot rebuild any of it

Design §2 argues this at length. The three code facts behind it:

- `crates/lbfs-client/src/fuse.rs` keeps no `NodeId` → path map, and FUSE hands
  the daemon a `(parent, name)` pair only at lookup time. A rebuild needs a
  namespace the client does not have.
- `crates/lbfs-server/src/fs/local/nodes.rs`'s module documentation states the
  property a rebuild breaks: the `O_PATH` descriptor "pins the inode for as long
  as the kernel holds a lookup count on the id, so `(st_dev, st_ino)` cannot
  come to name a different file underneath a live node".
- `DirHandle::resume_at` answers `EINVAL` for any cursor the handle did not
  issue. A re-`OPENDIR` mints fresh `d_off` cookies, so every saved cursor
  becomes invalid — which makes a half-paged `READDIR` across a reconnect a
  correctness problem under rebuilding and a non-event under retention.

### 3. The three traps

**Trap 1: a fresh `ATTACH` after a refused claim aliases node ids.**
`NodeTable::new` starts `next_id` at `ROOT_NODE + 1` and `next_generation` at
`1`. A FUSE request carries a node id and no generation beside it, so node 7 in
the client kernel's dentry cache would land on whatever node 7 the new session
issues. Task 10 must make a refused claim mark the session dead, never retry as
a fresh attach. Design §5 is the argument; acceptance criterion 8 is the check.

**Trap 2: the drain holds the session for 30 seconds.** `serve_requests` waits
up to `DRAIN_TIMEOUT` for replies it already produced. A claim arriving one
second into that wait must not queue behind it, so Task 5 flips the registry
entry to `Idle` *before* the drain, not after. Handler tasks from the dead
socket keep their own `Arc<dyn FileSystem>` clones and may still mutate the node
table while a new socket serves against it; that is safe for the same reason two
concurrent handlers on one socket are safe — `NodeTable` and `HandleTable` each
sit behind their own mutex.

**Trap 3: a superseded session task must not re-idle a claimed entry.** The old
socket's teardown and a new socket's claim race. The epoch in Task 3 resolves
it: a claim bumps the epoch, and `release` does nothing unless the entry still
carries the epoch its caller last attached under.

### 4. What the existing disconnect tests promise, and why they still pass

`vm/tests/disconnect.sh` stops the server outright, which empties the registry,
so every claim answers `STATUS_NO_SESSION` and the mount dies exactly as the
drill asserts. One number matters: the drill wraps its post-mortem `ls` in
`timeout 20`, and `tests/tests/loopback.rs`'s
`a_dead_server_leaves_an_eio_mount_that_still_unmounts` waits `SETTLE_TIMEOUT`
(30 s) for the first `EIO`. **The client's reconnect deadline must stay well
under both**, which is why Task 10 sets it to 10 seconds by default. A larger
default turns a passing drill into a hang.

### 5. Where the randomness comes from

`rustix` already ships in the workspace with `["event", "fs", "net", "process",
"thread"]`. Adding `"rand"` reaches `rustix::rand::getrandom`, which is the
`getrandom(2)` syscall — no new crate, no new supply chain, and `deny.toml`
unchanged. Task 2 makes that edit.

---

## File Map

| Path | Change |
|---|---|
| `docs/superpowers/specs/2026-08-20-lbfs-design.md` | §3.2, §3.3, §3.4, §4, §7, §8 and §11 record resumption |
| `crates/lbfs-proto/src/frame.rs` | `PROTOCOL_VERSION = 3`; three new statuses |
| `crates/lbfs-proto/src/types.rs` | `SessionTicket` |
| `crates/lbfs-proto/src/ops.rs` | `Opcode::Resume`, `Opcode::Detach`; `HelloRequest.resume`; `HelloReply.resume_grace_ms`; `AttachReply.ticket`; `ResumeRequest`, `ResumeReply`, `DetachRequest` |
| `crates/lbfs-server/src/rpc/registry.rs` | New: `Registry<T>`, the state machine, the reaper |
| `crates/lbfs-server/src/rpc/mod.rs` | `Server` holds a registry; `attach` mints; `resume` and `detach` join the handshake; teardown releases; `TCP_USER_TIMEOUT` |
| `crates/lbfs-server/src/config.rs` | `resume_grace`, `max_resumable_sessions`, `parse_duration` |
| `crates/lbfs-client/src/conn.rs` | `Connection::closed()`; `resume` and `detach` calls; the handshake carries a ticket |
| `crates/lbfs-client/src/session.rs` | New: `Session`, the reconnect supervisor, parked calls |
| `crates/lbfs-client/src/fuse.rs` | `LbfsFuse` holds an `Arc<Session>` |
| `crates/lbfs-client/src/main.rs` | `--reconnect-timeout`, `--no-reconnect`, `DETACH` at shutdown |
| `crates/lbfs-client/tests/mux.rs` | Death signalling, resume negotiation, parked calls |
| `tests/src/lib.rs` | `TestClient::resume`, `TestClient::detach`, ticket plumbing |
| `tests/tests/protocol.rs` | The claim matrix and the two identity cases |
| `tests/tests/loopback.rs` | A severable proxy and the mount-survives cases |
| `vm/tests/reconnect.sh`, `vm/test.sh` | The severed-connection drill |
| `crates/lbfs-server/pkg/lbfs.toml`, `vm/server-config.toml` | The two new config keys |

---

### Task 1: Spec — record what resumption changes

**Files:**
- Edit: `docs/superpowers/specs/2026-08-20-lbfs-design.md` (§3.2, §3.3, §3.4, §4, §7, §8, §11)

**Interfaces:**
- Consumes: `docs/superpowers/specs/2026-08-28-session-resumption-design.md`.
- Produces: the written contract every later task argues from. Names fixed
  here: protocol version `3`, the `RESUME` and `DETACH` opcodes, the three new
  statuses, the two config keys, and the sentence §7 replaces.

- [ ] **Step 1: §3.2 — the version and the new handshake field**

Replace the version-`2` paragraph of step 1 with a version-`3` one that keeps
the existing argument verbatim and adds: version `3` carries a request to
resume in `HELLO` and a session ticket in the `ATTACH` reply, and postcard's
trailing-byte tolerance is the same reason the match stays exact.

Add to step 2's list: the settled grace, in milliseconds, zero when the server
retains nothing.

- [ ] **Step 2: §3.3 — session lifetime**

After the `NodeId` bullet, state that a *session* rather than a connection
scopes node ids, generations and handles, that a session outlives its
socket by the configured grace, and that a client re-attaches with the ticket
`ATTACH` handed it. Point at the design document for what that does and does
not restore.

- [ ] **Step 3: §3.4 — two opcodes**

Add `RESUME` and `DETACH` to the Session row of the opcode table.

- [ ] **Step 4: §4 — the config keys**

Add `resume_grace = "60s"` and `max_resumable_sessions = 64` to the TOML block,
with one sentence each.

- [ ] **Step 5: §7 — replace the connection-loss bullet**

Find:

```text
- **Connection loss:** all in-flight and later ops fail `EIO`; the mount
  stays present and cleanly unmountable. No transparent reconnect in v1
  (node/handle state is session-scoped server-side; honest reconnection
  needs session resumption — the first fast-follow, §11).
```

Replace with a bullet that says: requests in flight at the break fail `EIO`;
requests issued afterwards park while the client re-attaches, bounded by
`--reconnect-timeout` (10 s default); a server that still holds the session
answers `RESUME` and the mount continues with its node ids, handles and
directory cursors intact; a server that does not — a restart, an expired grace,
a wrong ticket — leaves the mount dead in the old sense, `EIO` until unmount.
Name the design document for the reasoning.

- [ ] **Step 6: §8 — staleness**

Replace "Server restart ⇒ connection drop ⇒ `EIO` until remount (until
reconnection lands)" with the settled behaviour: a server restart empties the
session registry, so it refuses the claim and the mount answers `EIO` until
remount. A transport failure to a server that stayed up resumes instead.

- [ ] **Step 7: §11 — retire the fast-follow, add the leftovers**

Move fast-follow 1 out of the priority list and into a line recording that it
landed, naming the design document. Renumber the forced-sync item to 1 —
**check for a conflict with the forced-sync branch before editing this list**.

Add to "Future work": a persistent-session design over
`name_to_handle_at`/`open_by_handle_at`; the cold re-attach with a poisoned id
space; a session-level `FORGET` queue that survives a connection swap; and
reclaiming the lookup counts and handles stranded by requests that died in the
gap.

- [ ] **Step 8: Check the diff**

Run: `git diff --stat docs/superpowers/specs/2026-08-20-lbfs-design.md`
Expected: one file changed.

- [ ] **Step 9: Commit**

```bash
git add docs/superpowers/specs/2026-08-20-lbfs-design.md docs/superpowers/plans/2026-08-28-session-resumption.md
git commit -m "docs(spec): session resumption over a retained server session"
```

---

### Task 2: Proto — version 3, the ticket, two opcodes, three statuses

**Files:**
- Edit: `crates/lbfs-proto/src/frame.rs`, `crates/lbfs-proto/src/types.rs`, `crates/lbfs-proto/src/ops.rs`
- Edit: `Cargo.toml` (the `rustix` feature list)
- Edit: `crates/lbfs-server/src/rpc/mod.rs`, `crates/lbfs-client/src/conn.rs`, `tests/src/lib.rs` (mechanical field additions only)

**Interfaces:**
- Consumes: nothing.
- Produces: every wire name the later tasks use. **No behaviour.** After this
  task the fields exist, both ends send inert values (`resume: false`,
  `resume_grace_ms: 0`, `ticket: None`), and every existing test passes
  unchanged.

- [ ] **Step 1: Write the failing tests**

In `crates/lbfs-proto/src/ops.rs`'s test module, add cases pinning: the two new
opcode numbers round-trip through `TryFrom<u16>`; `36` still fails; a
`ResumeRequest` and a `ResumeReply` round-trip through postcard; and a
`HelloRequest` with `resume: true` round-trips.

In `crates/lbfs-proto/src/types.rs`, add a case pinning that `SessionTicket`
round-trips and that two tickets differing in one secret byte compare unequal
through the constant-time helper.

In `crates/lbfs-proto/src/frame.rs`, add a case pinning `PROTOCOL_VERSION == 3`
and that the three new statuses sit above `0xFF00`, differ from each other, and
differ from the three that already exist.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p lbfs-proto`
Expected: FAIL to compile — the names do not exist.

- [ ] **Step 3: `frame.rs`**

```rust
/// Version 3 adds session resumption: `HelloRequest.resume`,
/// `HelloReply.resume_grace_ms`, a ticket on the `ATTACH` reply, and the
/// `RESUME`/`DETACH` opcodes. The match stays exact for the reason version 2
/// made it exact — postcard ignores trailing bytes rather than refusing them,
/// so a version-2 server would decode a version-3 `HELLO` cleanly, drop the
/// resume request, and hand back a mount the client wrongly believes it can
/// re-attach to.
pub const PROTOCOL_VERSION: u32 = 3;

pub const STATUS_NO_SESSION: u16 = 0xFF04;
pub const STATUS_SESSION_BUSY: u16 = 0xFF05;
pub const STATUS_SESSION_MISMATCH: u16 = 0xFF06;
```

Leave `FLAG_NO_REPLY` and `FLAG_FORCE_SYNC_RESERVED` untouched.

- [ ] **Step 4: `types.rs` — the ticket**

```rust
/// What a client presents to claim a session it was already attached to.
///
/// `id` is the registry key: a server-local counter, never reused inside one
/// process. `secret` is the whole of the authentication — 128 bits from
/// `getrandom(2)`, compared in constant time. v1 has no authentication at all
/// (spec §1), and this does not add one: a ticket is a bearer capability worth
/// roughly what a fresh `ATTACH` to the same export is worth, plus the open
/// descriptors a fresh attach could not reach. The session-resumption design
/// document prices it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTicket {
    pub id: u64,
    pub secret: [u8; 16],
    /// How long the server holds this session after its socket dies. Zero
    /// means the server retains nothing.
    pub grace_ms: u32,
}

impl SessionTicket {
    /// Constant-time secret comparison.
    ///
    /// A short-circuiting `==` leaks the length of the matching prefix through
    /// timing, which turns 2^128 guesses into 16 × 256. The loop below reads
    /// every byte whatever it finds.
    pub fn secret_eq(&self, other: &[u8; 16]) -> bool {
        let mut diff = 0u8;
        for (a, b) in self.secret.iter().zip(other.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}
```

- [ ] **Step 5: `ops.rs` — opcodes, fields, request structs**

Add `Resume = 34` and `Detach = 35` to the enum and to `TryFrom<u16>`. Add
`pub resume: bool` to `HelloRequest`, `pub resume_grace_ms: u32` to
`HelloReply`, `pub ticket: Option<SessionTicket>` to `AttachReply`, and:

```rust
/// Reply: [`ResumeReply`]. Replaces `ATTACH` as the second frame of a
/// reconnecting session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeRequest {
    pub ticket: SessionTicket,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeReply {
    pub root_attr: FileAttr,
}

/// No reply body. Drops the session and everything it holds, so a clean
/// unmount does not leave descriptors resident for the grace period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetachRequest {
    pub ticket: SessionTicket,
}
```

- [ ] **Step 6: The `rustix` feature**

In the workspace `Cargo.toml`, add `"rand"` to the `rustix` feature list, with
a comment naming Task 5's use: session secrets come from
`rustix::rand::getrandom`, which is `getrandom(2)` and no new crate.

- [ ] **Step 7: Make the workspace compile again, inertly**

Fill the new fields at every construction site with values that change nothing:
`resume: false` in the client's `hello`, `resume_grace_ms: 0` in the server's
`HelloReply`, `ticket: None` in the server's `AttachReply`, and the same in
`tests/src/lib.rs`'s handshake helpers. Add `Resume` and `Detach` to the
server's post-handshake rejection alongside `Hello` and `Attach` — after the
handshake they are as illegal as the other two, and leaving them out would let
a client re-attach mid-session.

- [ ] **Step 8: Run the tests**

Run: `make check` then `make test-loopback`
Expected: PASS. The wire grew fields nobody reads and one version number
everybody checks; the loopback client and server both moved to `3` together.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml crates/lbfs-proto/src/frame.rs crates/lbfs-proto/src/types.rs crates/lbfs-proto/src/ops.rs crates/lbfs-server/src/rpc/mod.rs crates/lbfs-client/src/conn.rs tests/src/lib.rs
git commit -m "feat(proto): version 3 with session tickets, RESUME and DETACH"
```

---

### Task 3: Server — the session registry

**Files:**
- Create: `crates/lbfs-server/src/rpc/registry.rs`
- Edit: `crates/lbfs-server/src/rpc/mod.rs` (`pub mod registry;` only)

**Interfaces:**
- Consumes: `lbfs_proto::types::SessionTicket`.
- Produces: `Registry<T, G>` — generic over the payload and over an opaque
  shape guard, so its own tests need no filesystem and no handshake — plus
  `pub type SessionRegistry = Registry<Arc<dyn FileSystem>, (Limits, bool)>`
  in `rpc::mod`, the guard being the settled limits and the `writeback` flag.
  Methods: `mint`, `claim`, `release`, `drop_session`, `reap_expired`, `len`.

- [ ] **Step 1: Write the failing tests**

Cover, against `Registry<u32, u32>`:

1. `mint` returns a ticket whose id is fresh and whose secret differs between
   two mints.
2. `claim` on an attached session answers `Busy`.
3. `release` with the right epoch flips it to idle; `claim` then succeeds and
   returns the payload.
4. `claim` with a wrong secret answers `NoSession`, and the entry survives, so
   a corrected claim afterwards succeeds.
5. `claim` with an unknown id answers `NoSession`.
6. **The epoch rule.** Release, claim (epoch bumps), then release again with
   the *first* epoch: the entry stays attached. A superseded session task must
   not hand a live session back to the reaper.
7. `reap_expired` drops an entry whose deadline has passed and keeps one whose
   deadline has not, and it returns the payloads it dropped so the caller can
   free them off the runtime.
8. `drop_session` removes the entry; a later claim answers `NoSession`.
9. `mint` past `max_sessions` returns `None`, and the caller treats that as
   "no ticket", not as an error.
10. A claim whose guard differs answers `Mismatch` and mutates nothing: the
    same ticket with the matching guard straight afterwards succeeds, and the
    entry's deadline has not moved.
11. A claim on an `Idle` entry past its deadline answers `NoSession` even
    though the reaper has not run. Expiry is the clock's fact, not the reaper's
    schedule.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p lbfs-server registry`
Expected: FAIL to compile.

- [ ] **Step 3: Write the registry**

The shape:

```rust
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

pub struct Registry<T, G: PartialEq> {
    inner: Mutex<Inner<T, G>>,
    grace: Duration,
    max_sessions: usize,
}
```

Rules, all under the one lock:

- `mint(payload, guard) -> Option<(SessionTicket, u64)>` — `None` past
  `max_sessions`; otherwise a fresh id and 16 secret bytes via
  `rustix::rand::getrandom(&mut secret, GetRandomFlags::empty())`, looped until
  all 16 bytes fill — the call reports how many bytes it wrote, and a short
  read here is a silently weak secret. Entry starts `Attached` with epoch `0`.
- `claim(&SessionTicket, guard: &G) -> Claim<T>` — checked in this order: an
  unknown id or a secret that fails `secret_eq` gives `NoSession`;
  `State::Attached` gives `Busy`; an `Idle` entry past its deadline gives
  `NoSession`, because expiry is the clock's fact rather than the reaper's
  schedule; a guard that differs from the stored one gives `Mismatch`.
  **A refusal of any kind mutates nothing** — not the secret, the state, the
  epoch, or the deadline — so a corrected claim succeeds and a wrong one cannot
  extend retention. Only success changes the entry: bump the epoch, set
  `Attached`, return a clone of the payload with the new epoch.
- `release(id, epoch)` — flips to `Idle { deadline: now + grace }` only when
  the state is `Attached` and the entry's epoch equals the argument. Any other
  state does nothing.
- `drop_session(&SessionTicket) -> bool` — verifies the secret, then removes.
- `reap_expired() -> Vec<T>` — removes every `Idle` entry past its deadline and
  returns the payloads.

Two comments the code must carry: why the comparison is constant time (leaking
the matching prefix length turns 2^128 guesses into 16 × 256), and why `claim`
refuses an attached session rather than stealing it (a steal is the
session-hijack primitive in a protocol with no authentication, and a half-open
socket resolves itself inside the keepalive budget).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p lbfs-server registry`
Expected: PASS, all eleven.

- [ ] **Step 5: `make check`**

- [ ] **Step 6: Commit**

```bash
git add crates/lbfs-server/src/rpc/registry.rs crates/lbfs-server/src/rpc/mod.rs
git commit -m "feat(server): a registry of sessions that outlive their sockets"
```

---

### Task 4: Server — configuration

**Files:**
- Edit: `crates/lbfs-server/src/config.rs`
- Edit: `crates/lbfs-server/pkg/lbfs.toml`, `vm/server-config.toml`

**Interfaces:**
- Consumes: nothing.
- Produces: `Config::resume_grace: Duration` and
  `Config::max_resumable_sessions: usize`, plus `pub fn parse_duration`.

- [ ] **Step 1: Write the failing tests**

Beside the existing `parse_size` cases: `parse_duration` accepts `"60s"`,
`"500ms"`, a bare `"30"` as seconds and `"0"` as zero; it refuses `"60x"`,
`"-1"` and `""`. A TOML file with neither key defaults to 60 seconds and 64
sessions; one with `resume_grace = "0"` yields `Duration::ZERO`; an unknown key
still fails, because `deny_unknown_fields` stays on.

- [ ] **Step 2: Run them and watch them fail**

- [ ] **Step 3: Add the keys**

`RawConfig` grows `resume_grace: Option<String>` and
`max_resumable_sessions: Option<usize>`. `parse_duration` mirrors `parse_size`:
widen to `u64` before scaling, and refuse anything that does not parse whole.
Document `resume_grace = "0"` as the switch that turns retention off, so an
operator who wants today's behaviour has one.

- [ ] **Step 4: Update the two shipped configs**

Add both keys, commented, to `crates/lbfs-server/pkg/lbfs.toml` and `vm/server-config.toml`.

- [ ] **Step 5: `make check`**

- [ ] **Step 6: Commit**

```bash
git add crates/lbfs-server/src/config.rs crates/lbfs-server/pkg/lbfs.toml vm/server-config.toml
git commit -m "feat(server): resume_grace and max_resumable_sessions"
```

---

### Task 5: Server — mint at attach, release at teardown, reap on a timer

**Files:**
- Edit: `crates/lbfs-server/src/rpc/mod.rs`
- Edit: `tests/src/lib.rs` (expose the ticket on `TestClient`)
- Edit: `tests/tests/protocol.rs`

**Interfaces:**
- Consumes: Tasks 2, 3 and 4.
- Produces: `Server` holds a `SessionRegistry`; `hello` reports the grace;
  `attach` mints a ticket; `serve_requests` releases the entry before it
  drains; a reaper task runs per server. **No `RESUME` yet** — a session goes
  idle and then expires, and nothing claims it.

- [ ] **Step 1: Write the failing tests**

In `tests/tests/protocol.rs`:

1. A `HELLO` with `resume: true` comes back with a non-zero `resume_grace_ms`,
   and one with `resume: false` comes back with zero.
2. `ATTACH` on a handshake that asked to resume returns `Some(ticket)`; on a
   `resume: false` handshake it returns `None`.
3. Against a server configured with `resume_grace = "0"`, `ATTACH` returns
   `None` whatever the client asked for.
4. Two attaches return tickets with different ids and different secrets.

- [ ] **Step 2: Run them and watch them fail**

- [ ] **Step 3: Wire the registry into `Server`**

`Server::new` builds the registry from the config and spawns the reaper. The
reaper wakes on an interval (grace / 4, floored at a second), calls
`reap_expired`, and drops each payload through `spawn_blocking` — the reason
`LocalFs::releasedir` already gives for its own snapshots applies here: freeing a
large directory's entries is a million small deallocations, and the final
`close(2)` on an unlinked file's last descriptor does journal work. A registry
with a zero grace skips the task entirely.

- [ ] **Step 4: `hello` reports the grace**

`resume_grace_ms` = the configured grace in milliseconds when the client asked
for resumption and the server retains anything, else `0`. Carry the client's
`resume` bit into `Limits`, beside `writeback`, because `attach` needs it.

- [ ] **Step 5: `attach` mints**

After `LocalFs::from_root_fd` succeeds and before the reply, mint, storing the
settled `Limits` and `writeback` as the entry's guard. A `None`
from the registry — the `max_resumable_sessions` cap — means the reply carries
`ticket: None` and the session behaves as it does today. Log the refusal once
per occurrence with the cap in the line.

Hold the minted `(id, epoch)` beside the `Arc<dyn FileSystem>` for the session
task.

- [ ] **Step 6: `serve_requests` releases before it drains**

At the top of teardown, before `drop(session)` and the drain, call
`release(id, epoch)`. Trap 2 in the design section is the reason: a claim
arriving during the 30-second drain must not queue behind it.

- [ ] **Step 7: `TCP_USER_TIMEOUT`**

In `rpc::configure_socket`, add
`sockopt::set_tcp_user_timeout(sock, KEEPALIVE_BUDGET)` where the budget is
`KEEPALIVE_IDLE + KEEPALIVE_INTERVAL * KEEPALIVE_COUNT`. Without it a server
with replies queued for a black-holed peer sits in TCP retransmission for
minutes, holding the session attached past any grace worth configuring and
refusing every claim with `STATUS_SESSION_BUSY`.

- [ ] **Step 8: Expose the ticket in the harness**

`TestClient` keeps the `AttachReply`'s ticket and offers `ticket()`.
`connect_and_attach_with` grows a `resume: bool`.

- [ ] **Step 9: Run the tests, then the gates**

Run: `cargo test -p lbfs-tests --test protocol resume` then `make check` and
`make test-loopback`.
Expected: PASS. `make test-loopback` matters here: the loopback suite counts
the server's descriptors over the export
(`Loopback::export_fds`), and a session held past an unmount would show up as a
descriptor that never comes back. With no `DETACH` yet, the loopback harness's
own unmount leaves the session idle until the reaper takes it — so confirm the
existing fd-census cases still pass, and if a case fails on timing rather than
on a leak, note it for Task 7 rather than loosening it.

- [ ] **Step 10: Commit**

```bash
git add crates/lbfs-server/src/rpc/mod.rs tests/src/lib.rs tests/tests/protocol.rs
git commit -m "feat(server): retain a session for a grace after its socket dies"
```

---

### Task 6: Server — `RESUME`

**Files:**
- Edit: `crates/lbfs-server/src/rpc/mod.rs`
- Edit: `tests/src/lib.rs`, `tests/tests/protocol.rs`

**Interfaces:**
- Consumes: Task 5.
- Produces: `RESUME` as the alternative second frame. The server answers
  `STATUS_OK` with the root's attributes, or one of the three refusal
  statuses.

- [ ] **Step 1: Write the failing tests**

This is the suite that pins the contract. In `tests/tests/protocol.rs`:

1. **The happy path.** Attach, look up a file, open it, read a page. Drop the
   socket. Reconnect, `HELLO`, `RESUME` with the ticket. The old `NodeId` still
   answers `GETATTR`; the old `Fh` still answers `READ` with the same bytes.
2. **A half-paged `READDIR`.** `OPENDIR` a directory with enough entries to
   need two pages, read page one, drop the socket, resume, and read page two
   from the cookie the first connection handed out. Assert the union is the
   whole directory with no duplicates and no gaps.
3. **A wrong secret answers `STATUS_NO_SESSION`**, and the session survives: a
   corrected claim afterwards succeeds.
4. **An unknown id answers `STATUS_NO_SESSION`.**
5. **A claim while the first socket is still attached answers
   `STATUS_SESSION_BUSY`**, and the first socket goes on working.
6. **A claim whose handshake settled a different `max_io_size` answers
   `STATUS_SESSION_MISMATCH`**, and so does one that flipped `writeback`. The
   session survives both, and a matching claim afterwards succeeds.
7. **An expired grace answers `STATUS_NO_SESSION`**, against a server
   configured with a grace of a few hundred milliseconds.
8. **`RESUME` after the handshake is fatal**, like `HELLO` and `ATTACH`.
9. **Identity, part one: unlink during the gap.** Open a file, drop the socket,
   `unlink` it on the export directly, resume. The held `Fh` reads the original
   bytes; `LOOKUP` of the name answers `ENOENT`.
10. **Identity, part two: replace during the gap.** Open a file, drop the
    socket, replace it on the export with different content, resume. The held
    `Fh` still reads the *original* bytes, and a fresh `LOOKUP` yields a
    different `NodeId` and a different `generation` from the one the first
    connection held. **This is the case the whole design exists to get right.**
11. **The ticket is stable.** Resume, drop the socket again, resume again with
    the ticket `ATTACH` minted: it works. Nothing rotates, so nothing needs
    re-learning after a claim.

- [ ] **Step 2: Run them and watch them fail**

- [ ] **Step 3: Write `resume`**

A sibling of `attach` in the handshake sequence. `session()` reads the second
frame's opcode and branches: `Attach` → today's path, `Resume` → the new one,
anything else → `SessionError::Protocol("second frame must be ATTACH or
RESUME")`.

`resume` is one registry call and one `getattr`:

1. `registry.claim(&ticket, &guard)`, where the guard is this handshake's
   settled `Limits` and `writeback`. `NoSession`, `Busy` and `Mismatch` map
   straight to their statuses. A refusal mutates nothing inside the registry,
   so there is nothing to release and nothing to undo.
2. `fs.getattr(ROOT_NODE, None)` for the reply's `root_attr`.

Log every refusal with the peer address and the reason, and every successful
claim with the session id — a refused claim is the line an operator reads when
a mount died, and the claim line is what Task 13's drill greps for.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p lbfs-tests --test protocol resume`
Expected: PASS, all eleven.

- [ ] **Step 5: `make check` and `make test-loopback`**

- [ ] **Step 6: Commit**

```bash
git add crates/lbfs-server/src/rpc/mod.rs tests/src/lib.rs tests/tests/protocol.rs
git commit -m "feat(server): RESUME claims a retained session"
```

---

### Task 7: Server — `DETACH`

**Files:**
- Edit: `crates/lbfs-server/src/rpc/mod.rs`
- Edit: `tests/src/lib.rs`, `tests/tests/protocol.rs`

**Interfaces:**
- Consumes: Task 6.
- Produces: `DETACH` as an ordinary in-session request, answered `STATUS_OK`.

- [ ] **Step 1: Write the failing tests**

1. `DETACH`, drop the socket, reconnect, `RESUME`: `STATUS_NO_SESSION`.
2. `DETACH` with a wrong secret answers `ESTALE` and leaves the session
   claimable — a client that cannot prove ownership must not be able to destroy
   somebody else's session.
3. After `DETACH` the server's descriptors over the export return to baseline
   without waiting for the grace, measured the way the existing fd-census cases
   measure it.
4. A request after `DETACH` on the same socket still works: `DETACH` ends the
   *session's retention*, and the socket serves until it closes.

Point 4 needs a decision recorded in the test's own comment: `DETACH` removes
the registry entry, and the connection keeps its `Arc<dyn FileSystem>` until it
closes, so the export stays served and stops being resumable. That is what a
clean unmount wants.

- [ ] **Step 2: Run them and watch them fail**

- [ ] **Step 3: Handle `DETACH` in the read loop**

Beside `Forget`, which the loop already handles inline. `DETACH` is one
registry call, so spawning a task for it would cost more than doing it — but
unlike `FORGET` it takes a window permit and produces a reply, because the
client waits for it before closing.

- [ ] **Step 4: Run the tests, then `make check` and `make test-loopback`**

- [ ] **Step 5: Commit**

```bash
git add crates/lbfs-server/src/rpc/mod.rs tests/src/lib.rs tests/tests/protocol.rs
git commit -m "feat(server): DETACH drops a session at a clean unmount"
```

---

### Task 8: Client — `Connection::closed()`

**Files:**
- Edit: `crates/lbfs-client/src/conn.rs`
- Edit: `crates/lbfs-client/tests/mux.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub async fn closed(&self)`, which returns when the connection
  dies and returns immediately if it already has.

- [ ] **Step 1: Write the failing tests**

In `mux.rs`: `closed()` on a live connection stays pending; it completes when
the scripted server drops the socket; it completes immediately on a connection
that already died; and two concurrent waiters both complete.

- [ ] **Step 2: Run them and watch them fail**

- [ ] **Step 3: Add the signal**

`Shared` grows a `died: Arc<Notify>`; `Shared::kill` calls `notify_waiters`
after it stores `dead`. `Connection::closed` checks `is_dead()` first, then
waits — checking first is what makes it complete for a connection that died
before anybody asked. `notify_waiters` rather than `notify_one`: every waiter
must learn, and a stored permit for a future waiter would be wrong here because
the `is_dead()` check already covers that case.

**The module's fourth invariant does not change.** A dead `Connection` still
answers `EIO` forever and is never revived. `closed()` reports the death; it
does not undo it.

- [ ] **Step 4: Run the tests, then `make check`**

- [ ] **Step 5: Commit**

```bash
git add crates/lbfs-client/src/conn.rs crates/lbfs-client/tests/mux.rs
git commit -m "feat(client): Connection::closed reports a connection's death"
```

---

### Task 9: Client — the `Session` indirection, with no reconnect yet

**Files:**
- Create: `crates/lbfs-client/src/session.rs`
- Edit: `crates/lbfs-client/src/lib.rs`, `crates/lbfs-client/src/fuse.rs`, `crates/lbfs-client/src/main.rs`, `crates/lbfs-client/src/bin/lbfs-bench.rs`
- Edit: `tests/tests/loopback.rs`, `crates/lbfs-client/tests/loopback_cli.rs` (only where they name `Connection`)

**Interfaces:**
- Consumes: Task 8.
- Produces: `Session`, holding the current `Arc<Connection>` behind a `watch`
  channel, plus the address, the export path, the `Proposal` and the ticket.
  `LbfsFuse` holds an `Arc<Session>`. **A pure refactor: no reconnect, and
  every existing test passes unchanged.**

- [ ] **Step 1: Write the failing test**

One case in `mux.rs`: a `Session` over a live connection forwards a typed call
and returns the same answer the connection would; a `Session` whose connection
died answers `EIO`. Written first so the refactor has a target.

- [ ] **Step 2: Write `Session`**

```rust
enum State {
    Live(Arc<Connection>),
    Reconnecting,
    Dead,
}

pub struct Session {
    state: watch::Sender<State>,
    /// Everything a redial needs. Fixed for the life of the mount.
    addr: SocketAddr,
    export: Vec<u8>,
    proposal: Proposal,
    /// Fixed at attach. Nothing rotates (design §7.6).
    ticket: Option<SessionTicket>,
    /// Clamped to the server's advertised grace. Zero disables reconnection.
    deadline: Duration,
    /// The settled limits, which a resumed session must match. `LbfsFuse::init`
    /// reads them once at mount and the kernel cannot be told a new number
    /// afterwards.
    pub limits: HelloReply,
}
```

In this task `Session::current()` returns the connection or `EIO`, and nothing
ever writes `Reconnecting`. Give it the same call surface `LbfsFuse` uses
today: rather than re-declaring thirty methods, `Session` exposes
`current() -> Result<Arc<Connection>, Errno>` and `LbfsFuse`'s callbacks take
one extra line each.

- [ ] **Step 3: Move `LbfsFuse` onto it**

`LbfsFuse::new` takes an `Arc<Session>`; `ctx` and `entry_ctx` return one.
`init` reads `self.session.limits`. `destroy` reads a `dropped_forgets` that
sums across connections. `main.rs` builds the `Session` after `connect` and
hands it to `LbfsFuse`.

- [ ] **Step 4: Run everything**

Run: `make check` then `make test-loopback`
Expected: PASS with no test changed except the ones that named `Connection`
directly. A refactor that needs a behavioural test edited is a refactor that
changed behaviour — stop and find out why.

- [ ] **Step 5: Commit**

```bash
git add crates/lbfs-client/src/session.rs crates/lbfs-client/src/lib.rs crates/lbfs-client/src/fuse.rs crates/lbfs-client/src/main.rs crates/lbfs-client/src/bin/lbfs-bench.rs crates/lbfs-client/tests/mux.rs tests/tests/loopback.rs crates/lbfs-client/tests/loopback_cli.rs
git commit -m "refactor(client): a Session above the connection"
```

---

### Task 10: Client — the reconnect supervisor and parked calls

**Files:**
- Edit: `crates/lbfs-client/src/session.rs`, `crates/lbfs-client/src/conn.rs`
- Edit: `crates/lbfs-client/tests/mux.rs`

**Interfaces:**
- Consumes: Tasks 6, 8 and 9.
- Produces: `Connection::resume(addr, export, proposal, ticket)` beside
  `connect`; a supervisor task inside `Session` that dials, resumes and
  installs; `Session::current()` that parks while `Reconnecting`.

- [ ] **Step 1: Write the failing tests**

Against a scripted server in `mux.rs`:

1. **In flight fails.** A call outstanding when the socket dies gets `EIO`, and
   gets it promptly rather than after the deadline.
2. **Issued during the gap succeeds.** A call made after the death parks, and
   completes over the second connection once the scripted server answers
   `RESUME`.
3. **A refused claim is final.** A scripted `STATUS_NO_SESSION` marks the
   session dead: the parked call gets `EIO`, and so does every later one, with
   no second dial.
4. **`STATUS_SESSION_BUSY` retries** until the scripted server relents, and
   then succeeds.
5. **A transport failure retries.** A refused dial, then a working one.
6. **The deadline ends it.** A server that never comes back turns every parked
   call into `EIO` inside the deadline, and the deadline bounds the wait — assert the
   elapsed time, because a hang is the failure this bound exists to prevent.
7. **A mismatched version on the second connection is final**, like a refused
   claim.
8. **One ticket, many claims.** A second death and reconnect presents the same
   ticket the `ATTACH` reply carried, and the scripted server sees it twice.

- [ ] **Step 2: Run them and watch them fail**

- [ ] **Step 3: `Connection::resume`**

A sibling of `connect_with` that runs `HELLO` and then `RESUME` in place of
`ATTACH`, under the same end-to-end timeout, with the same `check_settled`
afterwards. It returns `ConnectError::NoSession`, `SessionBusy` or
`SessionMismatch` for the three refusal statuses, so the supervisor can tell a
retry from a surrender without parsing a string.

- [ ] **Step 4: The supervisor**

One task, spawned alongside the `Session`, in a loop:

```text
wait for the current connection's closed()
  → set Reconnecting
  → until the deadline:
        dial + HELLO + RESUME
        Ok           → install the new connection, set Live
        NoSession    → set Dead, stop
        SessionMismatch → set Dead, stop
        VersionMismatch → set Dead, stop
        Busy         → sleep the backoff, retry
        transport    → sleep the backoff, retry
  → deadline reached → set Dead, stop
```

Backoff: 50 ms, doubling to a 1 s ceiling, so a 200 ms blip costs one retry and
a 10 s outage costs a dozen rather than two hundred.

**Only the supervisor writes `Reconnecting`**, and it writes it
before the first dial, so a call arriving between the death and the
first dial parks rather than seeing a stale `Live`.

- [ ] **Step 5: Parked calls**

`Session::current()` becomes async: `Live` returns at once, `Dead` returns
`EIO` at once, `Reconnecting` waits on the `watch` receiver until the state
leaves `Reconnecting`. The wait needs no timeout of its own — the supervisor
guarantees the state leaves `Reconnecting` inside the deadline, and a wait with
its own clock would be a second answer to a question that already has one.

**`send_forget` cannot park.** The method is synchronous and runs on the FUSE dispatch
thread (`conn.rs` says why). While reconnecting it drops the forget and counts
it, which is what the connection already does when its queue is full. Task 11's
log line reports the total.

- [ ] **Step 6: Shutdown cancels reconnection**

`Session::shutdown()` sets `Dead`, which stops the supervisor and fails
everything parked. `main.rs` calls it before it drops the runtime, or a
supervisor still dialling holds the process open past the unmount.

- [ ] **Step 7: Run the tests, then `make check` and `make test-loopback`**

- [ ] **Step 8: Commit**

```bash
git add crates/lbfs-client/src/session.rs crates/lbfs-client/src/conn.rs crates/lbfs-client/tests/mux.rs
git commit -m "feat(client): re-attach to a retained session after a disconnect"
```

---

### Task 11: Client — the CLI and a clean detach

**Files:**
- Edit: `crates/lbfs-client/src/main.rs`, `crates/lbfs-client/src/session.rs`, `crates/lbfs-client/src/conn.rs`
- Edit: `crates/lbfs-client/tests/loopback_cli.rs`

**Interfaces:**
- Consumes: Task 10.
- Produces: `--reconnect-timeout <SECONDS>` (default 10) and `--no-reconnect`;
  a `DETACH` on the shutdown path.

- [ ] **Step 1: Write the failing tests**

Unit cases beside the existing `attr_timeout` and `event_loop_threads` ones:
the flag parses, refuses a negative and an absurd value, and `--no-reconnect`
yields a zero deadline. One `loopback_cli` case: a mount started with
`--no-reconnect` behaves exactly as today when its server dies.

- [ ] **Step 2: Run them and watch them fail**

- [ ] **Step 3: The flags**

Ten seconds by default, and the doc comment carries the reason: it has to stay
under the twenty-second `timeout` that `vm/tests/disconnect.sh` puts around its
post-mortem `ls` and under the loopback suite's thirty-second settle window,
because a mount that parks longer than a test waits looks exactly like a hang.
`Session` clamps whatever it gets to the server's advertised `grace_ms` — a
client still dialling for a session the reaper already dropped is a client
burning time on a guaranteed refusal.

- [ ] **Step 4: `DETACH` on the way out**

`main.rs` unmounts, drains, and then — before dropping the runtime — sends
`DETACH` on the live connection and waits for its reply. A failure goes to the
log and no further: the session expires by itself, and a client that cannot
detach must still exit.

Order matters. `DETACH` goes *after* the unmount drain, because the drain
flushes writeback and the `FORGET`s the kernel emits for every evicted inode,
and both need the session.

- [ ] **Step 5: The dropped-forget line**

`destroy` already warns about dropped forgets. Extend it to name the count
dropped while reconnecting, separately, since that is the one an operator can
act on by shortening the deadline.

- [ ] **Step 6: Run the tests, then `make check` and `make test-loopback`**

- [ ] **Step 7: Commit**

```bash
git add crates/lbfs-client/src/main.rs crates/lbfs-client/src/session.rs crates/lbfs-client/src/conn.rs crates/lbfs-client/tests/loopback_cli.rs
git commit -m "feat(client): --reconnect-timeout, --no-reconnect, DETACH at unmount"
```

---

### Task 12: Loopback — a severable connection

**Files:**
- Edit: `tests/tests/loopback.rs`

**Interfaces:**
- Consumes: Tasks 10 and 11.
- Produces: `Breaker`, a forwarding proxy the test can sever, plus four cases
  through a real mount.

- [ ] **Step 1: Write the proxy**

The loopback harness starts its server in-process and the client connects
straight to it, so no test can sever the socket without killing the server —
which is the one thing this feature must survive. `Breaker` listens on
`127.0.0.1:0`, dials the real server for each accepted connection, copies both
directions, and holds the halves so `sever()` can drop them. `Opts` grows a
`breaker: bool`; when set, `Loopback::start` puts one in the path and points the
client at it.

Keep it small, and say plainly what it stands for: a test double for a flaky
network, not a proxy anybody ships.

- [ ] **Step 2: Write the failing cases**

1. **An open descriptor survives.** Open a file through the mount, write to it,
   `sever()`, wait for the mount to answer again, write more through the *same*
   `std::fs::File`, then read the export directly and assert both writes landed
   in order. A re-open by name would pass this only by accident; a `sever()`
   between the two writes on a file that was also renamed on the export in
   between would not — case 4 covers that.
2. **A directory walk survives.** Open a directory with enough entries to need
   more than one `READDIR` page, read one, `sever()`, read the rest, and assert
   the
   union is the whole directory with no duplicates and no gaps.
3. **The mount unmounts cleanly mid-reconnect.** `sever()`, then unmount
   without waiting for the reconnect. Assert the unmount finishes inside
   `UNMOUNT_TIMEOUT` and the client thread joins — a supervisor still dialling
   must not hold it.
4. **Identity survives.** Open a file, `sever()`, replace the file on the
   export with different content, wait for the mount to come back, and assert
   the held descriptor still reads the *original* bytes while a fresh open by
   name reads the new ones. This is the loopback twin of Task 6's identity
   cases, and the one that fails loudly if anybody ever replaces retention
   with a re-open.
5. **Descriptors come back.** After the unmount, `export_fds()` returns to
   baseline, because Task 11's `DETACH` dropped the session rather than leaving
   it to the reaper.

- [ ] **Step 3: Run the cases**

Run: `cargo test -p lbfs-tests --test loopback sever -- --ignored --test-threads=1`
Expected: PASS.

- [ ] **Step 4: Run the whole loopback suite and `make check`**

Expected: PASS, including
`a_dead_server_leaves_an_eio_mount_that_still_unmounts` unchanged — a server
that died has no session, so the mount dies as it always did, ten seconds later
than before and well inside `SETTLE_TIMEOUT`.

- [ ] **Step 5: Commit**

```bash
git add tests/tests/loopback.rs
git commit -m "test(loopback): a mount survives a severed connection"
```

---

### Task 13: VM — the severed-connection drill

**Files:**
- Create: `vm/tests/reconnect.sh`
- Edit: `vm/test.sh`

**Interfaces:**
- Consumes: every task above.
- Produces: the end-to-end proof, on the guest pair, that a transport failure
  costs latency rather than a mount.

**This is the first task that touches the VM pair. Confirm nobody else holds it
before running anything here.**

- [ ] **Step 1: Write the drill**

Modelled on `vm/tests/disconnect.sh`, which it complements rather than
replaces. Mount, start a large `dd` with `conv=fsync`, wait until the server's
copy of the file is growing — the same measured "mid-I/O" the existing drill
uses rather than a fixed sleep — then kill the *connection* while leaving the
server running:

```bash
vm_ssh "$SERVER_IP" "sudo ss -K dst $CLIENT_IP dport = :$SERVER_PORT"
```

`ss -K` needs `CONFIG_INET_DIAG_DESTROY` and root; both hold on the guests.
Assert afterwards:

- The `dd` may fail, because it had a write in flight. Record which happened
  rather than demanding one — the design fails in-flight requests on purpose,
  and a `dd` that survived means the kernel retried the page.
- A *fresh* write to the mount succeeds within a few seconds, which is the
  whole feature.
- A file opened before the sever and held open across it still reads and writes
  afterwards. Use a small helper that holds a descriptor across the sever;
  `exec 3<>` in the shell is enough.
- The server logs a claim, and its session count returns to one.
- The mount unmounts cleanly.

- [ ] **Step 2: Wire it into `vm/test.sh`**

Beside `disconnect.sh`, after it — the two want the server in a known state and
`disconnect.sh` restores one.

- [ ] **Step 3: Deploy and run**

```bash
make vm-deploy
make vm-test
```

Expected: PASS, including `disconnect.sh` unchanged.

- [ ] **Step 4: Commit**

```bash
git add vm/tests/reconnect.sh vm/test.sh
git commit -m "test(vm): a severed connection costs latency, not the mount"
```

---

## Acceptance Criteria

1. `make check` passes: `cargo fmt --all --check`,
   `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo test --workspace`.
2. `make test-loopback` passes, including the five new severed-connection cases
   and every case that existed before this plan, unchanged.
3. `make vm-test` passes, including `vm/tests/disconnect.sh` unchanged and the
   new `vm/tests/reconnect.sh`.
4. A file held open across a severed connection reads and writes afterwards
   through the same descriptor, and the bytes land in order.
5. A `READDIR` half-way through a directory continues across a severed
   connection with no duplicate and no missing entry.
6. **A file replaced on the export during the gap does not change what a held
   descriptor reads, and a fresh lookup of the name yields a different `NodeId`
   and a different `generation`.** Pinned twice: protocol level (Task 6) and
   through a real mount (Task 12).
7. A request in flight when the socket dies fails `EIO` promptly, and no
   request is ever replayed.
8. A server restart leaves the mount dead, answering `EIO` until unmount, and
   the client never falls back to a fresh `ATTACH`.
9. A claim with a wrong secret, an unknown id, an expired grace or a mismatched
   handshake meets a refusal, and the session survives every refusal short of
   an expiry.
10. After a clean unmount the server's descriptor count over the export returns
    to baseline without waiting for the grace.
11. The protocol version is `3` on both ends, `FLAG_FORCE_SYNC_RESERVED` is
    untouched, and `git diff` shows no change to frame flag bit 1.

## Open Risks

- **The `Session` refactor in Task 9 touches every FUSE callback.** Thirty call
  sites, all mechanical, and a mistake in one shows up as a single operation
  behaving oddly rather than as a compile error. The mitigation is the rule in
  Task 9 Step 4: if the refactor needs a behavioural test edited, it changed
  behaviour and the change is wrong.
- **Parked requests turn a dead server into a ten-second pause.** Spec §8 says
  the outcome a filesystem must not have is a hang, and this adds a bounded one
  where none existed. Ten seconds is a judgement, not a measurement. If a
  workload finds it painful the flag moves it, and `--no-reconnect` restores
  today's behaviour exactly.
- **Requests that died in the gap strand lookup counts and handles.** At most
  `max_inflight` per reconnect — 128 descriptors by default — and they live
  until the session ends rather than until the connection ends. A link that
  flaps repeatedly accumulates them. The client's deadline bounds it in
  practice, since a flap that outlasts ten seconds ends the mount, and
  `init_process` already raises `RLIMIT_NOFILE` to its hard ceiling. Reclaiming
  them needs the undo records and the cumulative acknowledgement the design
  declines; revisit only if it shows up.
- **A retained session holds real memory.** A million-entry `DirHandle`
  snapshot survives the gap along with everything else, and
  `max_resumable_sessions` × that is the worst case. The cap and the grace are
  both operator-facing for this reason, and `resume_grace = "0"` turns the
  feature off on a server that cannot afford it.
- **`ss -K` may not work on a future guest kernel.** It needs
  `CONFIG_INET_DIAG_DESTROY`. If a swapped kernel lacks it, an `nftables` drop
  rule plus a `conntrack` flush reaches the same place with more moving parts.
  Task 13 is the only thing that depends on it, and the loopback proxy in Task
  12 covers the same behaviour without any kernel feature at all.
- **The forced-sync branch and this one both want `PROTOCOL_VERSION` and the
  `Opcode` enum.** Whichever lands second rebases. Nothing else in the two
  changes overlaps: this plan reads no frame flag, and the forced-sync change
  touches no session state.
- **A ticket is a bearer capability on a protocol with no authentication.** An
  observer on the wire reads it out of the `ATTACH` reply, and can also read
  every byte of every file the session carries. The ticket never rotates —
  design §7.6 prices what rotation would buy and names the dead mount a lost
  `ResumeReply` would cost. mTLS remains the answer, and spec §11 already
  carries it.

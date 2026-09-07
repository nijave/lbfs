# lbfs — Session Resumption: Design

Date: 2026-08-28
Status: Proposed

## 1. Overview

Spec §7 promises one thing about a lost connection: every in-flight and later
operation fails `EIO`, the mount stays present, and `fusermount3 -u` still
takes it down. Spec §11 names the reason that is all it promises — "node/handle
state is session-scoped server-side; honest reconnection needs session
resumption" — and puts resumption first in the fast-follow list.

This document decides what a resumed session restores, what it refuses to
restore, and where the boundary between the two sits. It changes the protocol
version from `2` to `3`.

**The ruling in one sentence: the server keeps the session object alive across
the gap and the client re-attaches to it, while every request that was in
flight when the socket died still fails `EIO`.** The design rebuilds nothing
from names, replays nothing, and leaves file identity untouched across a
reconnect.

### Goals

- A mount survives a transport failure that lasts seconds: a TCP reset, a route
  flap, a middlebox dropping an idle flow.
- Node ids, generations, open file handles, open directory handles and their
  `READDIR` cursors all mean after the reconnect exactly what they meant
  before.
- A reconnect never hides a change to the files underneath. What the client
  cannot know, the client reports as an error.
- The failure this replaces stays available: when the server cannot honour the
  claim, the mount dies the way §7 already describes.

### Non-goals

- Surviving a server restart. Sessions live in memory and die with the process.
  §11 prices a persistent design.
- Exactly-once semantics for the requests that crossed the gap. §3.1 argues
  that a reply cache is the wrong first step.
- Authentication. The session ticket is a bearer capability, and §6.2 prices
  what holding one buys.
- More than one socket per session. The registry rule in §6.3 leaves room for
  the per-core connection scaling §3.1 of the main spec anticipates.

## 2. What a session is, and why it cannot be rebuilt

`ATTACH` constructs one `LocalFs` per connection
(`crates/lbfs-server/src/rpc/mod.rs`, `fn attach`). That object owns four
things the client depends on and nothing else does:

| State | Owner | What the client holds |
|---|---|---|
| Node table | `fs::local::nodes::NodeTable` | `NodeId` + `generation`, one `O_PATH` fd per node, `nlookup` per node |
| Open files | `LocalFs::files` | `Fh` → `{ node, fd }` |
| Open directories | `LocalFs::dirs` | `Dh` → `{ node, fd, snapshot, cookie map }` |
| Negotiated shape | `Limits` and `LocalFs::writeback` | window, max I/O size, whether the client's kernel owns file size |

The session task returns when the socket ends, the `Arc<dyn FileSystem>` drops,
and every descriptor closes. That is the whole of today's behaviour.

### 2.1 Rebuilding node state by name is unsound

The obvious alternative to keeping the table is rebuilding it: on reconnect the
client re-looks-up each node it still remembers and the server re-issues the
same id. Three separate facts kill it.

**The client does not know the names.** FUSE hands the client daemon a
`(parent, name)` pair at lookup time and never again. Nothing in
`crates/lbfs-client/src/fuse.rs` keeps a reverse map from `NodeId` to a path,
and building one means tracking every `RENAME`, `UNLINK` and `LINK` that passes
through the mount — a second namespace, maintained by the side that has the
least information about it.

**Some nodes have no name.** A file the client holds open after an `unlink`
lives only as an inode with a descriptor pointing at it. `LocalFs` supports
that today because the node's fd pins the inode. A rebuild by name has nothing
to look up.

**A name can resolve to a different file.** This is the failure that matters.
If something replaced `/export/a.txt` during the gap, re-looking it up binds the
client's remembered `NodeId` to a fresh inode. The client's kernel goes on
believing the id names the file it opened, and every read afterwards returns
bytes from a file the application never opened. An `EIO` is a bad afternoon; a
node id that quietly changed meaning is data loss with no message attached.

The node table's own module documentation already states the property the
rebuild would break: an `O_PATH` descriptor "pins the inode for as long as the
kernel holds a lookup count on the id, so `(st_dev, st_ino)` cannot come to name
a different file underneath a live node". Dropping the descriptor drops the
pin.

**Ruling: the server keeps the node table, descriptors and all. A resumed
session hands back the same `NodeId` values because the table never went away.**

### 2.2 Re-opening file handles by name is unsound for the same reason, harder

An `Fh` is an index into `LocalFs::files`, and a real descriptor sits behind it,
opened with flags the server already masked (`LocalFs::mask_open_flags`).
Re-opening it across the gap needs a name, which §2.1 has already ruled out, and
it adds two failures of its own: something may have replaced the file, so the
re-opened descriptor addresses different bytes; something may have unlinked it,
so the re-open fails and an application's descriptor breaks under it for a
reason POSIX does not allow.

Local POSIX says an open descriptor keeps addressing its inode through renames
and unlinks. Keeping the descriptor is what makes that true across a network
too. Re-opening makes lbfs the one filesystem where `open` then `rename` then
`read` returns somebody else's data.

**Ruling: the server keeps the descriptors. `Fh` and `Dh` values survive
verbatim.**

### 2.3 Directory snapshots make the hardest case free

Spec §3.3 already reads: "Directory listings snapshot at `OPENDIR`. The server
reads the whole directory once and the handle keeps that list… Every other
offset must be a cookie this handle handed out, and one the server never issued
answers `EINVAL` rather than truncating the listing in silence."

A half-paged `READDIR` across a reconnect is a non-event under retention.
The `DirHandle` survives, the snapshot survives, the `resume` map
that turns a cookie into an index survives, and the next `READDIR` at the last
cookie the client received continues the same listing.

Under any rebuilding design the same case is a choice between two bad answers.
A re-`OPENDIR` mints a fresh snapshot with fresh `d_off` cookies, so the
client's saved cursor is a cookie the new handle never issued: `EINVAL` by the
rule above, or a silently truncated or duplicated listing once that rule
relaxes. `readdir(3)` returning a partial directory with no error is exactly
the class of failure this design refuses.

**The snapshot rule earned its place for a different reason and pays for this
one.**

## 3. The correctness boundary

Retention decides what survives. This section decides what still fails.

### 3.1 The gap fails: requests in flight at the break return `EIO`

When the socket dies, the client has a set of `request_id`s with no reply. For
each of them the client knows nothing: the server may never have read the
frame, may have executed it and lost the reply to the broken socket, or may
have died mid-write with the reply half-delivered.

Re-issuing them is unsound across most of the opcode table:

| Opcode | What a replay does |
|---|---|
| `CREATE` with `O_EXCL`, `MKDIR`, `SYMLINK`, `LINK` | `EEXIST` for an operation that succeeded |
| `UNLINK`, `RMDIR` | `ENOENT` for an operation that succeeded |
| `RENAME` with `NOREPLACE` | `EEXIST`; with `EXCHANGE`, swaps back |
| `WRITE` on a session with `writeback = false` | The server's descriptor carries `O_APPEND` (`mask_open_flags`), so replayed bytes append twice |
| `SETXATTR` with `XATTR_CREATE` | `EEXIST` for an operation that succeeded |
| `LOOKUP` and the entry-returning ops | A second increment of `nlookup`, so one node never reaches zero |
| `OPEN`, `CREATE`, `OPENDIR` | A second handle, and the first one leaks |

Answering these correctly means a duplicate-request cache: per-slot sequence
numbers, cached replies, and a cumulative acknowledgement so the server can
retire a cached reply. That is the NFSv4.1 session and its exactly-once
machinery, and it reaches into the window, the frame header and every handler.
This design declines it and takes the plain position instead.

**Ruling: every request with no reply at the moment of the break fails `EIO`,
exactly as it does today. The protocol gains no replay and no reply cache.**

The mount promises the same thing about that `EIO` it promises today: the
operation may or may not have happened, and the application decides what to do
about it. The design claims nothing new here, and nothing new can go wrong.

### 3.2 Requests the client had not yet sent park and then run

A request that arrives while the client is dialling has not begun. No part of
its outcome sits in doubt, and nothing about it needs to fail. The client parks it
until one of two things happens: the session comes back, in which case the
request goes out on the new socket; or the client abandons reconnection, in
which case the request gets `EIO`.

This split is the whole user-visible feature:

- **In flight at the break → `EIO` immediately.** A `dd` mid-write still
  reports a failure, at the same moment it does today.
- **Issued during the gap → latency, then success.** The compile job that
  opens its next source file half a second into a two-second blip waits and
  then reads it.

A client-side deadline bounds the parking window, default 10 seconds
(§8.2). Past that the mount is dead in the §7 sense and stays dead. A
filesystem that hangs is worse than one that fails, and spec §8 already names
that as the outcome the design must not have.

### 3.3 What a resumed session must never paper over

Five things stay visible.

1. **An unknown outcome.** §3.1. No replay under any circumstance.
2. **A refused claim.** If the server has no session for the ticket, the client
   does not fall back to a fresh `ATTACH`. §5 explains why that fallback is the
   worst available option.
3. **A forgotten node.** Retention resurrects nothing. An id the session already
   forgot answers `ESTALE`, and a released handle answers `EBADF`, on the same
   terms as before.
4. **A file replaced or unlinked during the gap.** A retained descriptor keeps
   addressing its original inode, which is what an uninterrupted session would
   do and what a local descriptor does. A fresh `LOOKUP` of the name yields a
   different `NodeId` with a different `generation`. The old id never quietly
   becomes the new file.
5. **A change to the negotiated shape.** The server refuses a claim whose
   handshake settled different limits or a different `writeback` value (§7.4).
   The client's FUSE mount took `max_write` and `max_background` from the first
   handshake and no later handshake can move them, and the retained handles
   carry the first `writeback` value in their open flags.

## 4. Restart against blip: the server decides, the client never guesses

The question "did the server restart, or did the network blip?" has an
appealing wrong answer — a boot id, a start timestamp, a heuristic on how long
the gap lasted. Every one of them is a guess the client makes about a fact only
the server holds, and a wrong guess turns a clean `ESTALE` into silent reuse of
ids that mean something else.

The design removes the question. The client always presents its ticket and the
server always answers from its own registry:

| Server condition | Answer |
|---|---|
| Process restarted | The registry is empty. `STATUS_NO_SESSION`. |
| Grace expired | The reaper already dropped it. `STATUS_NO_SESSION`. |
| Ticket unknown or secret wrong | `STATUS_NO_SESSION`. |
| Session held, socket already gone | `STATUS_OK`, with the root's attributes. |
| Session held, another socket still attached | `STATUS_SESSION_BUSY`. |
| Session held, negotiated shape differs | `STATUS_SESSION_MISMATCH`. |

A restarted server cannot answer `STATUS_OK` by accident: tickets carry 128
random bits and live only in memory, so a fresh process holds none of them.
A second server behind the same address holds none of them either, and refuses.

**Ruling: resumption is server-attested. The client contributes a ticket and an
expectation about limits, and believes the answer.**

## 5. Why a refused claim must not fall back to `ATTACH`

The tempting recovery from `STATUS_NO_SESSION` is a fresh `ATTACH` — the client
already knows how, and the mount would come back. It would also corrupt.

A fresh session starts its node counter at `ROOT_NODE + 1` and its generation
counter at 1 (`NodeTable::new`). The client's kernel still holds node ids from
the dead session, and a FUSE request carries a node id with no generation
beside it. Node 7 in the kernel's dentry cache would land on whatever node 7
the new session happens to issue next. The server has no way to detect the
mistake and the client has no way to signal it.

The only thing a fresh attach carries over intact is `ROOT_NODE`, which is `1`
by definition. Everything below it becomes a coin flip.

**Ruling: a refused claim leaves the mount dead. Every pending and later
request gets `EIO`, the mount stays present, and `fusermount3 -u` takes it
down — spec §7, unchanged.**

*Alternative considered.* A fresh attach whose node counter starts far above
anything the previous session issued would make every remembered id answer
`ESTALE` rather than alias. The kernel retries some path lookups on `ESTALE`,
so parts of a workload would recover by themselves. That design deserves its
own document: it needs a rule for how far "far above" reaches across repeated
restarts, a study of which VFS paths retry and which hand the error to the
application, and an answer for the open descriptors that cannot recover at all.
§11 keeps it as future work.

## 6. Session identity and what a ticket buys

### 6.1 The ticket

`ATTACH` mints a ticket and returns it:

```rust
pub struct SessionTicket {
    /// Registry key. A server-local counter, never reused within a process.
    pub id: u64,
    /// 128 bits from `getrandom(2)`. The whole of the authentication.
    pub secret: [u8; 16],
    /// How long this server holds the session after a socket dies, in
    /// milliseconds. The client clamps its own reconnect deadline to it.
    pub grace_ms: u32,
}
```

The client holds it for the life of the mount and presents it in `RESUME`. The
same ticket serves every claim the session ever makes; §7.6 records why
nothing rotates. The server compares the secret in constant time and treats an
unknown id and a wrong secret identically, so a guesser learns nothing from
the difference.

### 6.2 What a stolen ticket buys, in a protocol with no authentication

Spec §1 sets the trust model at "a network you would run plaintext NFS on", and
this design does not raise it. Naming the exposure precisely is what it can do.

A peer that guesses a ticket and wins the race against the real client gets a
session on an already-attached export. Compare that against what the same peer
gets today with no ticket at all: a `HELLO` and an `ATTACH` to any allowlisted
path, which grants read and write access to the same tree. The extra a ticket
buys stays narrow, and stays real:

- Descriptors for files an unlink or a rename has since moved out of reach,
  which a fresh attach cannot open.
- The chance to evict the legitimate client, whose next resume answers
  `STATUS_SESSION_BUSY` until its deadline runs out.

Guessing costs 2^128 tries against a server that answers one claim per round
trip. The exposure that matters is a passive observer on the wire, who reads the
ticket out of the `ATTACH` reply — and that observer can also read every byte of
every file the session carries. mTLS remains the answer to both, and §11 of the
main spec already carries it.

**Ruling: a 128-bit random secret, compared in constant time, logged nowhere.
The design records the exposure rather than pretending v1 closed it.**

### 6.3 One socket per session

A session admits one attached socket at a time. A claim against a session that
still has one answers `STATUS_SESSION_BUSY` and changes nothing.

Refusing beats stealing. A steal is the session-hijack primitive in a protocol
with no authentication, and the case it would serve — a half-open connection
where the client saw a reset and the server has not noticed yet — resolves on
its own within roughly 25 seconds through the keepalive the server already sets
(`rpc::configure_socket`). The client retries and the second claim succeeds.

One case needs help. Keepalive fires only on an idle socket; a server with
replies queued for a black-holed peer sits in TCP retransmission for minutes,
holding the session attached past any grace worth configuring. The server sets
`TCP_USER_TIMEOUT` to match the keepalive budget, so a write the peer never
acknowledges fails inside the same ~25 seconds an idle socket takes to die.

## 7. Protocol changes

### 7.1 Version 3

`PROTOCOL_VERSION` moves from `2` to `3`, and the handshake stays an exact
match. Spec §3.2 already gives the reason and it applies again: postcard
ignores trailing bytes, so a version-2 server decoding a version-3 `HELLO`
would drop the client's request to resume and answer as though the client had
never asked. The client would then hold a mount it believes is resumable and
discover otherwise at the worst moment. An exact-match refusal turns that into
a startup failure an operator can read.

Spec §11 nominated the `HELLO` version field as the vehicle for this change.
This is that.

### 7.2 `HELLO`

`HelloRequest` grows one field:

```rust
    /// Whether this client will try to resume its session after a
    /// disconnection. A server that answers `resume_grace_ms == 0` declines.
    pub resume: bool,
```

`HelloReply` grows one:

```rust
    /// How long this server holds a session after its socket dies, in
    /// milliseconds. Zero means retention is off and every disconnection is
    /// final.
    pub resume_grace_ms: u32,
```

The client learns before `ATTACH` whether the feature exists on this server,
which keeps the ticket out of the reply when nothing will ever use it.

### 7.3 `ATTACH`

`AttachReply` grows `ticket: Option<SessionTicket>`. `Some` when both sides
asked for retention, `None` otherwise.

### 7.4 `RESUME` — opcode 34

`RESUME` replaces `ATTACH` as the second frame of a resuming connection. The
read loop's existing rule ("HELLO or ATTACH after the handshake" is fatal)
extends to it.

```rust
pub struct ResumeRequest {
    pub ticket: SessionTicket,
}

pub struct ResumeReply {
    /// Freshly stat'd, exactly as `ATTACH` reports it.
    pub root_attr: FileAttr,
}
```

The registry runs every check under its one lock: the id exists; the secret
matches in constant time; the entry is idle, and idle within its deadline; the
settled `Limits` and `writeback` equal the values stored at mint. A refusal at
any step answers the status from §4's table and mutates nothing — not the
secret, not the state, not the deadline — so a client that corrects itself can
claim again, and a wrong claim cannot extend retention.

### 7.5 `DETACH` — opcode 35

A clean unmount must not leave a session holding descriptors for the grace
period. The socket closing cannot carry that meaning, because a crashed client
closes its socket the same way a polite one does — and the crashed client is
exactly the case retention exists for.

`DETACH` says it out loud: drop this session now. The client sends it
during shutdown, before dropping the connection, and waits for the reply. A
client that dies without sending one leaves its session to the reaper, which is
the intended behaviour.

### 7.6 The ticket does not rotate

An earlier draft returned a fresh secret on every claim, to bound a leaked
secret's useful life to one gap. Dropped, for a failure the flaky link makes
routine: the server rotates inside the claim, the `ResumeReply` dies on the
wire — a second break during the reconnect, precisely the scenario retention
targets — and the client now holds a consumed secret. Its next claim answers
`STATUS_NO_SESSION` and the mount dies in the one case the feature exists to
survive. Making rotation safe means acknowledging delivery of the new secret,
which is the retired-reply machinery §3.1 declines. What rotation bought was
already small: §6.2's observer, the only adversary who can read a ticket, can
also read every byte of every file. One secret serves the session's whole
life, and mTLS remains the real answer (main spec §11).

### 7.7 New statuses

```
STATUS_NO_SESSION       = 0xFF04
STATUS_SESSION_BUSY     = 0xFF05
STATUS_SESSION_MISMATCH = 0xFF06
```

### 7.8 What does not change

The frame header keeps its layout, its lengths and its flags. **Flag bit 1
stays reserved for the forced-sync control message of spec §11, and nothing in
this design reads or writes it.** No opcode grows a field. Bulk data still
travels outside the serializer.

## 8. Structure

### 8.1 Server

A `SessionRegistry` on `Server`, beside the uring executor and the buffer pool,
because a session must outlive the connection that made it:

```rust
struct Retained {
    fs: Arc<dyn FileSystem>,
    /// The negotiated shape at mint. A claim must present an equal one.
    limits: Limits,
    secret: [u8; 16],
    /// Incremented on every claim, and the single home of the value: a
    /// session task hands its own epoch back at teardown and does nothing
    /// when the entry's has moved on.
    epoch: u64,
    state: State,
}

enum State {
    Attached,
    Idle { deadline: Instant },
}
```

Four operations, each under one lock:

- **`mint`** — `ATTACH` registers a fresh session, shape and all, as
  `Attached` with epoch `0`.
- **`claim`** — `RESUME` verifies the ticket and the caller's settled shape
  against the stored one, requires `Idle` within its deadline, bumps the epoch
  and returns the `Arc<dyn FileSystem>`. Every refusal — wrong secret, busy,
  expired, mismatched shape — mutates nothing.
- **`release`** — the session task, at teardown, presents its epoch. A match
  against an `Attached` entry flips it to `Idle { deadline: now + grace }`; a
  mismatch means another socket already claimed it, and the call does nothing.
- **`drop`** — `DETACH`, and the reaper on expiry, remove the entry.

**Teardown flips to `Idle` before the drain, not after.** `serve_requests`
waits up to `DRAIN_TIMEOUT` (30 s) for replies it already produced, and a
resume arriving one second into that wait must not queue behind it. The drain
still runs; it just no longer holds the session hostage. Handler tasks from the
dead socket keep a clone of the `Arc<dyn FileSystem>` and may still be mutating
the node table while the new socket serves requests against it, which is safe
for the same reason two concurrent handlers on one socket are safe: every table
sits behind its own mutex.

**The reaper** is one task per server, waking on an interval, dropping expired
entries. Each drop goes to `spawn_blocking`, for the reason `LocalFs::releasedir`
already gives about its own snapshots: freeing a large directory's entries is a
million small deallocations, and the closes behind an unlinked file's last
descriptor can cost tens of milliseconds of journal work.

**Configuration** grows two keys:

```toml
resume_grace = "60s"   # "0" turns retention off entirely
max_resumable_sessions = 64
```

The cap bounds what a client that connects and vanishes repeatedly can
accumulate. Past it, `ATTACH` still succeeds and simply mints no ticket:
retention is best-effort, and a client without one gets today's behaviour.

### 8.2 Client

**The `Connection` invariant survives verbatim.** `crates/lbfs-client/src/conn.rs`
states that a dead connection stays dead, and it still does — a `Connection`
that has failed answers `EIO` forever and is never revived. The change adds an
object above it:

```
LbfsFuse ──▶ Session ──▶ Arc<Connection>   (swapped on reconnect)
                 │
                 └──▶ supervisor task: await death → dial → HELLO → RESUME → install
```

`Session` holds the address, the export path, the `Proposal`, the ticket and a
`watch` channel carrying the current state (`Live(Arc<Connection>)`,
`Reconnecting`, `Dead`). `LbfsFuse` holds an `Arc<Session>` where it holds an
`Arc<Connection>` today, and `fn ctx` reads the current connection out of it.

**The client asks for resumption; nothing assumes it.** The `Proposal` gains a `resume`
flag that defaults to **off**, so every embedder of the client library — the
loopback harness, the driver tests, `lbfs-bench` — keeps today's teardown
semantics untouched: no ticket, no retention, no parking. The shipped
`lbfs-client` binary asks for resumption by default, and `--no-reconnect`
clears the handshake request as well as the deadline, restoring today's
behaviour exactly.

`Connection` gains one method — `closed().await`, waking when `Shared::kill`
runs — so the supervisor learns of a death without polling. Adding a `Notify` to
`Shared::kill` is the whole of it.

The supervisor runs one reconnect at a time. It dials with backoff until it
succeeds or its deadline runs out, and it distinguishes two failures:

- **Transport failure** (refused, timed out, reset): keep trying. A server
  restarting under `systemctl restart` refuses connections for a second or two
  and then answers.
- **`STATUS_NO_SESSION`, `STATUS_SESSION_MISMATCH`, or a version mismatch**: a
  server answered and does not have this session. Stop immediately and mark the
  session dead. Retrying cannot change the answer.
- **`STATUS_SESSION_BUSY`** is the one exception on the transport side: the
  server has the session and has not yet noticed the old socket. Keep trying
  until the deadline.

**The deadline defaults to 10 seconds**, clamped to three-quarters of the
server's advertised `resume_grace_ms`. Ten seconds covers a reset plus a redial and a service
restart, and it stays comfortably inside the 20-second `timeout` that
`vm/tests/disconnect.sh` puts around its post-mortem `ls`, and inside the
30-second `SETTLE_TIMEOUT` the loopback suite waits for the first `EIO`. A CLI
flag `--reconnect-timeout` moves it; `--no-reconnect` sets it to zero and
restores today's behaviour exactly.

The client-side deadline stays strictly below the server-side grace on
purpose, and three-quarters rather than merely-not-above: a client still
dialling for a session the reaper already dropped is a client burning time on
a guaranteed refusal, and a clamp that could land *on* the grace would leave
it dialling at the exact moment the reaper fires.

## 9. What leaks, and the bound on it

Retention turns two former non-issues into accounting.

**Lookup counts and handles stranded by the gap.** A `LOOKUP` the server
executed whose reply died with the socket leaves `nlookup` one higher than the
client believes. An `OPEN` in the same position leaves a handle nothing will
ever release. Today the session dies and reclaims every one; under retention
they survive to the end of the session.

The bound is the window. At most `max_inflight` requests can be outstanding at
the break, so one reconnect strands at most 128 descriptors by default. A link
that flaps repeatedly accumulates, which is what the client's 10-second
deadline bounds in practice — a flap that outlasts it ends the mount. The
server counts strandable requests per resume and logs the total, so an operator
watching `EMFILE` has the number in front of them. `init_process` already
raises `RLIMIT_NOFILE` to its hard ceiling for exactly this class of problem.

**Forgets queued behind a dead socket.** `Connection`'s forget batcher holds up
to `FORGET_QUEUE` items, and they die with the connection. Their nodes stay
resident for the life of the retained session. Moving the batcher up into
`Session` so the queue survives a swap would fix it and is deliberately not part
of this design: it changes an object whose loss the current code documents as
harmless, and the harm it now does stays bounded and measurable. §11 keeps it.

**Retained sessions with no client coming back.** Bounded three ways: the
grace, the `max_resumable_sessions` cap, and `DETACH` on a clean unmount.

## 10. Interaction with what already exists

**The client's page cache across the gap.** `entry_timeout` and `attr_timeout`
default to one second and `keep_cache` is on, so the kernel may answer from
cached attributes for the duration of a short gap. Under the one-client
assumption of spec §1 nothing else writes the export, so those caches are as
valid after the gap as before it. A server restart refuses the claim, so the
case where they would be wrong never reaches a live mount. Concurrent writers
on the server remain the pre-existing gap spec §8 already documents.

**`vm/tests/disconnect.sh` passes unchanged.** It stops the server, which
empties the registry, so every claim answers `STATUS_NO_SESSION` and the mount
dies exactly as the drill asserts. The one difference is timing: the drill's
post-mortem `ls` runs after the client's reconnect deadline rather than
immediately, and 10 seconds sits inside its 20-second `timeout`. A new drill
(§12) covers the case the existing one cannot: a severed connection to a server
that is still running.

**`a_dead_server_leaves_an_eio_mount_that_still_unmounts` passes unchanged**
and on today's clock: the loopback harness never asks to resume (§8.2), so its
mount dies without a reconnect park. The same holds for every existing
loopback and driver test — with the library default off, no session lingers
and the fd-census cases measure exactly what they measured before. Only the
severed-connection cases of §12 opt in.

**Unmount must cancel reconnection.** `main.rs` unmounts, drains and exits. A
supervisor still dialling would hold the runtime open. Shutdown marks the
session dead, which stops the supervisor and fails anything parked.

## 11. What this design does not do

- **No replay and no reply cache.** §3.1.
- **No survival across a server restart.** Sessions are memory-only. Persisting
  one means persisting descriptors, which needs `name_to_handle_at` per node,
  `open_by_handle_at` to restore, `CAP_DAC_READ_SEARCH` to call it, filesystem
  support for file handles, and an answer to what a stale handle means after
  inode reuse. A separate design.
- **No cold re-attach with a poisoned id space.** §5 keeps it as an
  alternative worth its own document.
- **No authentication.** §6.2 prices the ticket.
- **No multi-socket sessions.** §6.3's one-socket rule is the conservative
  choice, and the per-core scaling of spec §3.1 would replace it with a rule
  about how many, not whether.
- **No forget-queue survival.** §9.
- **No change to frame flag bit 1**, which belongs to the forced-sync
  fast-follow.
- **No per-request timeouts.** Spec §8 declines them and this design does not
  reopen the question. A request parked on a live socket that never answers is
  the same hang it produces today.

## 12. Testing

Following spec §10's layers.

**Unit.** Ticket comparison in constant time; the registry state machine —
mint, claim, refuse-busy, refuse-mismatch, expire, detach, and the epoch rule
that stops a superseded session task from re-idling a claimed entry; the
duration parser for `resume_grace`.

**Protocol integration**, the layer that pins this contract, in
`tests/tests/protocol.rs` against a real server over a tempdir with no FUSE:

- `ATTACH` returns a ticket; `HELLO` reports a grace.
- Drop the socket, reconnect, `RESUME`: the old `NodeId` still resolves, the
  old `Fh` reads the same bytes, and a half-paged `READDIR` continues from the
  cookie the first connection handed out.
- A wrong secret and an unknown id both answer `STATUS_NO_SESSION`.
- A claim against a still-attached session answers `STATUS_SESSION_BUSY`.
- A claim whose handshake settled different limits answers
  `STATUS_SESSION_MISMATCH`, and a corrected claim afterwards succeeds.
- `DETACH` drops the session: the next claim answers `STATUS_NO_SESSION` and
  the server's descriptors for the export are back to baseline.
- Grace expiry drops it, with the grace configured down to milliseconds.
- **The identity cases.** Unlink the file behind a held `Fh` during the gap:
  the resumed handle reads the original bytes and `LOOKUP` of the name answers
  `ENOENT`. Replace the file during the gap: the resumed handle still reads the
  original bytes, and a fresh `LOOKUP` yields a different `NodeId` and a
  different `generation`. These two are the assertions that say the design did
  not paper anything over.

**Client multiplexer**, in `crates/lbfs-client/tests/mux.rs` against a scripted
server: a call in flight when the socket dies gets `EIO`; a call issued
afterwards parks and completes on the second connection; a scripted
`STATUS_NO_SESSION` kills the session for good; the deadline turns a server
that never comes back into `EIO`.

**Loopback**, which needs a way to sever a connection without killing the
server. The harness grows a small forwarding proxy between client and server
with a `sever()` method, and an opt-in switch so exactly these cases request
resumption. Cases: a `std::fs::File` held open across a sever
still reads and writes; a directory walk in progress still completes; the mount
unmounts cleanly while a reconnect is in flight; and the export's descriptor
count returns to baseline after `DETACH`.

**VM.** `vm/tests/disconnect.sh` unchanged. A new `vm/tests/reconnect.sh` kills
the TCP connection out from under a running server (`ss -K dst <client>`, which
needs `CONFIG_INET_DIAG_DESTROY` and root — both present on the guests) while a
`dd` runs, and asserts the write completes rather than failing.

## 13. Open questions

- **The strandable-request count.** §9 bounds it at `max_inflight` per
  reconnect and offers no way to reclaim it. A client that told the server
  which `request_id`s never arrived would let the server undo their handle and
  lookup-count side effects. That needs the server to keep an undo record per
  dispatched request and a cumulative acknowledgement to retire them — the same
  machinery §3.1 declines. Worth revisiting only if the leak shows up in
  practice.
- **`resume_grace` default.** 60 seconds is a guess: long enough for a route
  flap, short enough that a crashed client's descriptors come back before an
  operator notices. Nothing measures it yet.
- **Whether `RESUME` should re-verify the allowlist.** This design says no,
  on the grounds that a resumed session's authority equals an uninterrupted
  session's and no more, and an uninterrupted session never re-verifies. An
  operator who edits the allowlist expects a restart to apply it, and a restart
  clears every session.

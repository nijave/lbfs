# lbfs code review

Date: 2026-08-22
Commit reviewed: `1c985b2`
Scope: the whole workspace — about 7,200 lines across `lbfs-proto`, `lbfs-server`,
`lbfs-client`, and the `tests` crate.

## Result

No critical findings. No important findings. Five minor findings, listed below.
None of them block v1. The first two are hardening for the mTLS milestone or the
first production deployment. The rest are notes for follow-up work.

The review gate was green at review time: `make check` runs
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, and 198 tests with zero failures. The 20 loopback tests stayed
ignored, as their `make test-loopback` target intends.

## Method

Four research checklists informed the review:

1. FUSE semantics from the kernel's `fuse.rst` plus the `fuser` 0.15 sources:
   reply discipline, nlookup accounting, writeback-cache offset ownership,
   readdirplus cookie rules, xattr size-probe conventions, `max_background`.
2. io_uring pitfalls: buffer and descriptor lifetime, completion mapping,
   cancellation, shutdown, short-transfer handling.
3. tokio patterns: cancellation safety, window accounting, channel bounds,
   panic containment, half-close behavior.
4. Rust practices: unsafe scope, integer truncation, postcard wire stability,
   errno mapping, buffer reuse soundness.

The review read every source file against those checklists, then re-checked
each candidate finding against the code before it entered this document. Early
candidates died there: the vectored-write slice arithmetic in
`lbfs-proto/src/io.rs` is correct and tested, and the window accounting between
the two ends is conservative rather than merely equal.

## Minor findings

### 1. The server puts no bound on concurrent sessions

`accept_loop` (`lbfs-server/src/rpc/mod.rs`) spawns one session task per
accepted connection with no cap. A client that opens many connections grows the
memory of the server without limit: each session carries its own node table,
handle tables, and request tasks. The io_uring lanes and the buffer pool belong
to the whole server, so the growth stops at file descriptors and RAM, not
before.

This matches the documented v1 trust model: run the server on a network you
trust, and mTLS comes next. It still deserves a cheap control. A
`max_sessions` configuration key that refuses connections past the cap would
turn an accident into a log line.

### 2. One dead io_uring lane leaves every export answering EIO until restart

`URING_THREADS = 1` (`rpc/mod.rs`). The lane's fatal path (`abandon` in
`fs/local/uring.rs`) deliberately leaks in-flight payloads instead of freeing
memory the kernel may still write to. That choice is correct. Its consequence is
not widely known inside the code: after an abort, later submissions panic, the
per-request containment turns each into EIO, and the process keeps running as a
server that can never do I/O again.

The path itself is defensive armor — `submit_and_wait` reports a non-retryable
error only in situations the design makes unreachable. But if it ever fires,
only a manual restart recovers the mount, and `vm/lbfs-server.service` ships
with `Restart=no`. Two changes close this gap:

- Make a fatal lane error abort the process, so a supervisor restarts it.
- Set `Restart=on-failure` in the unit file.

Raising `URING_THREADS` is also worth revisiting when profiling shows large
parallel streaming I/O behind one ring.

### 3. The client READ data path does not match spec §3.1 wording

The spec says receivers read bulk data directly into a pooled buffer. The
client allocates a fresh `Vec<u8>` per reply (`conn.rs`, `read_loop`) and copies
the bytes again into fuser's reply buffer. Correctness does not suffer, and the
copy is inherent to `ReplyData::data(&[u8])`. The pooled groundwork exists on
the server side only. Defer this until profiling demands it, but note that the
spec text promises more than the code does.

### 4. Pre-epoch timestamps come back wrong through fuser

The client converts times correctly in both directions (`fuse.rs`), including
negative epochs. fuser then re-encodes the values for the kernel and mangles
times before 1970 on the way. The README documents this under known
limitations. The fix lives upstream in fuser, not here.

### 5. Small items

- `NodeId` and `Fh` are bare `type` aliases (`lbfs-proto/src/types.rs`).
  Newtypes would stop a node id and a handle crossing paths in a refactor.
- `write_frame` checks header-versus-slice agreement with `debug_assert_eq!`
  (`lbfs-proto/src/io.rs`). Release builds skip the check. All callers are
  correct today. A runtime error would keep it that way tomorrow.
- The xattr opcodes `FGetXattr`/`FSetXattr` need Linux 5.19 or newer (this
  note first said 6.5 — the adjudication corrected it: the man page's 6.5
  belongs to `IORING_OP_WAITID`, the adjacent entry). The server never probes
  opcode support at startup, so an older kernel turns xattr operations into
  odd per-op errnos instead of one clear startup failure. The supported
  target (Ubuntu 26.04) is far newer than 5.19, so this stays out of the
  envelope — a startup `Probe` would still fail more kindly.

## Areas checked and sound

These are the failure modes the review hunted for. Each held up under
re-examination, and the list records why, so a future review knows where the
arguments that carry the weight live.

**Window accounting, both ends.** The server admits a request with a permit and
releases it only after the reply is on the socket. The client takes its permit
before send and parks it in the correlation table until the reply arrives. That
makes the client strictly stricter than the server; no legal client can
overrun. `FORGET` rides outside the window on both ends by matching decisions,
each documented at its site.

**Cancellation safety.** The client reserves room on the outbound queue before
it registers a waiter, and nothing awaitable sits between registration and
queueing. A dropped future cannot strand a permit. The server answers even a
panicking handler with EIO for its own request id, so one bad backend call
cannot strand a client forever.

**io_uring memory safety.** Every pointer an SQE names targets memory the slab
owns, and the slab releases an entry only after its CQE. Every descriptor
argument travels as a cloned `Arc<OwnedFd>`, which removes fd-reuse races by
construction. Cancellation drops payloads on the ring thread after completion.
An `openat2` result becomes an `OwnedFd` on the ring thread, so cancelled opens
cannot leak descriptors — a test proves this. The wakeup ordering (task visible
in the channel before the eventfd counter rises) is lossless.

**Lookup-count ledger.** `lookup_impl` is the single place that pairs a register
with an entry, so every entry costs exactly one FORGET. `READDIRPLUS` pays back
the counts of entries the kernel buffer refuses. `.` and `..` carry node 0 and
owe nothing. Unresolved names travel with node 0 instead of vanishing. This
design closes the classic readdirplus leak that pins descriptors until EMFILE,
and tests on both sides hold the fix in place.

**Path escape prevention.** Lookups are single-component, validated bytes with
no `/`, NUL, `.`, or `..`. Resolution goes through `openat2` with
`RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS` from pinned descriptors. The allowlist
opens first and matches the name read back from `/proc/self/fd/N`, so a symlink
swapped mid-attach exports nothing. The root bind-mount loop gets its own guard.

**Durability policy.** `fsync = "ignore"` masks `O_SYNC`/`O_DSYNC` at open and
short-circuits both sync opcodes through one function, so files and directories
obey one policy. Under `honor`, the real syscalls run through the ring.

**Writeback flag handling.** With writeback negotiated, the server clears
`O_APPEND` and promotes `O_WRONLY` to `O_RDWR`. Without it, neither happens.
Both branches mirror libfuse's passthrough and virtiofsd, and the handshake
carries the flag because only the client knows it.

**Buffer pool.** The pool hands out buffers zero-filled, recycles them
unzeroed, and exposes exactly their initialized prefix. The WRITE length is the
count read off the socket, never the header claim, so recycled tail bytes
cannot reach a file. A test dirties a pooled buffer and proves the tail stays
invisible.

**Node table.** Ids and generations are monotonic and never reused. The
`(st_dev, st_ino)` dedup rests on the O_PATH pin, which keeps the key honest on
local filesystems. The final `close(2)` of a forgotten node runs outside the
table lock, so a batched forget cannot queue a whole connection behind journal
work.

**Wire contract enforcement.** The code checks every claimed length before
reserving memory. Violations that desync the stream are connection-fatal. A
malformed body inside an honest frame answers EINVAL and survives. Integration
tests pin this boundary for every opcode, along with window recycling, ESTALE
after forget, both fsync policies, and E2BIG symmetry on oversize xattrs.

## Strengths worth keeping as precedents

- Raise `max_background` to the negotiated window at init. Most FUSE daemons
  leave fuser's default of 16, and background requests are nearly all bulk I/O.
- Answer protocol-status mismatches in the handshake with distinct typed errors,
  so the CLI prints "not exported" instead of a raw errno.
- Keep the fatal/EINVAL boundary in one place per side, with a test matrix over
  every opcode on both sides of it.
- Document each invariant next to the code that holds it. This codebase's
  comments state contracts, not narrations, and the review used them as claims
  to verify rather than decoration to skip.

## Adjudication, 2026-08-22

An adjudication pass verified every finding against the code at `2a6df84`
before anything landed. It also corrected two errors in this note inline: the
xattr opcode floor (5.19, not 6.5) and the gate commands in the Result section.
Rulings:

| Finding | Ruling | Disposition |
| --- | --- | --- |
| 1. No bound on concurrent sessions | Confirmed | Deferred; revive with the mTLS/authorization milestone or the multi-client fairness item (spec §11), which owns the per-client limits this belongs beside |
| 2. Dead io_uring lane serves EIO until restart | Confirmed | Act now: the process aborts after `abandon`; the unit gains `Restart=on-failure` with a rate bound; the deploy health gate polls restart-aware (the two-line remedy above would have raced the deploy gate as written) |
| 3. Client READ path vs spec §3.1 | Confirmed as a spec overstatement | The v1 plan record shows client-side non-pooling was deliberate; spec §3.1 now says so. Client pooling declined — fuser's `ReplyData` copies the slice at 0.15 through 0.18 alike |
| 4. Pre-epoch timestamps | Confirmed | The outbound half arrives with the fuser 0.18.0 pin (upgrade plan Task 11 now budgets the README sentence); the inbound half waits on a later fuser release |
| 5a. `NodeId`/`Fh` bare aliases | Confirmed | Deferred; serde encodes a newtype as its inner value, so the migration stays wire-compatible and can land on its own — not inside the upgrade plan's mechanical sweep |
| 5b. `write_frame` `debug_assert_eq!` | Confirmed | Act now: a mismatch returns `io::Error`, with a unit test; the two harness escape hatches that send lying frames stay |
| 5c. xattr opcodes "need Linux 6.5" | Refuted on the number | The floor is 5.19; the missing-startup-probe half stands and stays deferred until a server target older than Ubuntu 26.04 exists |

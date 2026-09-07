# Forced-Sync Control Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Status:** Complete on the host. All six tasks ran on 2026-08-28 against
`make check` and `make test-loopback`. Task 7 — the VM pair — stays deliberately
unexecuted: another agent holds 192.168.77.10/.11 for the session-resumption
work, so this branch verifies at the loopback level only, and Task 7 records the
one step a later session must run.

**2026-09-07:** that later session ran Task 7 on the VM pair, and the plan is
now complete end to end. The unforced control lost its file to the power cut,
the forced run kept its file, and the client logged its exit sync — the dated
results subsection under Task 7 carries the evidence.

Two things the execution learned that the plan did not know:

- **`LbfsFuse::destroy` is the wrong home for the driver's sync, and the first
  build proved it by panicking.** `BackgroundSession` has no `Drop` of its own,
  so dropping it releases the session thread's `JoinHandle` — detaching, never
  joining — and then unmounts. `drop(session)` in `main.rs` thus returns while
  that thread is still running, `run()` returns, the tokio `Runtime` drops,
  and `destroy` reaches a runtime mid-shutdown: `A Tokio 1.x context was found,
  but it is being shutdown`, on the exit path, in the shipped binary. `make
  test-loopback` caught it through `loopback_cli.rs`, which runs the real
  binary; the in-process harness missed it because `umount_and_join` joins. The
  sync moved to `force_sync_on_exit` in `main.rs`, after `drop(session)` and
  before the connection closes — where the runtime and the connection are both
  still owned, and where `umount(2)` has already guaranteed every dirty page
  reached the server. Task 4's checkbox covers that home, not the one it
  named.
- **The loopback layer cannot witness an unforced sync, so that case moved
  down.** Task 5 planned a case proving an application's `fsync(2)` still obeys
  the server's policy, using the FIFO the protocol suite uses. Through a mount
  that is impossible twice over: the client implements no `mknod` (`ENOSYS`), and
  even given a FIFO the kernel opens it through its own `fifo_open` rather than
  through FUSE, so the sync never reaches the server. `crates/lbfs-client/tests/
  live.rs` pins the property instead, where the FIFO works. A comment in
  `loopback.rs` records why the case is absent rather than leaving a gap.

Two findings from the reading shaped the plan:

- **The server ignores unknown frame flag bits today.** `read_loop` in
  `crates/lbfs-server/src/rpc/mod.rs` checks `body_len`, the opcode, `data_len`
  and the in-flight window, and reads `hdr.flags` in exactly one place: the
  `FORGET` arm, which logs a debug line when `FLAG_NO_REPLY` is absent and
  proceeds regardless. Bit 1 costs no protocol version — which is what spec §3.1
  reserved it for.
- **A server built before this change cannot say so, which hurts more than it
  looks.** Both ends still answer `2` to the handshake, so a new client meets an
  old server, sets bit 1, and collects a cheerful `STATUS_OK` for a sync that
  never ran. §4 of the design section below closes that with a reply-side
  acknowledgement on the same reserved bit.

**Goal:** Force a real `fsync` on the server even while that server runs
`fsync = "ignore"`, through two entry points — a control xattr on the mount root
for user space, and the client driver's own call at unmount — both riding the
frame flag bit spec §3.1 reserved for them.

**Architecture:** `FLAG_FORCE_SYNC` stops being a reserved constant and becomes a
request flag on `FSYNC`/`FSYNCDIR` and an acknowledgement flag on their replies.
The server threads the frame's flags from `read_loop` through `dispatch` into two
`FileSystem` methods that grow a `force: bool`; `LocalFs::maybe_fsync` reads
`force` as a policy override. The client sends the flag from two places and
nowhere else: `LbfsFuse::setxattr` intercepting one reserved name on the mount
root, and `force_sync_on_exit` in `main.rs` just after the unmount. Nothing
touches the ordinary `fsync(2)` path, so `fsync = "ignore"` still means what it
meant.

**Tech Stack:** Rust (edition 2021), tokio 1, fuser 0.18.0 (ABI 7.40,
exact-pinned), io-uring 0.7, rustix 1, postcard 1.1 + serde/serde_bytes, libc,
tracing, tempfile.

**Spec:** `docs/superpowers/specs/2026-08-20-lbfs-design.md`

## Global Constraints

- Frame header: exactly 24 bytes, little-endian, layout per spec §3.1.
- Protocol magic `LBFS`, version `2`, exact match on both ends. **No task here
  touches the version**, and §1 below argues why the flag needs none.
- Defaults: port `9423`, window `128` (clamp 8..=1024), max body `64 KiB`.
- Status field: `0` OK, `1..=4095` Linux errno, `>= 0xFF00` protocol statuses.
- Frame flag bit 0 is `NO_REPLY`. **Bit 1 belongs to this plan.** A concurrent
  branch designs session resumption (spec §11 fast-follow 1) and stays off bit 1.
- Names, symlink targets, xattr names and values travel as byte strings — never
  `String`.
- The RPC layer reaches storage only through the `FileSystem` trait (spec §5.1).
  `LocalFs` never touches a frame; `rpc::dispatch` never touches a descriptor.
  The flag crosses that boundary as a `bool`, never as a `u16`.
- Every task ends green: `make check` (fmt --check, clippy `-D warnings`, tests)
  passes before every commit. Run `make test-loopback` before calling Task 5
  done.
- TDD: write the failing test first for every behavior.
- No `unsafe` outside `crates/lbfs-server/src/fs/local/uring.rs`.
  `tests/tests/loopback.rs` carries `#![deny(unsafe_code)]` and keeps it.
- Commit after every task with the exact paths staged (no blanket `git add .`).
- **Leave the VM pair alone.** Task 7 records what a later session must run.

---

## Design and Context

Read this whole section before Task 1. It answers five questions the spec leaves
open, each from the code as it stands on this branch's parent.

### 1. Does the server tolerate an unknown flag bit? Yes.

This one question decides whether the feature costs a protocol version, and
`read_loop` (`crates/lbfs-server/src/rpc/mod.rs`) answers it. Every check that
runs before a handler spawns is a length, an opcode or a window:

```rust
if hdr.body_len > MAX_BODY_SIZE { return Err(SessionError::Protocol(...)); }
let op = Opcode::try_from(hdr.op_or_status).map_err(...)?;
if matches!(op, Opcode::Hello | Opcode::Attach) { return Err(...); }
if hdr.data_len > data_limit(op, limits) { return Err(...); }
```

`hdr.flags` appears once in the whole file, inside the `FORGET` arm:

```rust
if hdr.flags & FLAG_NO_REPLY == 0 {
    tracing::debug!("FORGET without NO_REPLY; answering nothing regardless");
}
```

The file holds no mask of known bits and refuses no unknown one. A frame that
sets bit 1 on any opcode reaches its handler exactly as a frame that sets nothing
would. Setting bit 1 against a server built before this change lies inert rather
than killing the connection, and spec §3.1's promise — "we reserve bit 1 now …
so adding it later breaks nothing" — holds as written.

**No protocol version bump.** The version exists to catch a change in what a
peer can *decode*; this change adds no opcode, no body field and no reply shape.
Version 2's own justification makes the contrast: `WriteRequest.kill_suidgid`
needed a bump because postcard ignores trailing bytes, so a version-1 peer would
decode a version-2 body cleanly while dropping the flag. Postcard encodes nothing
here.

### 2. Flag on `FSYNC`/`FSYNCDIR`, not a dedicated opcode

Spec §11 offers both. The flag wins on three counts and loses on none:

- It costs no opcode number, and `Opcode::try_from` kills the connection on an
  unknown opcode — the failure mode §1 just showed the flag avoids.
- The two sync opcodes already carry every field a forced sync needs: the node,
  the handle, and `datasync`. A dedicated opcode would carry the same three and
  differ only in a bit.
- The forced and unforced forms share `LocalFs::maybe_fsync`, which is where
  spec §6 puts the durability policy — in one place. A separate opcode would need
  a second path into that function or a second copy of it.

### 3. What "force" means, precisely

`FLAG_FORCE_SYNC` on a request means: **perform the sync this opcode already
describes, whatever the durability policy says.** The flag overrides the policy
and does nothing else.

- Under `fsync = "honor"` on a file, the flag changes nothing. The unforced call
  already runs the real `fsync`/`fdatasync`, so forcing leaves no observable
  difference.
- Under `fsync = "ignore"` on a file, the flag turns the immediate
  acknowledgement into a real `fsync(2)` or `fdatasync(2)` on that handle's
  descriptor.

**The export root widens the rule, and it must.** A forced `FSYNCDIR` on the
export root runs `syncfs(2)` rather than `fsync(2)`. Both entry points §11 names
— the mount-root control and the unmount call — mean "make the export durable",
and per-inode `fsync` on a directory descriptor cannot deliver that: it flushes
the directory's own metadata and leaves every dirty file page exactly where
`fsync = "ignore"` left it. `syncfs(2)` on any descriptor in the filesystem is
the one syscall that delivers what "before snapshots" asks for, and the export
root is the node whose name already means "the whole export". A forced `FSYNCDIR`
on any *other* directory keeps the narrow reading and runs `fsync`.

Two consequences this plan states rather than leaves to discovery:

- On the export root under `honor`, the flag does change behaviour — it widens
  `fsync(dirfd)` to `syncfs(dirfd)`. A widening never syncs less, so nothing
  breaks, but the "no observable difference under honor" property covers files
  and subdirectories, not the root.
- `syncfs(2)` flushes the whole backing filesystem, not the exported subtree. An
  export sharing a filesystem with other data makes that data pay too. `sync(1)`
  over-reaches the same way and a snapshot wants exactly that reach; the Open
  Risks below carry the cost.

**`O_SYNC` masking stays put.** Spec §6's `ignore` has two halves, and this
control touches one of them. `LocalFs::mask_open_flags` fixes a property of a
descriptor at `OPEN`/`CREATE` time, while the flag rides two per-call opcodes; a
request arriving after the descriptor exists has no way to reopen it, and a
control that changed the policy for the life of a handle would be a different
feature — a runtime policy switch, not a forced sync. Nor does it need to:
`O_SYNC` would have made each write durable as it landed, and the forced sync
makes every write durable at the moment it runs. Same guarantee, deferred to a
point the caller picks.

### 4. The reply carries an acknowledgement, on the same bit

§1 showed that an old server ignores the flag. That helps the connection and
hurts the caller: the handshake still lands on `2`, so a new client cannot
separate a server that forced the sync from one that never heard of forcing, and
both answer `STATUS_OK`. A user-space control that reports success for a sync
that never ran beats no control by nothing at all.

The fix costs one bit and no new field. `writer_task` in the server hardcodes
`flags: 0` on every reply frame today; a server that performed a forced sync
writes `FLAG_FORCE_SYNC` there instead. A server built before this change writes
`0`, which answers the question truthfully. The client keeps the reply's flags —
`conn.rs`'s `Reply` struct drops them today — and the control reports
`EOPNOTSUPP` when the acknowledgement fails to arrive.

Compatible in both directions, by construction. The bit only ever rides a reply
to a request that set it, which an old client never sends; and an old client's
reader builds `Reply { status, body, data }` without reading `hdr.flags` at all.

### 5. The two entry points

**User space: a control xattr on the mount root.** The spec offers "an ioctl or a
control xattr". lbfs implements no `ioctl` — the callback falls through to
fuser's `ENOSYS` default — while xattrs work end to end and already carry
loopback and protocol cases. The xattr wins on cost and on testability.

The name is `user.lbfs.sync`, and the intercept covers **the mount root inode
alone**. Two properties follow, and both matter:

- Shadowing stays confined to one inode. A `setxattr(2)` of `user.lbfs.sync` on
  any other file in the mount travels to the server and lands as an ordinary
  attribute, so a user who wants that name for their own purposes loses it on the
  root directory alone.
- `user.` rather than `trusted.` because `trusted.*` demands `CAP_SYS_ADMIN`, and
  a control the CI workloads this filesystem exists for cannot invoke amounts to
  no control. The kernel's `xattr_permission` allows `user.*` on a directory,
  which the mount root is.

The client intercepts `setxattr` and nothing else. `getxattr`, `listxattr` and
`removexattr` travel to the server, so the name reads back as absent (`ENODATA`)
and never joins a listing — both true, since the mount stores nothing. Any value
triggers one sync; the client reads none of it.

**The driver: `main.rs`, one line after the unmount.** The plan first named
`LbfsFuse::destroy` and the Status note above records the panic that ruled it
out. `force_sync_on_exit` runs immediately after `drop(session)`, which is the
moment this wants for three reasons:

1. `drop(session)` unmounts, and `umount(2)` syncs the superblock — so the kernel
   has already written back every dirty page as ordinary `WRITE` callbacks, and
   the server holds every byte in its page cache. Under `ignore` that is exactly
   the data a crash would lose.
2. The unmount has taken the mount away, so no FUSE callback can still wait on
   this thread.
3. `run()` owns both the tokio `Runtime` and the `Connection` at this line, which
   `destroy` on a detached session thread cannot promise.

`run()`'s own thread is no tokio worker, so `Runtime::block_on` is legal here. A
timeout bounds the call and the log carries its failure rather than raising one:
an exit that refused to happen would be the worse trade.

One consequence worth naming: the driver-initiated sync belongs to the shipped
binary, not to `LbfsFuse`. An embedder — `tests/tests/loopback.rs` included —
mounts the bridge without it and calls the control itself if it wants one.

**The ordinary `fsync(2)` path never sets the flag.** Spec §11 names two entry
points and neither one is an application's own `fsync`. Forcing those would drain
`fsync = "ignore"` of all meaning, which is the option the config exists to
offer.

### 6. Proving a real `fsync` happened

The protocol suite already owns the witness, in `tests/tests/protocol.rs`:

```rust
/// It is the one file type whose `fsync(2)` fails, which makes it the only
/// witness to whether the durability policy ran the syscall at all.
fn make_fifo(path: &Path) { ... }
```

`fsync(2)` on a FIFO answers `EINVAL`. Under `ignore` an unforced `FSYNC` on a
FIFO answers `STATUS_OK`, because no syscall runs. A **forced** `FSYNC` on that
same FIFO under that same policy must answer `EINVAL` — and an errno the kernel
produces only when the syscall runs proves directly that the syscall ran. This
beats a counter and needs no test-only field.

What stays unproven, plainly: the `EINVAL` proves `fsync(2)` ran, and the
`STATUS_OK` proves it returned success. Neither proves the bytes reached the
platter. No userspace test can — that needs a crash or a block-layer trace, which
means VM work (Task 7).

The `syncfs` path on the export root has no such witness, since `syncfs(2)`
succeeds on any descriptor. The reply acknowledgement covers it instead — the
server writes that bit only on the branch that performed the sync — and the
loopback case asserts it across a real socket.

## File Map

| File | Change |
|---|---|
| `crates/lbfs-proto/src/frame.rs` | `FLAG_FORCE_SYNC_RESERVED` → `FLAG_FORCE_SYNC`, live on both request and reply |
| `crates/lbfs-server/src/rpc/mod.rs` | `read_loop` passes `hdr.flags` to `dispatch`; `OutFrame` and `writer_task` carry reply flags |
| `crates/lbfs-server/src/rpc/dispatch.rs` | `Reply` becomes a struct with `flags`; `dispatch` takes the frame flags and acknowledges a forced sync |
| `crates/lbfs-server/src/fs/mod.rs` | `fsync` and `fsyncdir` grow `force: bool` |
| `crates/lbfs-server/src/fs/local/mod.rs` | `maybe_fsync` takes `force`; `fsyncdir` runs `syncfs` on a forced export root |
| `crates/lbfs-client/src/conn.rs` | `Reply` keeps the frame flags; `call_raw` takes request flags and returns reply flags; `force_sync_export` |
| `crates/lbfs-client/src/fuse.rs` | `CONTROL_XATTR_SYNC` and the `setxattr` intercept |
| `crates/lbfs-client/src/main.rs` | `force_sync_on_exit`, run after `drop(session)` |
| `tests/src/lib.rs` | `TestClient::call_flagged`, and a `Reply` that keeps the reply's flags |
| `tests/tests/protocol.rs` | Forced-sync wire cases, including the FIFO witness |
| `tests/tests/loopback.rs` | The control xattr from user space, under both policies |
| `crates/lbfs-client/tests/loopback_cli.rs` | The shipped binary's exit sync, read out of its own log |
| `docs/superpowers/specs/2026-08-20-lbfs-design.md` | §3.1, §6, §11 |
| `README.md` | The control xattr, beside the durability policy it overrides |

---

## Task 1: Proto — the flag goes live

- [x] Rename `FLAG_FORCE_SYNC_RESERVED` to `FLAG_FORCE_SYNC` in
      `crates/lbfs-proto/src/frame.rs` and replace the "Never set in product v1"
      note with what the bit means on a request and on a reply.
- [x] Add a unit case pinning `FLAG_NO_REPLY` and `FLAG_FORCE_SYNC` as bits 0 and
      1 and asserting they never overlap — one bit now carries two protocols'
      worth of meaning, and a later flag must not land on top of it.
- [x] `make check`.
- [x] Commit: `feat(proto): make frame flag bit 1 the live FORCE_SYNC flag`.

## Task 2: Server — honour the flag

- [x] Failing test first, in `crates/lbfs-server/src/fs/local/mod.rs`'s test
      module: under `FsyncPolicy::Ignore`, a forced `fsync` on a FIFO handle
      answers `EINVAL` while an unforced one answers `Ok`.
- [x] Add `force: bool` to `FileSystem::fsync` and `FileSystem::fsyncdir` in
      `crates/lbfs-server/src/fs/mod.rs`, documented as the policy override.
- [x] `LocalFs::maybe_fsync(&self, fd, datasync, force)`: run the real sync when
      `force || self.fsync_policy == FsyncPolicy::Honor`.
- [x] `LocalFs::fsyncdir`: on `force` against `ROOT_NODE`, run
      `rustix::fs::syncfs` on a blocking thread — io_uring carries no opcode for
      it, and `statfs` next door already set that precedent. Otherwise
      `maybe_fsync`.
- [x] Failing test: under `Ignore`, a forced `fsyncdir` on `ROOT_NODE` succeeds,
      and so does an unforced one. Task 3's acknowledgement pins which branch
      ran, since `syncfs` cannot fail informatively.
- [x] `make check`.
- [x] Commit: `feat(server): honour FORCE_SYNC over the durability policy`.

## Task 3: Server — thread the flag and acknowledge it

- [x] Failing test first, in `tests/tests/protocol.rs`: under `Ignore`, a forced
      `FSYNC` frame on a FIFO answers `EINVAL`, and the same frame unforced
      answers OK. This needs `TestClient::call_flagged` and a `Reply` that keeps
      the reply frame's flags, so add both to `tests/src/lib.rs` first.
- [x] Turn `dispatch::Reply` from a 3-tuple into a struct with `status`, `flags`,
      `body`, `data`, and update the five constructors and every direct
      construction.
- [x] `dispatch` takes the frame's `flags`; the `Fsync` and `Fsyncdir` arms read
      `FLAG_FORCE_SYNC` out of it, pass `force` to the trait, and write
      `FLAG_FORCE_SYNC` onto a reply whose forced sync succeeded.
- [x] `read_loop` passes `hdr.flags` into the spawned `dispatch`; `OutFrame`
      carries `flags`; `writer_task` puts them on the wire instead of `0`.
- [x] Failing test: the acknowledgement rides a forced `FSYNC`/`FSYNCDIR` reply
      under both policies, and stays off an unforced one.
- [x] Failing test: `FLAG_FORCE_SYNC` on `GETATTR` lies inert — answered
      normally, unacknowledged, connection alive. §1 rests on this property, so
      the suite asserts it rather than assuming it.
- [x] `make check`.
- [x] Commit: `feat(server): carry FORCE_SYNC through dispatch and acknowledge it`.

## Task 4: Client — send the flag from two places

- [x] Failing test first, in `crates/lbfs-client/tests/live.rs`: against a server
      on `FsyncPolicy::Ignore`, `Connection::force_sync_export` succeeds, while a
      plain `fsync` on a FIFO handle still answers `Ok` — which proves the
      ordinary path kept its behaviour.
- [x] `conn.rs`: `Reply` gains `flags`, filled from the frame header in
      `reader_task`. `call_raw` takes a request `flags: u16` and returns the
      reply's; `call` and `call_unit` pass `0` and drop the answer.
- [x] `conn.rs`: `force_sync_export()` — `OPENDIR(ROOT_NODE)`, forced
      `FSYNCDIR`, `RELEASEDIR`; `EOPNOTSUPP` when the reply carries no
      acknowledgement; the releasedir runs whatever the sync answered.
- [x] `fuse.rs`: `CONTROL_XATTR_SYNC = b"user.lbfs.sync"`, and a `setxattr`
      intercept that fires for that name on `FUSE_ROOT_ID` and nowhere else.
- [x] `main.rs`: `force_sync_on_exit` runs the forced sync on the owned runtime
      under a timeout, logs the outcome, and never fails the exit. **Not**
      `fuse.rs::destroy` — see the Status note for the panic that ruled it out.
- [x] `make check`.
- [x] Commit: `feat(client): force a real sync from the mount root and at unmount`.

## Task 5: Loopback — the control from user space

- [x] Failing test first: under `Opts { fsync: FsyncPolicy::Ignore, .. }`,
      `setxattr(mnt, "user.lbfs.sync", b"")` succeeds, and `lb.conn()`'s own
      `force_sync_export` collects the acknowledgement — which proves the server
      took the honour branch, across a real socket.
- [x] Failing test: the same name on a *file* in the mount travels, stores and
      reads back, and the name never joins the mount root's `listxattr` — the
      shadowing bound from §5.
- [x] Failing test: the control also succeeds under `FsyncPolicy::Honor`.
- [x] Failing test, in `crates/lbfs-client/tests/loopback_cli.rs`: the shipped
      binary logs the forced sync on its way out, and none of its three failure
      lines. The log is the only witness — `syncfs` leaves nothing to stat for.
- [x] `make test-loopback`.
- [x] Commit: `test(loopback): force a sync through the mount root control xattr`.

## Task 6: Spec and README

- [x] §3.1: bit 1 is `FORCE_SYNC`, live, on requests and on replies.
- [x] §6: the control exists, what it does to `O_SYNC` masking (nothing), and the
      export-root `syncfs` widening.
- [x] §11 fast-follow 2: struck, with one line naming what stayed out.
- [x] README: the control xattr, beside the durability policy it overrides.
- [x] `make check`.
- [x] Commit: `docs(spec): the forced-sync control exists; strike fast-follow 2`.

## Task 7: VM verification — NOT RUN ON THIS BRANCH

Another agent holds 192.168.77.10/.11 for the session-resumption work, so this
branch stops at the loopback level. A later session runs, in order:

- [x] `make build-guest && make vm-deploy`.
- [x] Server on `fsync = "ignore"`, mount from the client guest, write a file,
      then `setfattr -n user.lbfs.sync -v 1 /mnt/lbfs` and confirm exit 0.
- [x] The one step no host test reaches: write, force the sync, then cut the
      server guest's power (`virsh destroy`, never a clean shutdown) and confirm
      the bytes survive the reboot. Run the same shape without the forced sync as
      the control — that run may lose them, and a control that never loses them
      means the export's filesystem flushed on its own and the case proves
      nothing either way.
- [x] Unmount the client and confirm the server logs the driver-initiated sync.
- [x] Record the result at the top of this plan.

### Results — 2026-09-07, the power-cut run

A later session ran this task against a fresh `make build-guest && make
vm-deploy` of current main (`f07da9c`), with the guest's `/etc/lbfs.toml`
flipped to `fsync = "ignore"` for the drill and back to `"honor"` afterwards.
Each scenario mounted fresh, wrote a distinctive end-of-file marker through the
mount, and ran `dd conv=fsync` so the application's own `fsync(2)` — which the
policy ignores — pushed every byte into the server's page cache before the cut.

- **Control (unforced): the bytes died.** The 33-byte marker file reached the
  server (content readable over ssh, `Dirty: 984 kB`, `Writeback: 0 kB` in
  `/proc/meminfo`), and `virsh destroy` roughly two seconds later erased it:
  after reboot and journal replay the file did not exist at all. The control
  earned its keep on the first attempt — no self-flush beat the cut, so an
  application `fsync` under `ignore` buys nothing across a power loss.
- **Forced: the bytes survived.** Same shape, fresh marker, then
  `setfattr -n user.lbfs.sync -v 1 /mnt/lbfs` — exit 0, so the reply carried
  the acknowledgement. `virsh destroy` came right after, and the reboot found
  the file whole: size 32, marker still the last line. The platter now backs
  what Task 3 could only prove at the syscall level.
- **Exit sync: the driver reported it.** A clean `fusermount3 -u` let the
  client exit on its own, and its log holds the witness:
  `INFO lbfs_client: forced a sync of the export before exit`.

One correction to this task's wording: the unmount step says "confirm the
server logs the driver-initiated sync", but the server logs nothing on that
path — the client's exit line is the only witness, exactly as the Task 5
loopback case already documented. The drill read the client's log instead.

The run left the pair healthy: `fsync = "honor"` restored and the service
active, the export empty, no stray client process or mount, and a final
mount–write–read–unmount smoke check green.

---

## Acceptance Criteria

1. `FLAG_FORCE_SYNC` is bit 1, set on `FSYNC`/`FSYNCDIR` requests and echoed on
   the replies of syncs the server actually performed.
2. The protocol version stays `2`. No opcode, body field or reply shape changes.
3. Under `fsync = "ignore"`, a forced `FSYNC` on a FIFO answers `EINVAL` and an
   unforced one answers OK — proving the syscall ran.
4. `FLAG_FORCE_SYNC` on an opcode that syncs nothing lies inert: answered
   normally, unacknowledged, connection alive.
5. A `setxattr` of `user.lbfs.sync` on the mount root forces a sync; the same
   name on any other file behaves as an ordinary attribute; the name never joins
   the root's `listxattr`.
6. Unmount forces a sync of the export before the connection closes, and a
   failure there logs rather than hanging or failing the unmount.
7. An application's own `fsync(2)` still obeys the server's policy.
8. `make check` and `make test-loopback` pass.

## Open Risks

- **A mixed-build deployment degrades quietly on the server side.** Both ends
  answer `2` to the handshake, so a new client will attach to a server built
  before this change; the reply acknowledgement turns that into an `EOPNOTSUPP`
  the caller can read, but only along the client-driven paths. Nothing compels an
  operator to deploy both halves, and the next real version bump is what finally
  makes the mismatch impossible.
- **`syncfs(2)` reaches past the export.** A forced sync on the export root
  flushes the whole backing filesystem, so an export sharing a disk with other
  data makes that data pay. An unprivileged user on the mount can invoke it
  repeatedly, which amplifies cost on a busy server. That same user could already
  provoke real syncs under `fsync = "honor"`, so this widens an existing surface
  rather than opening a new one — but it does widen it.
- **The unmount sync runs unconditionally and costs time.** Every unmount now
  spends one `syncfs` on the export, under both policies. A timeout bounds it and
  its failure changes nothing, but a heavily dirtied export will make an unmount
  visibly slower than before.
- **`user.lbfs.sync` on the mount root no longer stores.** This plan says so, and
  the loss covers that one inode, but it takes a name out of the user's
  namespace.
- **Durability past the syscall stays unproven on this branch.** Task 3 proves
  `fsync(2)` and `syncfs(2)` ran and returned success. Whether the bytes reached
  the platter needs the power-cut case in Task 7, which this session cannot run.

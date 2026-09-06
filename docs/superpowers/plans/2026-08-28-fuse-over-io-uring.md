# FUSE over io_uring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Status:** Not started. Task 1 is a gate, not a formality — it decides whether
the rest of this plan runs at all, and the honest prior is that it might not.
Two performance campaigns in this repository already ended in recorded negative
results (`docs/benchmarks/2026-08-22-big-requests.md`, and the `--fuse-threads`
ladder in the bottleneck analysis), both after the reasoning looked sound. This
one carries far more implementation cost than either, most of it outside this
repository, so it earns its budget by measuring the prize first.

**Goal:** Move the client's FUSE transport off `read`/`write` on `/dev/fuse` and
onto the kernel's io_uring command interface, so a request and its reply cross
the kernel boundary through a shared ring rather than through a syscall pair per
operation. The target is the small-request latency the mount cannot currently
reach: 4 KiB shapes, where per-request cost is the whole cost.

**Architecture:** Three layers, and only the middle one is large. The kernel
already implements the ring (`FUSE_OVER_IO_URING`, ABI 7.42). fuser 0.18.0 can
neither ask for it nor speak it, so the work is a fuser fork or upstream
contribution: raise the declared ABI floor past 7.42, then add a ring session
backend beside the existing `/dev/fuse` loop. lbfs itself changes least — one
capability in `capabilities()` behind a CLI flag, defaulting off until the
benchmark says otherwise.

**Tech Stack:** Rust (edition 2021), tokio 1, fuser (forked from 0.18.0),
io-uring 0.7.14 (already a workspace dependency, server side), libc; Linux 7.0
guests under libvirt; fio 3.41 for the acceptance run.

**Spec:** `docs/superpowers/specs/2026-08-20-lbfs-design.md` §11 names this item
("FUSE-over-io_uring via a fuser fork") beside the kernel-module client this
should undercut.

## Global Constraints

- **No change to the wire protocol.** This plan touches the client's kernel
  boundary only. Protocol version stays `2`, no opcode changes, no field
  changes, and `git diff` must not touch `crates/lbfs-proto/` or
  `crates/lbfs-server/`.
- **The `/dev/fuse` path stays.** The ring is an alternative backend chosen at
  mount time, never a replacement. A kernel without `FUSE_OVER_IO_URING`, or a
  mount that did not ask for it, keeps today's behaviour byte for byte.
- **Default off until measured.** The CLI flag ships defaulting to the existing
  transport. Task 8 decides whether that default flips, and may
  decide "no".
- Frame header, magic, version, window and status conventions per spec §3.1 —
  untouched by this plan, listed so a reader does not go looking.
- Every task ends green: `make check` (fmt --check, clippy `-D warnings`, tests)
  passes before every commit. Run `cargo fmt --all` first.
- TDD: write the failing test first for every behaviour a test can reach
  without a mount.
- No `unsafe` outside `crates/lbfs-server/src/fs/local/uring.rs` **in this
  repository**. The fork will need `unsafe` for the ring; that is the fork's
  code under the fork's rules, and none of it lands in `crates/`.
- Commit after every task with the exact paths staged (no blanket `git add .`).

---

## Design and Context

Read this whole section before Task 1.

**A note on sources.** Every uapi citation below cites
`/usr/include/linux/fuse.h` on the development host (FUSE ABI 7.45; the
`kernel-devel` copy at `/usr/src/kernels/7.0.12-101.fc43.x86_64/include/uapi/
linux/fuse.h` agrees). **The kernel's *implementation* sources are not
available on this machine** — `/usr/src/kernels/*/fs/fuse/` carries `Kconfig`
and `Makefile` and no `.c` files. This section cites the ABI precisely and
describes the protocol shape from it, and Task 2 exists specifically to read
`fs/fuse/dev_uring.c` before anyone writes ring code. Do not read anything below
as a line-cited claim about kernel behaviour the way the parallel-direct-
writes plan cites `fs/fuse/file.c`; that plan had the sources and this one does
not yet.

### 1. What the kernel offers

`FUSE_OVER_IO_URING` is `1ULL << 41`, an `INIT` capability
(`/usr/include/linux/fuse.h:492`), introduced in ABI **7.42** along with every
structure it needs (`:221-231`). The guest kernels carry the implementation:
`grep -c fuse_uring /proc/kallsyms` on the client guest returns 64, including
`fuse_uring_cmd`, so the kernel carries the code rather than merely declaring the flag.

The interface is an `io_uring` command channel, not a data ring. Two commands
exist (`:1283-1291`):

```c
enum fuse_uring_cmd {
	FUSE_IO_URING_CMD_INVALID = 0,
	/* register the request buffer and fetch a fuse request */
	FUSE_IO_URING_CMD_REGISTER = 1,
	/* commit fuse request result and fetch next request */
	FUSE_IO_URING_CMD_COMMIT_AND_FETCH = 2,
};
```

The shape that falls out of those two names is a **fetch/commit loop with no
idle syscall**: the server registers a set of buffers, each registration also
asking for a request; when a request arrives the completion carries it; the
server answers by submitting `COMMIT_AND_FETCH`, which both delivers the reply
and re-arms that entry for the next request. One SQE per operation, and no
`read` to go find work.

The SQE's 80-byte command area carries (`:1294-1305`):

```c
struct fuse_uring_cmd_req {
	uint64_t flags;
	uint64_t commit_id;   /* entry identifier for commits */
	uint16_t qid;         /* queue the command is for (queue index) */
	uint8_t  padding[6];
};
```

`qid` says the queues are plural and indexed — the upstream design is one queue
per CPU. `commit_id` is how a reply names the request it answers, which is the
same job the `unique` field does in a `/dev/fuse` header.

Each registered entry points at a header block (`:1268-1278`):

```c
struct fuse_uring_req_header {
	char in_out[FUSE_URING_IN_OUT_HEADER_SZ];   /* 128 */
	char op_in[FUSE_URING_OP_IN_OUT_SZ];        /* 128 */
	struct fuse_uring_ent_in_out ring_ent_in_out;
};
```

`in_out` holds a `struct fuse_in_header` on the way in and a
`struct fuse_out_header` on the way out; `op_in` holds the per-opcode header;
and `ring_ent_in_out` (`:1251-1265`) carries `flags`, `commit_id` and
`payload_sz`. **The bulk payload lives in a separate buffer**, which is the
detail that matters most to lbfs: a `WRITE`'s data and a `READ`'s reply data do
not travel in these 256 bytes, so the ring does not by itself remove the payload
copy at the centre of this filesystem's hot path.

### 2. Why fuser 0.18.0 cannot reach it, in two independent ways

**It does not advertise a new enough ABI.** `FUSE_KERNEL_MINOR_VERSION` is
`40` on Linux (`fuser-0.18.0/src/ll/fuse_abi.rs:36-44`), and `INIT` sends that
value verbatim (`src/ll/request.rs:1005`, `:1039`). `FUSE_OVER_IO_URING`
arrived at 7.42. A mount that says 40 is not one the kernel will offer the ring
to, whatever flags it sets — so the capability bit is not the first blocker,
the version is.

**It cannot speak the transport.** `InitFlags` names the bit —
`FUSE_OVER_IO_URING = 1 << 41` (`src/ll/flags/init_flags.rs:91-92`) — which is
what makes it look closer than it stands. `src/session.rs` reads requests with
`read` on the `/dev/fuse` descriptor and writes replies back; grep the crate for
`uring` and every hit outside `init_flags.rs` is the substring inside the word
"during". Naming a capability is not speaking a protocol.

**What we already have.** `io-uring 0.7.14` is a workspace dependency today
(the server's `LocalFs` uses it) and provides `opcode::UringCmd80`
(`io-uring-0.7.14/src/opcode.rs:1703`), which is exactly the SQE shape
`fuse_uring_cmd_req` needs. The fork needs no new dependency to submit these
commands.

### 3. What the prize might be, and why Task 1 gates everything

**The optimistic case.** The NFS comparison prices the userspace round trip at
roughly 60 µs per 4 KiB write, and kernel NFS answers a complete 4 KiB write in
~104-111 µs where lbfs's own RPC layer — no FUSE in the path at all — measured
146 µs. A transport that removes syscalls and context switches from the request
path attacks precisely the fixed per-request cost that dominates every 4 KiB
shape this filesystem runs.

**The pessimistic case, which has the better evidence.** Three facts argue
for restraint:

- **Per-request cost is small everywhere measured so far.**
  `2026-08-22-big-requests.md` found cost per megabyte flat across request
  sizes, meaning the fixed per-request part is small beside the per-byte part.
  That measurement is about *streaming*, so it does not settle the 4 KiB case —
  but it measures directly the quantity this plan proposes to reduce, and it
  came out small.
- **More kernel-side parallelism bought nothing on these guests.** The
  `--fuse-threads` ladder found no win from a second `/dev/fuse` reader, and the
  parallel-direct-writes campaign re-confirmed it (36.5k against 37.5k IOPS).
  Two vCPUs, one already carrying tokio workers. FUSE-over-io_uring's upstream
  case rests on core scaling and context-switch avoidance on many-core
  machines; this pair is the opposite shape.
- **The payload copy survives.** Per §1, bulk data does not ride in the ring
  header. Whatever the ring saves, it does not save the copy.

**Task 1 measures the ceiling before anyone builds anything.** The question
it answers is narrow and answerable: *how much of a 4 KiB operation's latency sits
between the application's syscall and lbfs's own RPC call?* That difference is
the entire budget this plan could ever recover, and if it comes back small,
the plan
stops there and joins its predecessors as a recorded negative result.

**One correction Task 1 must make first.** The FUSE-cost table in the
bottleneck analysis subtracts a raw-RPC column measured on 2026-08-22, before
the kill-priv change, the fuser 0.18 upgrade and the window-permit fix. It now
claims a 4 KiB write costs 146 µs through the RPC layer while the *mount*
measures ~135 µs, which is impossible — a mount cannot beat the transport
underneath it. Re-measure those baselines before sizing this work
from them.

### 4. Fork, vendor, or upstream

`Cargo.toml` pins `fuser = { version = "=0.18.0" }` exactly, with a comment
explaining that post-0.18.0 releases come from a coding agent and want human
review before adoption. That pin is the context for this choice:

- **A `[patch.crates-io]` git fork** keeps the dependency legible and the diff
  reviewable, and is the cheapest thing to undo. Preferred.
- **Vendoring** the crate buys nothing here and loses upstream's fixes.
- **Upstreaming** is the right end state and the wrong starting point: a
  transport backend is a large PR, and this repository should know whether the
  thing is worth having before asking a maintainer to carry it. Task 9 decides
  whether to offer it.

---

## File Map

| Path | Change |
|---|---|
| *(fork)* `fuser/src/ll/fuse_abi.rs` | ABI minor 40 → 42, with the 7.41/7.42 deltas that implies |
| *(fork)* `fuser/src/session.rs` | a ring session backend beside the `/dev/fuse` loop |
| *(fork)* `fuser/src/ring.rs` *(new)* | queue registration, entry pool, fetch/commit loop |
| `Cargo.toml` | `[patch.crates-io]` entry pointing at the fork, for as long as a fork exists |
| `crates/lbfs-client/src/fuse.rs` | one `Capability` entry, behind the new flag |
| `crates/lbfs-client/src/main.rs` | `--fuse-io-uring` flag, default off |
| `docs/benchmarks/2026-08-28-fuse-over-io-uring.md` *(new)* | the result, positive or negative |
| `docs/superpowers/specs/2026-08-20-lbfs-design.md` | §7 records the transport choice; §11 loses the item if it lands |

---

### Task 1: Measure the ceiling before building anything

**This task is a gate.** If the recoverable budget is small, the plan stops and
Task 9's negative-result record is the whole deliverable.

**Files:**
- Edit: `Makefile` (deploy `lbfs-bench` to the guest alongside the binaries)
- Create: `docs/benchmarks/2026-08-28-fuse-over-io-uring.md` (opened with the
  baseline; the finding lands in Task 8 or Task 9)

**Interfaces:**
- Consumes: nothing.
- Produces: a current, same-day measurement of the FUSE tax on 4 KiB shapes,
  and a go/no-go ruling written down.

- [ ] **Step 1: Re-measure the raw RPC baseline, because the recorded one is stale**

`make build-guest` builds `lbfs-server` and `lbfs-client` only, so `lbfs-bench`
never reaches the guest. Add it, deploy it, and run the 4 KiB shapes against the
same export the mount uses. The recorded 146 µs write / 92 µs read predates
kill-priv, fuser 0.18 and the window-permit fix.

- [ ] **Step 2: Measure the same shapes through the mount, same day, interleaved**

4 KiB random read and random write, QD1 psync, plus QD16 libaio. Alternate
mount and raw-RPC measurements rather than running one after the other, and
report medians of at least three — the campaign method every benchmark in this
repository now uses, for the reason `2026-08-22-big-requests.md` and the
parallel-direct-writes plan both record.

- [ ] **Step 3: Split the difference into its parts**

The gap between the two columns is FUSE plus the bridge. Attribute it with
`bpftrace` on the client: time from the kernel's `fuse_simple_request` (or the
nearest available symbol — check `/proc/kallsyms`, since this kernel inlines
a good many `fs/fuse` functions) to the bridge's `conn.call`,
and back. What this plan could recover is the syscall-and-scheduling part of
that, not the whole gap.

- [ ] **Step 4: Rule**

Write the ruling into the benchmark document with its numbers.

**Proceed** only if the syscall-and-scheduling share of a 4 KiB operation is
large enough to matter — as a starting bar, **above 25 µs on a ~135 µs
operation**, which is roughly 20% and enough to close a meaningful part of the
gap to kernel NFS. Below that, stop: record the measurement, skip to Task 9,
and leave spec §11's item in place with the finding attached.

- [ ] **Step 5: Commit**

```bash
git add Makefile docs/benchmarks/2026-08-28-fuse-over-io-uring.md
git commit -m "bench: price the FUSE transport tax on small requests"
```

---

### Task 2: Read the kernel implementation and write down the protocol

**Files:**
- Edit: `docs/benchmarks/2026-08-28-fuse-over-io-uring.md` (a protocol section)

**Interfaces:**
- Consumes: Task 1's go ruling.
- Produces: a written, cited description of the registration and fetch/commit
  sequence, precise enough that Task 4 transcribes rather than invents.

- [ ] **Step 1: Get the sources**

`fs/fuse/dev_uring.c` and `fs/fuse/dev_uring_i.h` are not on this machine —
`/usr/src/kernels/*/fs/fuse/` has no `.c` files. Fetch the tree matching the
guests (`uname -r` reports 7.0.0-28-generic) rather than reading a summary of
it. Every claim in the rest of this plan that concerns kernel behaviour, as
opposed to the ABI, stays unverified until this step lands.

- [ ] **Step 2: Answer these questions in writing, each with a file and line**

- How many queues does the kernel expect, and does it require one per CPU or
  merely index them? What does it do with a `qid` that never registers?
- What exactly must a `FUSE_IO_URING_CMD_REGISTER` SQE carry, and how does the
  kernel learn the address and size of the payload buffer?
- Is registration per queue, per entry, or both? How many entries may a queue
  hold, and what happens when they are all in flight?
- How does `commit_id` get assigned — kernel-chosen and echoed, or
  server-chosen?
- How does the mount fall back if registration fails halfway?
- What ends the ring at unmount, and what must the server do to drain it
  without losing a reply? (`crates/lbfs-client/src/main.rs` treats
  `drop(session)` as "unmount, drain, exit"; the loopback suite has a case that
  pins it.)
- Where does the payload buffer come from, and can it be an io_uring registered
  buffer — the thing that would remove a copy rather than a syscall?

- [ ] **Step 3: Commit**

```bash
git add docs/benchmarks/2026-08-28-fuse-over-io-uring.md
git commit -m "docs(bench): the FUSE io_uring registration and commit protocol"
```

---

### Task 3: Fork fuser and raise the ABI floor

**Files:**
- *(fork)* `src/ll/fuse_abi.rs`, plus whatever 7.41/7.42 introduces
- Edit: `Cargo.toml` (`[patch.crates-io]`)

**Interfaces:**
- Consumes: Task 2's notes.
- Produces: a fork that mounts and passes the existing suites at ABI 7.42, with
  no ring code yet. This step is separately valuable and separately revertible:
  if it destabilises anything, that is worth knowing before ring code hides it.

- [ ] **Step 1: Fork at the `=0.18.0` tag**, so the diff from the pinned
  release is the whole review surface.

- [ ] **Step 2: Raise `FUSE_KERNEL_MINOR_VERSION` to 42 on Linux**, leaving the
  macOS branch alone (`fuse_abi.rs:36-44`).

- [ ] **Step 3: Handle what 7.41 and 7.42 add.** 7.41 adds `FUSE_ALLOW_IDMAP`
  (`/usr/include/linux/fuse.h:222`). Claiming a version means honouring
  everything it implies, including any struct that grew a field.

- [ ] **Step 4: Point the workspace at the fork and run everything**

`make check`, `make test-loopback`, `make vm-test`. A version bump with no
behaviour change should move no number and break no case.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "deps(fuser): fork at 0.18.0 and declare FUSE ABI 7.42"
```

---

### Task 4: Build the ring transport in the fork

The large task. It lives entirely in the fork; nothing here lands under
`crates/`.

- [ ] **Step 1: Entry and queue types** — a registered entry owning its
  `fuse_uring_req_header` and its payload buffer, and a queue owning its
  entries and its `qid`.
- [ ] **Step 2: Registration** — submit `FUSE_IO_URING_CMD_REGISTER` per entry
  via `opcode::UringCmd80`, and fail the mount cleanly if the kernel refuses.
- [ ] **Step 3: The fetch/commit loop** — take a completion, decode the
  `fuse_in_header` out of `in_out` and the op header out of `op_in`, dispatch
  it through the same `Filesystem` trait the `/dev/fuse` loop uses, then answer
  with `COMMIT_AND_FETCH` carrying the `commit_id`.
- [ ] **Step 4: Payload handling** — `WRITE` data in and `READ` data out, using
  whatever Task 2 Step 2 established about the payload buffer.
- [ ] **Step 5: Teardown** — unmount must still drain. The loopback case that
  pins `drop(session)` semantics is the specification.
- [ ] **Step 6: Fall back** — a kernel that does not grant the capability, or a
  registration that fails, returns the session to the `/dev/fuse` loop rather
  than failing the mount.
- [ ] **Step 7: Commit in the fork**, and pin the workspace at that revision.

---

### Task 5: Negotiate the capability from lbfs, behind a flag

**Files:**
- Edit: `crates/lbfs-client/src/fuse.rs` (`capabilities()`)
- Edit: `crates/lbfs-client/src/main.rs` (the flag)

**Interfaces:**
- Consumes: Task 4's fork.
- Produces: `--fuse-io-uring`, default off; one optional `Capability`.

- [ ] **Step 1: Write the failing tests.** `capabilities()` is already unit
  tested (`only_the_promised_capability_is_required` and its neighbours). Pin
  that the new bit appears only when the flag says so, and that it stays
  **optional** rather than required — a kernel that refuses it must leave a
  working mount, exactly as `FUSE_HANDLE_KILLPRIV_V2` does.
- [ ] **Step 2: Add the flag**, defaulting off, documented as experimental.
- [ ] **Step 3: Add the capability**, behind the flag.
- [ ] **Step 4: `make check`.**
- [ ] **Step 5: Commit**

```bash
git add crates/lbfs-client/src/fuse.rs crates/lbfs-client/src/main.rs
git commit -m "feat(client): optional FUSE_OVER_IO_URING transport behind a flag"
```

---

### Task 6: Prove correctness before measuring speed

**Files:**
- Edit: `tests/tests/loopback.rs`
- Edit: `vm/test.sh` or `vm/tests/` as needed

- [ ] **Step 1: Run the whole loopback suite with the flag on.** Every existing
  case must pass over the ring transport unchanged — that is the point of a
  transport. The suite is the specification of correct behaviour and it does
  not get relaxed for a new backend.
- [ ] **Step 2: Add the transport dimension where it costs little.** Follow the
  file's existing `(writeback: bool)` shape rather than duplicating cases.
- [ ] **Step 3: `make vm-test` with the flag on**, including the crc32c verify
  job and the disconnect drill.
- [ ] **Step 4: Commit.**

---

### Task 7: Benchmark, interleaved

- [ ] **Step 1: Interleaved A/B**, ring against `/dev/fuse`, alternating round
  by round, medians of at least three, server drained before every job. Not one
  pass then the other — that method produced a phantom 16 × regression on this
  pair once already.
- [ ] **Step 2: The shapes that matter.** 4 KiB random read and write at QD1 and
  QD16, which is where Task 1 said the budget lives. Streaming shapes as
  no-regression checks only.
- [ ] **Step 3: Confirm the kernel actually took the ring** before believing any
  number — a capability the kernel declined leaves a mount that works perfectly
  and measures nothing. Check the `INIT` reply flags, and count `fuse_uring_*`
  entries with `bpftrace` while a job runs.
- [ ] **Step 4: Record and commit.**

---

### Task 8: Decide the default

- [ ] **Step 1: Rule on the flag.** Flip the default only if the ring wins the
  4 KiB shapes and loses nothing else. A wash keeps the flag off, and that is a
  legitimate outcome rather than a failure to argue away.
- [ ] **Step 2: Update spec §7** with whichever transport the mount uses by
  default and why.
- [ ] **Step 3: Commit.**

---

### Task 9: Record the result and settle the fork's future

Runs whether the answer was yes or no.

- [ ] **Step 1: Finish the benchmark document.** A negative result gets the same
  care as a positive one; `2026-08-22-big-requests.md` is the template.
- [ ] **Step 2: Rule on the fork.** Landed and winning → offer it upstream, and
  say so in §11. Landed and neutral → keep the fork behind the flag, or drop it
  and record why. Never started → the fork question never arose.
- [ ] **Step 3: Update spec §11**, striking the item if it landed, annotating it
  with the measurement if it did not.
- [ ] **Step 4: Commit.**

---

## Acceptance Criteria

1. `make check` passes.
2. `make test-loopback` passes with the flag **off**, unchanged from today.
3. `make test-loopback` and `make vm-test` pass with the flag **on**, including
   the crc32c verify job and the disconnect drill.
4. The `INIT` exchange shows the kernel granted `FUSE_OVER_IO_URING` when the
   flag says so, and a `bpftrace` count confirms requests crossed the ring.
5. A kernel that refuses the capability, or a mount without the flag, behaves
   exactly as today — the same tests, the same numbers within spread.
6. 4 KiB random read and write improve against a same-day interleaved control,
   or the plan records that they did not and the default stays off.
7. Streaming shapes and the buffered shapes land inside their run-to-run spread.
8. The protocol version is still `2`, and `git diff` touches nothing under
   `crates/lbfs-proto/` or `crates/lbfs-server/`.

## Open Risks

- **Task 1 may end the plan, and that is the most likely single outcome.** Two
  predecessors ended this way. The budget spent to find out is one benchmark
  campaign, the cheapest part of this document by a wide margin.
- **The payload copy survives the ring.** Bulk data does not ride in the
  256-byte header (§1), so a `READ` reply and a `WRITE` request still copy. If
  Task 2 Step 2 finds the payload buffer can be an io_uring *registered*
  buffer, that is a second and possibly larger win — chase it there, not here.
- **Two vCPUs is the wrong shape for this feature.** Its upstream case is core
  scaling, and every core-scaling experiment on this pair has come back empty.
  A win on a 16-core host that does not appear here is a real result and needs
  saying rather than burying; consider re-running Task 7 on a wider guest before
  concluding.
- **The fork is a standing maintenance cost** against a dependency deliberately
  pinned exactly because its upstream releases want human review. A fork that
  wins belongs upstream rather than in a fork.
- **ABI 7.42 pulls in more than one flag.** Claiming a version claims
  everything below it; Task 3 exists as its own gate so a version bump's
  fallout does not get attributed to ring code.
- **`fuse_uring_*` symbol availability is not the same as a working ring.** The
  guest exports 64 of them, which proves the kernel carries the code and nothing
  about whether this mount can register a queue. Task 2 Step 1 is where that
  becomes knowledge.

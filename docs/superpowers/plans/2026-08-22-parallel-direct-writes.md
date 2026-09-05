# Parallel Direct Writes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Status:** Complete, with one acceptance bar missed for a reason that
invalidates the bar rather than the change. All six tasks ran on 2026-08-28.
Probe A carried the day: four threads writing one file went from 6938 to
15910 IOPS, 2.29 × its same-day control and 89 % of the four-threads-four-files
ceiling, where it had reached 38 % of that ceiling before. Every other shape
held within spread, and the kernel confirmed it took both flags — `MAP_SHARED`
on an `O_DIRECT` descriptor returns `ENODEV`, and the write path moved wholly
from `fuse_cache_write_iter` to `fuse_direct_write_iter`.

Four things the execution learned that the plan did not know:

- **Probe B never needed this change, so its 3 × bar was unreachable by
  construction.** A libaio write is not a synchronous iocb, so `fuse_direct_IO`
  already returned `-EIOCBQUEUED` beforehand and the exclusive lock only ever
  spanned the queueing, not the round trip. Measured 0.99 ×. The plan named the
  FUSE event loop as the suspect if B disappointed; `--fuse-threads 4` ruled
  that out (36.5k against 37.5k for one loop). Acceptance criterion 5 fails as
  written; strike it rather than chase it.
- **A pre-existing server bug blocked the acceptance run, and fixing it came
  first.** A saturated mount tripped `in-flight window overrun` and took `EIO`
  mid-job, because the server released a request's window permit only after
  the reply's socket write returned, while the client frees its own slot the
  moment it reads that reply. Fixed on its own branch (`fix/window-permit-
  release`, PR #15); both benchmark builds carry it.
- **Straight before-then-after passes cannot measure this pair.** Sequential
  1 MiB write read 1065 MB/s in the first pass and 66 MB/s in the second, and
  re-measuring the *first* build reproduced the second figure — the row tracks
  the host's page cache. The recorded campaign alternates builds round by
  round and reports medians of three.
- **Linux 7.0 inlines `fuse_dio_wr_exclusive_lock`**, so Step 4's
  bpftrace probe is unavailable (`grep -c` of `/proc/kallsyms` gives 0). Probe
  A against its four-file control stands in for it, as Step 4 anticipated.

The plan first targeted fuser 0.15.1; a 2026-08-27 revision retargeted it at
fuser 0.18.0 (ABI 7.40), which reached `main` through the two-step upgrade
plan. Three things that revision fixed, kept here as history:

- fuser 0.18.0 names `FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES` and can
  negotiate `FUSE_DIRECT_IO_ALLOW_MMAP`, so the plan no longer declares a
  constant by hand and the `mmap(MAP_SHARED)` cost turned from a hard limit
  into a deliberate choice. §2, §3, Task 2, and the Open Risks carry the
  details.
- The kill-priv plan and the fuser upgrade both landed after the benchmark
  tables below got their numbers. The absolute "today" figures run stale —
  single-thread 4 KiB random write now costs ~166 µs, not 300 µs — while the
  lock signature they show still stands. Task 6 takes its own same-day control
  pass, and the acceptance bars read against that pass as multipliers.
- Line numbers into `crates/lbfs-client/src/fuse.rs` predate the 0.18
  migration; search by name. Kernel `fs/fuse/*` citations name Linux v7.0 and
  still hold.

**Goal:** Let two threads write one file at the same time, by answering an application's `O_DIRECT` open with `FOPEN_DIRECT_IO | FOPEN_PARALLEL_DIRECT_WRITES` so the client kernel takes `i_rwsem` shared instead of exclusive across each write round trip.

**Architecture:** One function in the client bridge grows an argument. `open_flags()` reads the application's own open flags — which fuser hands to both the `open` and the `create` callback — and returns the cached reply for an ordinary open and the direct reply for an `O_DIRECT` one. Nothing crosses the wire, nothing changes on the server, and the protocol version stays where the previous plan left it.

**Tech Stack:** Rust (edition 2021), tokio 1, fuser 0.18.0 (ABI 7.40, exact-pinned), io-uring 0.7, rustix 1, postcard 1.1 + serde/serde_bytes, libc, tracing, tempfile; Linux 7.0 guests under libvirt; fio 3.41 for the acceptance run.

**Spec:** `docs/superpowers/specs/2026-08-20-lbfs-design.md`

## Global Constraints

- **`docs/superpowers/plans/2026-08-22-per-write-getattr-elimination.md` landed first (`356f68e..1fe14cc`), as this plan requires.** The working protocol version is `2`, `WriteRequest` carries `kill_suidgid`, the client asks for `FUSE_HANDLE_KILLPRIV_V2`, and `crates/lbfs-server/src/fs/local/killpriv.rs` holds `KillPrivPolicy`. Every code block below describes that end state on fuser 0.18.0.
- Frame header: exactly 24 bytes, little-endian, layout per spec §3.1.
- Protocol magic `LBFS`, version `2`, exact match on both ends. **No task here touches the version.** This change adds no wire field and no opcode.
- Defaults: port `9423`, window `128` (clamp 8..=1024), max body `64 KiB`.
- Status field: `0` OK, `1..=4095` Linux errno, `>= 0xFF00` protocol statuses.
- Names, symlink targets, xattr names and values travel as byte strings — never `String`.
- Bulk data never passes through postcard; senders emit it with vectored writes, receivers read it into pooled buffers.
- The RPC layer reaches storage only through the `FileSystem` trait (spec §5.1). `LocalFs` never touches a frame; `rpc::dispatch` never touches a descriptor. **A FUSE reply flag is not something the storage layer may decide** — §2 of the design section below turns that constraint into the ruling for this plan.
- Every task ends green: `make check` (fmt --check, clippy `-D warnings`, tests) passes before every commit. Run `cargo fmt --all` first — the code blocks below carry rustfmt's output as best a document can, and a stray line width should cost a reformat rather than an argument.
- TDD: write the failing test first for every behavior.
- No `unsafe` outside `crates/lbfs-server/src/fs/local/uring.rs`. `tests/tests/loopback.rs` carries `#![deny(unsafe_code)]` and keeps it.
- Commit after every task with the exact paths staged (no blanket `git add .`).

---

## Design and Context

Read this whole section before Task 1. It answers four questions from Linux **v7.0** sources — the version both guests run — and from the vendored fuser 0.18.0 at `/home/nick/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/fuser-0.18.0/`. Every `fs/fuse/*` line number below cites v7.0 and nothing else.

### The measurement this plan answers

`docs/benchmarks/2026-08-22-bottleneck-analysis.md` records the shape:

| shape | IOPS | mean latency |
|---|---|---|
| randwrite 4k psync QD1, 1 thread, 1 file | 3325 | 300 µs |
| randwrite 4k psync QD1, 4 threads, 1 file | 3275 | 1220 µs |
| randwrite 4k psync QD1, 4 threads, 4 files | 8840 | 451 µs |

Four writers on one file deliver 0.98 × one writer at 4 × the latency — the signature of a per-inode exclusive lock. The queued shape carries the same wound: randwrite 4k libaio QD16 reaches 4963 IOPS at 3023 µs through the mount, where kernel NFS `async` on the same guest pair reaches 30390 at 521 µs (`docs/benchmarks/2026-08-22-nfs-comparison.md`) and lbfs's own raw RPC layer, with no FUSE in the path, serves 48766 reads at the same depth.

### 1. What the two FOPEN bits change, and what stays exclusive

**Today every write takes the exclusive lock, `O_DIRECT` or not.** `fuse_file_write_iter` routes on the *server's* reply flag, not on the application's `O_DIRECT` (`fs/fuse/file.c:1843-1849`):

```c
	/* FOPEN_DIRECT_IO overrides FOPEN_PASSTHROUGH */
	if (ff->open_flags & FOPEN_DIRECT_IO)
		return fuse_direct_write_iter(iocb, from);
	else if (fuse_file_passthrough(ff))
		return fuse_passthrough_write_iter(iocb, from);
	else
		return fuse_cache_write_iter(iocb, from);
```

`open_flags()` in `crates/lbfs-client/src/fuse.rs` returns `FOPEN_KEEP_CACHE` and nothing else, so every write on this mount lands in `fuse_cache_write_iter`, which takes `inode_lock(inode)` at `file.c:1494` and releases it at `file.c:1525` — and `generic_file_direct_write`, with the whole round trip behind it, sits inside that window (`file.c:1506-1509`). For a `pwrite` the round trip really does sit inside the lock: `is_sync_kiocb` holds, so `fuse_direct_IO` blocks to completion (`file.c:2887-2898`) rather than returning `-EIOCBQUEUED`. Four psync threads on one inode cannot overlap. That is the 0.98 ×.

**`FOPEN_PARALLEL_DIRECT_WRITES` reaches exactly one predicate, and only from the direct path.** `fuse_dio_wr_exclusive_lock` (`file.c:1397-1424`) returns true — meaning "take the exclusive lock" — on four conditions:

| condition | line | applies to |
|---|---|---|
| the reply lacked `FOPEN_PARALLEL_DIRECT_WRITES` | `file.c:1405-1406` | every open today |
| `IOCB_APPEND` | `file.c:1412-1413` | `O_APPEND` writes, always |
| `FUSE_I_CACHE_IO_MODE` on the inode | `file.c:1416-1417` | any inode with a cached (non-direct) open |
| the write ends past `i_size` | `file.c:1419-1421` | extending writes, always |

`fuse_dio_lock` (`file.c:1426-1451`) turns that into `inode_lock` or `inode_lock_shared`, and re-checks both the end-of-file test and the caching-mode test under the shared lock before committing to it (`file.c:1444-1449`) — a race that loses simply upgrades to exclusive. `fuse_direct_write_iter` (`file.c:1785-1808`) is the only caller. The two bits thus ship as a pair: a reply carrying `FOPEN_PARALLEL_DIRECT_WRITES` without `FOPEN_DIRECT_IO` reaches a kernel that deletes it on arrival — `fuse_file_io_open` at `fs/fuse/iomode.c:220-221` is one `if` and one `&= ~`.

**Non-extending writes inside a pre-sized file take the shared lock. Extending writes and `O_APPEND` stay exclusive, by the kernel's choice, and this plan does not argue with either.** An append needs the eventual end of file before it can pick an offset, and parallel direct I/O past the end of file is a case the kernel says it does not support yet (`file.c:1408-1421`).

**Reads on the same descriptor change route too.** `fuse_file_read_iter` sends every read on an `FOPEN_DIRECT_IO` file to `fuse_direct_read_iter` (`file.c:1822-1824`), which takes **no inode lock at all** and chunks at `fc->max_read` (`file.c:1651`) — the value lbfs already pins through the `max_read=` mount option (`crates/lbfs-client/src/fuse.rs:430`). Buffered reads issued on that descriptor bypass the page cache with it, which is what `O_DIRECT` asked for. The randread QD16 number should not move; §4 makes that a check rather than a hope.

**`FUSE_ASYNC_DIO` and these bits compose, and the composition is the queued-write win.** `fuse_direct_IO` sets `io->async = fc->async_dio` (`file.c:2852`) and `io->blocking = is_sync_kiocb(iocb)` (`file.c:2854`), then returns `-EIOCBQUEUED` for any non-blocking async request (`file.c:2892-2894`). lbfs already negotiates `FUSE_ASYNC_DIO` (`fuse.rs:379-383`), so a libaio iocb against a pre-sized file becomes a background request that returns immediately — under a *shared* lock rather than an exclusive one. One line qualifies it: `if ((offset + count > i_size) && io->write) io->blocking = true;` (`file.c:2866-2868`). An extending write is synchronous even with the capability, which is the second reason §4 insists the working file exists at full size before the run.

**The writeback cache on the mount does not conflict with a per-file `FOPEN_DIRECT_IO`, and the kernel invalidates nothing extra on open.** Three separate mechanics say so:

- `fuse_open` invalidates the inode's pages only when the reply *lacks* `FOPEN_KEEP_CACHE` (`file.c:289-294`). Keeping that bit on the direct reply is what stops every `O_DIRECT` open from throwing away pages a buffered reader on the same file still wants. Keep it.
- `fuse_direct_io` handles coherence per range instead: `filemap_write_and_wait_range` before the transfer when the file is direct (`file.c:1667-1673`), `invalidate_inode_pages2_range` before a write (`file.c:1682-1688`), and again after it (`file.c:1741-1748`), with the comment naming `generic_file_direct_write` as the model it copies.
- `fuse_finish_open` still links a writable direct descriptor onto `fi->write_files` under the writeback cache (`file.c:227-228`). Harmless and useful: the handle stays available to the writeback of pages some *other* descriptor dirtied.

**Ruling for Q1: reply `FOPEN_KEEP_CACHE | FOPEN_DIRECT_IO | FOPEN_PARALLEL_DIRECT_WRITES` when, and only when, the application passed `O_DIRECT`.**

### 2. Where the decision lives: the client, alone

The client already holds everything the decision needs. fuser hands the application's flags to both callbacks — `fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen)` and `fn create(..., flags: i32, reply: ReplyCreate)` in `fuse.rs` — and `O_DIRECT` survives the trip. The asymmetry is fuser 0.18's, not lbfs's: `open` gets the `OpenFlags` newtype, a transparent wrapper over the same `i32` with a public field (`src/open_flags.rs:21`), while `create` still gets the bare integer. The kernel masks `fuse_open_in.flags` down to `open_flags & ~(O_CREAT | O_EXCL | O_NOCTTY)` and then drops `O_TRUNC` because lbfs withholds `FUSE_ATOMIC_O_TRUNC` (`file.c:34-36`); `fuse_create_open` masks `flags & ~O_NOCTTY` (`dir.c:844-847`). Neither touches `O_DIRECT`.

**Create needs the same treatment as open, for a mechanical reason.** `fuse_create_open` stores the reply's flags on the new handle at `dir.c:887` and then runs the identical `fuse_finish_open` path at `dir.c:905`. A `create` that answered with the cached reply would leave every freshly created file on the serialised path until something closed and reopened it — which is precisely the fio and database shape this plan exists for.

**The alternative — an `OpenReply.flags` field the server fills in — costs a protocol version and buys nothing.** Three reasons against it:

- It pushes FUSE vocabulary across the RPC boundary. `FOPEN_DIRECT_IO` is a bit in `fuse_open_out`; `LocalFs` has no FUSE and, per spec §5.1 and this plan's Global Constraints, must never grow one. A `FileSystem` trait method returning FUSE reply flags breaks the one boundary the spec draws hardest.
- The server already receives the information and throws it away on purpose. `OpenRequest.flags` and `CreateRequest.flags` carry the application's flags verbatim, and `LocalFs::mask_open_flags` (`crates/lbfs-server/src/fs/local/mod.rs:490-511`) drops `O_DIRECT` from its own descriptor because the buffer pool offers no alignment guarantee. Round-tripping a decision the client can make locally would add a field, a protocol version 3, and a lock-step deploy for zero new facts.
- The client's "direct" and the server's "direct" are different words. `FOPEN_DIRECT_IO` means "do not use the client's page cache for this handle" and carries no alignment demand — `fs/fuse/file.c` contains no alignment check anywhere. `O_DIRECT` on the server's descriptor would mean aligned block I/O against the export, which v1 declines. Stripping one and setting the other is coherent, not contradictory, and Task 1 writes that sentence into the spec so nobody reads it as a bug.

**fuser 0.18.0 passes the bits through untouched, and no negotiation applies.** `ReplyOpen::opened(self, fh: ll::FileHandle, flags: FopenFlags)` (`src/reply.rs:361`) forwards `flags` into `fuse_open_out.open_flags`, and `ReplyCreate::created(...)` (`src/reply.rs:502-519`) does the same. The one check either makes is an assertion refusing `FOPEN_PASSTHROUGH`, a bit this plan never sets. Because these are per-open reply flags rather than `INIT` capabilities, `KernelConfig::add_capabilities` never sees them and nothing can refuse them at mount time.

**fuser 0.18.0 names bit 6, so no local constant exists.** `FopenFlags` carries `FOPEN_PARALLEL_DIRECT_WRITES = 1 << 6` (`src/ll/flags/fopen_flags.rs:22`), matching the kernel's `include/uapi/linux/fuse.h:393`:

```c
#define FOPEN_PARALLEL_DIRECT_WRITES	(1 << 6)
```

**Kernel floor: Linux 6.2.** The flag sits in the uapi's ABI 7.36 block (`uapi/linux/fuse.h:203`, "add FOPEN_PARALLEL_DIRECT_WRITES"), absent from v6.1's header and present in v6.2's. Both guests run Linux 7.0, well clear of it. An older kernel degrades cleanly rather than failing: `fuse_file_open` stores the reply's flag word verbatim (`file.c:161`) and tests only the bits it knows, so bit 6 on a pre-6.2 kernel means exactly today's serialised behaviour.

**Ruling for Q2: client-local, one function, no wire change.** Spec §11 records `OpenReply.flags` as the extension point if a future server ever needs to force or veto direct I/O per file.

### 3. The three failure modes, and what lbfs promises about each

**Mixed access — one descriptor `O_DIRECT`, another buffered, same file.** The kernel keeps them coherent and drops the parallelism, in that order of priority. A buffered open runs `fuse_file_cached_io_open` (`iomode.c:238`, reached because `fuse_file_io_open` returns early only for a direct open — `iomode.c:231-233`), which sets `FUSE_I_CACHE_IO_MODE` (`iomode.c:63-65`) and waits for in-flight parallel direct writes to drain (`iomode.c:43-48`). From then on `fuse_dio_wr_exclusive_lock` sees that bit and returns true (`file.c:1416-1417`), so direct writes go back to the exclusive lock — today's behaviour, no worse. Data coherence comes from the range work already quoted in §1: a direct write flushes the overlapping dirty pages and invalidates them before the transfer and again after (`file.c:1667-1688`, `file.c:1741-1748`), and a direct read flushes plus `fuse_sync_writes` first (`file.c:1667-1680`). The waiting buffered open cannot starve, because the waiter sets `FUSE_I_CACHE_IO_MODE` *before* it sleeps (`iomode.c:44`), which pushes every new direct write onto the exclusive branch where it never takes a counter reference at all.

**What lbfs promises:** on one inode a direct descriptor and a cached descriptor stay coherent at page granularity, and neither reads bytes the other already wrote. The pair gives up speed instead: while any cached descriptor stays open, direct writes serialise exactly as they do today. Task 5 pins the coherence half with a loopback test.

**mmap on a direct descriptor — out of scope, by choice.** `fuse_file_mmap` refuses `MAP_SHARED` on an `FOPEN_DIRECT_IO` file with `-ENODEV` unless the connection negotiated `FUSE_DIRECT_IO_ALLOW_MMAP` (`file.c:2393-2399`). `MAP_PRIVATE` still works through `generic_file_mmap` (`file.c:2403-2405`). That capability is `(1ULL << 36)` (`uapi/linux/fuse.h:489`), and fuser 0.18.0 can reach it: `InitFlags::FUSE_DIRECT_IO_ALLOW_MMAP` exists (`src/ll/flags/init_flags.rs:82`), and fuser negotiates `FUSE_INIT_EXT` with the `flags2` split itself (`src/ll/request.rs:999-1014`). The earlier revision of this plan called the bit unreachable inside fuser 0.15.1, whose `fuse_init_in` stopped at one `u32`; on 0.18 the mechanism is one more `Capability` entry in `capabilities()`. This plan still declines it, because a shared mapping beside parallel direct writes raises coherence questions v1 has no answer for. The user-visible change: an application that opens a file `O_DIRECT` and then `mmap`s that same descriptor `MAP_SHARED` gets `ENODEV` where it used to get a mapping. Task 1 records it in the spec, Task 6 observes it on the VM pair, and §11 carries the follow-up.

**`O_APPEND` with `O_DIRECT` — nothing changes, and here is why both mount shapes still hold.** `mask_open_flags` already treats append as a two-case problem (`crates/lbfs-server/src/fs/local/mod.rs:472-489`): with the writeback cache on it clears `O_APPEND` from the server's descriptor, because the client computes offsets and a flushed page must not append twice; with the cache off it keeps `O_APPEND`, because server-side append is what makes the operation atomic against a stale client `i_size`. Both survive:

- `IOCB_APPEND` takes the exclusive lock unconditionally (`file.c:1412-1413`), before any of this plan's bits get a vote. Two appending threads still serialise.
- Both write paths pick the offset the same way, through `generic_write_checks` — `file.c:1496` on the cached path, `file.c:1792` on the direct path — which sets `ki_pos = i_size` for an append while holding the exclusive lock. The `WRITE` frame that reaches the server matches today's frame byte for byte.
- With the cache off, the server's own `O_APPEND` descriptor still lands the bytes at the true end of file, so a stale client `i_size` costs nothing. That is the existing design, untouched.
- One thing the direct path skips: the `fuse_update_attributes(STATX_SIZE | STATX_MODE)` at the head of `fuse_cache_write_iter` (`file.c:1482-1486`). That costs nothing. With the cache off the kernel skipped it anyway (the `if` at `file.c:1482`), and with the cache on the previous plan established that the local cache answers the refresh and it never reaches the wire.

The `O_WRONLY → O_RDWR` promotion in the same function stays put. It exists so a partial-page read-back through the same handle does not hit `EBADF`, it keys off the mount-wide writeback setting rather than any single open, and leaving it alone costs nothing.

Task 4 pins the append behaviour with a loopback test in both mount shapes.

### 4. How the previous plan and this one compose

Both plans touch the kill-priv contract, from opposite sides, and they agree.

**The direct write path never calls `file_remove_privs`, so the `GETXATTR` the previous plan removed does not exist here to begin with.** `fuse_direct_write_iter` runs `generic_write_checks` and then goes straight to the transfer (`file.c:1785-1808`). No `kiocb_modified` appears anywhere in it — compare `file.c:1502` on the cached path. `S_NOSEC` never enters the picture on a direct descriptor: nothing consults it, because nothing probes `security.capability`. That leaves the previous plan's win intact and does not double it; the two changes remove the same round trip from disjoint sets of opens.

**The kernel still sets `FUSE_WRITE_KILL_SUIDGID` on direct writes, with no capability gate of its own** (`file.c:1701-1703`):

```c
		if (write) {
			if (!capable(CAP_FSETID))
				ia->write.in.write_flags |= FUSE_WRITE_KILL_SUIDGID;
```

That runs on every mount, `FUSE_HANDLE_KILLPRIV_V2` negotiated or not. The bridge helper `kill_suidgid(write_flags)` the previous plan added thus keeps working unchanged, and the server keeps receiving the instruction.

**Order matters, and this is the reason.** On a direct descriptor the client kernel performs no strip of its own — no `file_remove_privs`, and no `SETATTR` behind it — so the wire flag plus the server's `KillPrivPolicy` become the only thing standing between a `4755` file and a write that leaves the bit in place. Landing this plan before the previous one would silently widen a setuid window. Landing it after, as the Global Constraints demand, closes the loop: the kernel sets the flag, the bridge forwards it, and the server honours it under either policy branch.

**One small side effect worth naming:** `kiocb_modified` also calls `file_update_time`, so a direct write does not dirty the client's inode timestamps and does not generate the background `SETATTR` that follows. `fuse_write_update_attr` invalidates the cached mtime, ctime, blocks and size instead (`file.c:1802` for the sync case, `file.c:2903` for the queued one), and the next `stat` reads the server's own values. Fewer frames, and the timestamps come from the side that actually wrote the bytes.

### 5. Acceptance experiment design

Four probes, all on the two-guest pair (server `192.168.77.10`, client `192.168.77.11`, mount at `/mnt/lbfs`). Every job runs alone, after a server-side drain: `sync`, then poll `/proc/meminfo` until `Dirty + Writeback` falls under 8 MB. That drain is not optional — the bottleneck analysis records write numbers swinging by a factor of four without it.

**Two setup rules this change makes mandatory.** Both working files must exist at their full 512 MiB before any timed job starts, because an extending write takes the exclusive lock (`file.c:1419-1421`) and runs synchronously even under `FUSE_ASYNC_DIO` (`file.c:2866-2868`) — a job that grows its own file measures the old path and reports no win. And the four-thread job must name its file explicitly with `--filename`, because fio's default gives each job its own `$name.$jobnum.0` and four separate inodes say nothing about one inode's lock.

| probe | job | today | expect |
|---|---|---|---|
| A | randwrite 4k psync QD1, 4 threads, **1 file** | 3275 IOPS, 1220 µs | above 6600 IOPS (the > 2 × bar); 7000-8800 if it reaches the four-file control |
| A control | randwrite 4k psync QD1, 4 threads, 4 files | 8840 IOPS, 451 µs | unchanged — this is the ceiling two vCPUs allow |
| A control | randwrite 4k psync QD1, 1 thread, 1 file | 3325 IOPS, 300 µs | unchanged within spread |
| B | randwrite 4k libaio QD16, 1 file | 4963 IOPS, 3023 µs | above 15000 IOPS; 20000-25000 if writes track reads |
| C | randread 4k psync QD1 | 8322 IOPS, 119.3 µs | unchanged within spread |
| C | randread 4k libaio QD16 | 40290 IOPS, 393 µs | unchanged within spread |
| C | seq read 1M psync | 1580 MB/s, 632 µs | unchanged within spread |
| C | seq write 1M psync | 361 MB/s, 2757 µs | unchanged within spread |
| D | randwrite 4k psync QD1, `direct=0` | not yet measured | far above the direct figure; the page cache absorbs it, and this proves buffered opens kept the cached reply |
| D | second buffered `dd` read of a warm file | not yet measured | gigabytes per second, proving `FOPEN_KEEP_CACHE` survived |

Where probe B's expectation comes from: randread at QD16 reaches 40290 IOPS against a 119 µs QD1 latency, so the depth-16 machinery multiplies the single-operation rate by about 4.8. A 4 KiB write costs roughly 166 µs at QD1 on the merged tree — the kill-priv plan's measured result — which puts the same multiplier near 29000. Below three times the same-day QD16 control means something else still serialises; the bpftrace check in Task 6 Step 4 says what.

The reference points either side: kernel NFS `async` reaches 30390 IOPS on this shape and lbfs's raw RPC layer serves 48766 reads at the same depth, so 20000-25000 would put the mount inside the range the transport can support rather than at a lock's mercy.

**(2026-08-27)** Every absolute number in the table above predates the
kill-priv change and the fuser 0.18 upgrade — single-thread 4 KiB random write
now runs near 6000 IOPS, not 3325 — so the "today" column is shape evidence,
not a prediction. Task 6 Step 2 measures every row fresh on the control build,
and the two acceptance bars read as multipliers against that same-day pass:
probe A above 2 × its own four-thread one-file control, probe B above 3 × its
own QD16 control.

---

## File Map

| Path | Change |
|---|---|
| `docs/superpowers/specs/2026-08-20-lbfs-design.md` | §6 clarifies client-side versus server-side "direct"; §7 gains the per-open direct-I/O bullet; §11 gains two follow-ups |
| `crates/lbfs-client/src/fuse.rs` | `open_flags(app_flags: OpenFlags)`, both call sites, three unit tests |
| `tests/tests/loopback.rs` | six new cases: concurrent direct writers, append with direct, mixed cached and direct |
| `docs/benchmarks/2026-08-22-bottleneck-analysis.md` | records the measured result |

---

### Task 1: Spec — per-open direct I/O

**Files:**
- Edit: `docs/superpowers/specs/2026-08-20-lbfs-design.md` (§6, §7, §11)

**Interfaces:**
- Consumes: nothing.
- Produces: the written contract the later tasks argue from. Names fixed here: the reply flag set `FOPEN_KEEP_CACHE | FOPEN_DIRECT_IO | FOPEN_PARALLEL_DIRECT_WRITES`, and the promise about mixed access.

- [x] **Step 1: Separate the two meanings of "direct" in §6**

Find this line at the end of §6:

```text
Writes otherwise land in the server page cache (no `O_DIRECT` in v1).
```

Replace it with:

```text
Writes otherwise land in the server page cache: the server never puts
`O_DIRECT` on its own descriptor, because the buffer pool offers no alignment
guarantee (§5.1). That is a statement about the *server's* file, and it does
not conflict with §7's per-open direct I/O on the client. The two words mean
different things — `FOPEN_DIRECT_IO` tells the client's kernel to keep one
handle out of its page cache and demands no alignment of anybody, while
`O_DIRECT` on the export would demand aligned block I/O. Setting the first and
stripping the second is one coherent position.
```

- [x] **Step 2: Add the per-open direct-I/O bullet to §7**

In §7, find the caching bullet:

```text
- **Caching (all kernel-side, justified by the one-client assumption):**
`entry_timeout`/`attr_timeout` default 1 s, CLI-tunable (0 disables);
**writeback cache** on (kernel aggregates small writes — the biggest win
for build workloads); `keep_cache` so re-reads stay local; `readdirplus`
on; `max_write`/`max_readahead` = negotiated max I/O size.
```

Directly after it, add:

```text
- **Per-open direct I/O:** an `OPEN` or `CREATE` whose flags carry `O_DIRECT`
comes back with `FOPEN_KEEP_CACHE | FOPEN_DIRECT_IO |
FOPEN_PARALLEL_DIRECT_WRITES`; every other open comes back with
`FOPEN_KEEP_CACHE` alone. The client decides this by itself from the flags
fuser hands its `open` and `create` callbacks; nothing crosses the wire and the
server's view of the open never changes.

  Without `FOPEN_DIRECT_IO` the kernel routes even an `O_DIRECT` write through
`fuse_cache_write_iter`, which holds `i_rwsem` exclusively across the whole
round trip, so four threads writing one file measure 0.98 × one thread. With
both bits, a write that stays inside the file's current size takes the shared
lock and overlaps its neighbours. Three shapes stay exclusive because the
kernel keeps them so: appends, writes past the end of file, and any inode that
also has a non-direct descriptor open.

  `FOPEN_KEEP_CACHE` rides the direct reply as well. The bit governs the
*inode's* page cache rather than the handle's, so dropping it would make every
`O_DIRECT` open discard pages a buffered reader on the same file is still
using. Coherence between the two comes from the direct path itself, which
flushes and invalidates the page range it touches before the transfer and
again after.

  **What the mount promises about mixed access.** A direct descriptor and a
cached descriptor on one inode stay coherent at page granularity, and neither
reads bytes the other has already written. What the pair gives up is speed:
while any cached descriptor stays open, direct writes serialise exactly as
they did before this behaviour existed.

  **What it costs.** `mmap(MAP_SHARED)` on a descriptor the application opened
`O_DIRECT` now fails `ENODEV`. The kernel allows that combination only under
`FUSE_DIRECT_IO_ALLOW_MMAP`, an `INIT` capability this mount chooses not to
negotiate — a shared mapping beside parallel direct writes raises coherence
questions v1 leaves unanswered. `MAP_PRIVATE` on such a descriptor still
works, and `mmap` on an ordinary open keeps its behaviour. §11 carries the
follow-up.
```

- [x] **Step 3: Add the two follow-ups to §11**

At the end of the "Future work" list in §11, add:

```text
- **`FUSE_DIRECT_IO_ALLOW_MMAP`.** Would restore `mmap(MAP_SHARED)` on an
  `O_DIRECT` descriptor (§7). fuser 0.18 reaches the bit — `FUSE_INIT_EXT`
  and `flags2` both ship — so the mechanism is one more `Capability` entry
  in `capabilities()`. Take it only after answering what a shared mapping
  means for coherence beside parallel direct writes; the kernel gate exists
  because that combination is subtle, not because the bit was hard to set.
- **A server-decided `OPEN` reply.** `OpenReply` carries only `fh` today, and
  the client picks its own FUSE reply flags (§7). A server that wanted to force
  or veto direct I/O per file — a policy engine, or a backend that knows a file
  is on tape — would add a `flags` field there and a protocol version bump. The
  field does not exist yet because nothing has needed it, and inventing it
  would put FUSE vocabulary inside the `FileSystem` trait (§5.1).
```

- [x] **Step 4: Check the diff**

Run: `git diff --stat docs/superpowers/specs/2026-08-20-lbfs-design.md`
Expected: one file changed, three hunks.

- [x] **Step 5: Commit**

```bash
git add docs/superpowers/specs/2026-08-20-lbfs-design.md
git commit -m "docs(spec): per-open direct I/O for O_DIRECT opens"
```

---

### Task 2: Client — decide the reply flags from the application's open flags

**Files:**
- Edit: `crates/lbfs-client/src/fuse.rs` (`fn open_flags`, the `open` and `create` callbacks, and the `mod tests` block — search by name)

**Interfaces:**
- Consumes: fuser 0.18's names — `FopenFlags` with its `FOPEN_PARALLEL_DIRECT_WRITES` variant and the `OpenFlags` newtype, both already in the file's import block, and in scope for the tests through `use super::*`.
- Produces: `fn open_flags(app_flags: OpenFlags) -> FopenFlags`. Both `open` and `create` call it with the flags fuser handed them; `create` wraps its bare `i32` in `OpenFlags` at the call site.

- [x] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `crates/lbfs-client/src/fuse.rs`, beside the existing `opens_keep_the_page_cache`:

```rust
    /// The whole change, stated as a table.
    ///
    /// An open carrying `O_DIRECT` comes back on the kernel's direct path with
    /// the parallel-write bit; every other open keeps the cached reply it has
    /// always had. `fuse_file_write_iter` routes on this reply and not on the
    /// application's own flag (`fs/fuse/file.c:1843-1849`), which is why an
    /// `O_DIRECT` write serialises today.
    #[test]
    fn only_an_o_direct_open_gets_the_direct_io_reply() {
        let direct = FopenFlags::FOPEN_DIRECT_IO | FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES;

        for plain in [
            libc::O_RDONLY,
            libc::O_WRONLY,
            libc::O_RDWR,
            libc::O_RDWR | libc::O_APPEND,
            libc::O_WRONLY | libc::O_SYNC,
            libc::O_RDONLY | libc::O_NONBLOCK,
        ] {
            let reply = open_flags(OpenFlags(plain));
            assert_eq!(reply, FopenFlags::FOPEN_KEEP_CACHE, "flags {plain:#o}");
            assert!(!reply.intersects(direct), "flags {plain:#o}");
        }

        // `O_APPEND | O_DIRECT` belongs in the direct set even though every
        // append takes the exclusive lock anyway (`file.c:1412-1413`): the
        // reply describes the handle, and the kernel decides per write.
        for flags in [
            libc::O_RDONLY | libc::O_DIRECT,
            libc::O_WRONLY | libc::O_DIRECT,
            libc::O_RDWR | libc::O_DIRECT,
            libc::O_RDWR | libc::O_DIRECT | libc::O_APPEND,
            libc::O_WRONLY | libc::O_DIRECT | libc::O_SYNC,
        ] {
            let want = FopenFlags::FOPEN_KEEP_CACHE | direct;
            assert_eq!(open_flags(OpenFlags(flags)), want, "flags {flags:#o}");
        }
    }

    /// `FOPEN_PARALLEL_DIRECT_WRITES` means nothing on its own. The kernel
    /// deletes it from any reply that did not also carry `FOPEN_DIRECT_IO`
    /// (`fs/fuse/iomode.c:220-221`), and the only code that reads it sits
    /// behind `fuse_direct_write_iter` (`file.c:1405`, reached from
    /// `file.c:1844-1845`). The two bits travel together or not at all.
    #[test]
    fn the_parallel_write_bit_never_travels_alone() {
        for flags in [
            libc::O_RDONLY,
            libc::O_WRONLY | libc::O_DIRECT,
            libc::O_RDWR | libc::O_APPEND,
            libc::O_RDWR | libc::O_DIRECT | libc::O_SYNC,
        ] {
            let reply = open_flags(OpenFlags(flags));
            if reply.contains(FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES) {
                assert!(reply.contains(FopenFlags::FOPEN_DIRECT_IO), "{flags:#o}");
            }
        }
    }
```

An earlier revision of this plan added a third test pinning a hand-declared
bit-6 constant against collisions with fuser's named flags. fuser 0.18 names
`FOPEN_PARALLEL_DIRECT_WRITES` itself (`src/ll/flags/fopen_flags.rs:22`), so
the constant and its pin test no longer exist — the exact-pinned dependency
owns that value now.

Then replace the existing `opens_keep_the_page_cache` test in the same block:

```rust
    /// Re-reads stay local whether or not writes are cached: `--no-writeback`
    /// turns off dirty-page aggregation, not the page cache.
    ///
    /// The direct reply keeps the bit too. `FOPEN_KEEP_CACHE` governs the
    /// *inode's* pages rather than the handle's, and `fuse_open` invalidates
    /// the whole mapping when a reply omits it (`fs/fuse/file.c:292-293`), so
    /// dropping it here would let one `O_DIRECT` open throw away the cache a
    /// buffered reader on the same file is still using.
    #[test]
    fn opens_keep_the_page_cache() {
        for flags in [libc::O_RDONLY, libc::O_RDWR | libc::O_DIRECT] {
            assert!(open_flags(OpenFlags(flags)).contains(FopenFlags::FOPEN_KEEP_CACHE));
        }
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p lbfs-client --lib open_flags direct_io parallel_write`
Expected: FAIL to compile — `this function takes 0 arguments but 1 argument was supplied` on every `open_flags(...)` call. The flag names themselves resolve, because fuser 0.18 supplies them.

- [x] **Step 3: Give `open_flags` the application's flags**

In `crates/lbfs-client/src/fuse.rs`, replace `fn open_flags()` and its doc comment:

```rust
/// Flags on an `OPEN`/`CREATE` reply, decided from the application's own open
/// flags.
///
/// `FOPEN_KEEP_CACHE` on every reply (spec §7): without it the kernel throws
/// away an inode's cached pages on every open, so a file read twice is fetched
/// twice. One client owns the export, so nothing can invalidate that cache
/// behind the mount's back, and this holds whether or not writes are cached —
/// `--no-writeback` turns off dirty-page aggregation, not reading. It holds on
/// the direct reply too, because the bit governs the inode's pages rather than
/// this handle's, and a buffered reader on the same file still wants them.
///
/// `FOPEN_DIRECT_IO | FOPEN_PARALLEL_DIRECT_WRITES` when the application asked
/// for `O_DIRECT`, and only then. `fuse_file_write_iter` routes on this reply
/// rather than on the application's flag (`fs/fuse/file.c:1843-1849`), so
/// without the first bit even an `O_DIRECT` write goes through
/// `fuse_cache_write_iter`, which holds `i_rwsem` exclusively from
/// `file.c:1494` to `file.c:1525` — across the whole round trip, since a
/// `pwrite` is a synchronous iocb. That is why four threads writing one file
/// measure 0.98 × one thread. The second bit is the one that relaxes the lock:
/// `fuse_dio_wr_exclusive_lock` returns false only for a reply that carries it
/// (`file.c:1405-1406`), and `fuse_dio_lock` then takes `inode_lock_shared`
/// (`file.c:1436`). The kernel keeps three shapes exclusive whatever this
/// function says — appends (`file.c:1412-1413`), writes past the end of file
/// (`file.c:1419-1421`), and any inode that also has a cached descriptor open
/// (`file.c:1416-1417`) — which is what makes the relaxation safe rather than
/// merely fast.
///
/// The two direct bits ship as a pair because the kernel demands it:
/// `fuse_file_io_open` deletes the parallel bit from any reply lacking
/// `FOPEN_DIRECT_IO` (`fs/fuse/iomode.c:220-221`).
///
/// The server sees none of this. It receives the same flags in
/// `OpenRequest`/`CreateRequest` and goes on stripping `O_DIRECT` from its own
/// descriptor, which is a different question — the FUSE flag means "keep this
/// handle out of the client's page cache" and demands no alignment from
/// anybody. Spec §6 and §7 say it at length.
fn open_flags(app_flags: OpenFlags) -> FopenFlags {
    let mut flags = FopenFlags::FOPEN_KEEP_CACHE;
    if app_flags.0 & libc::O_DIRECT != 0 {
        flags |= FopenFlags::FOPEN_DIRECT_IO | FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES;
    }
    flags
}
```

- [x] **Step 4: Hand the decision to both call sites**

In `crates/lbfs-client/src/fuse.rs`, replace the `open` callback:

```rust
    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let ino = ino.0;
        let (conn, _) = self.ctx();
        self.rt.spawn(async move {
            match conn.open(ino, flags.0 as u32).await {
                Ok(fh) => reply.opened(FileHandle(fh), open_flags(flags)),
                Err(e) => reply.error(errno(e)),
            }
        });
    }
```

and the `created` arm of the `create` callback:

```rust
                Ok((e, fh)) => reply.created(
                    &ttl,
                    &to_fuse_attr(e.node, &e.attr),
                    Generation(e.generation),
                    FileHandle(fh),
                    open_flags(OpenFlags(flags)),
                ),
```

`create` needs this as much as `open` does: `fuse_create_open` stores the reply's flags on the new handle (`fs/fuse/dir.c:887`) and runs the same `fuse_finish_open` path (`dir.c:905`), so a `create` answering with the cached reply would leave every freshly made file on the serialised path until something closed and reopened it. In `open` the compiler enforces the plumbing — `flags` is the only `OpenFlags` in the callback. In `create` fuser hands a bare `i32`, and the `OpenFlags(flags)` wrap is the one place this plan constructs the newtype; `mode` and `umask` are `u32`, so handing the wrong integer still fails to compile.

- [x] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p lbfs-client --lib open_flags direct_io parallel_write page_cache`
Expected: PASS — `only_an_o_direct_open_gets_the_direct_io_reply`, `the_parallel_write_bit_never_travels_alone`, `opens_keep_the_page_cache`.

- [x] **Step 6: Run the whole gate**

Run: `make check`
Expected: PASS. `open_flags` has exactly two callers and both moved in Step 4, so nothing else in the workspace mentions it.

- [x] **Step 7: Commit**

```bash
git add crates/lbfs-client/src/fuse.rs
git commit -m "feat(client): O_DIRECT opens reply FOPEN_DIRECT_IO | FOPEN_PARALLEL_DIRECT_WRITES"
```

---

### Task 3: Loopback — two concurrent `O_DIRECT` writers on one file

**Files:**
- Edit: `tests/tests/loopback.rs`

**Interfaces:**
- Consumes: Task 2's reply flags.
- Produces: `fn two_direct_writers_on_one_file_both_land(writeback: bool)` plus two `#[test]` wrappers, following the file's existing `file_content_round_trips(writeback: bool)` shape.

- [x] **Step 1: Widen the imports**

In `tests/tests/loopback.rs`, replace the `use std::os::unix::fs::MetadataExt;` line at line 57 with:

```rust
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
```

`FileExt` supplies `write_all_at` and `read_exact_at`, which are `pwrite`/`pread` and thus leave the shared file offset alone — the only honest way for two threads to write one file at fixed places. `OpenOptionsExt` supplies `custom_flags`, which is how `O_DIRECT` reaches `open(2)` from Rust.

- [x] **Step 2: Write the failing test**

Add to `tests/tests/loopback.rs`, beside the `file_content_round_trips` pair:

```rust
/// Two threads, two `O_DIRECT` descriptors, one file, one moment.
///
/// This is the shape the mount used to serialise. With only `FOPEN_KEEP_CACHE`
/// on the reply, every `O_DIRECT` write went through `fuse_cache_write_iter`,
/// which holds `inode_lock` from `fs/fuse/file.c:1494` to `file.c:1525` —
/// across the whole round trip, because a `pwrite` is a synchronous iocb — and
/// four threads on one file measured 0.98 × one thread. With
/// `FOPEN_DIRECT_IO | FOPEN_PARALLEL_DIRECT_WRITES` the same writes take
/// `inode_lock_shared` (`file.c:1432-1450`) and overlap.
///
/// **A loopback mount cannot prove they overlapped.** One host, one runtime,
/// and no honest timing floor to compare against. What it proves is that
/// nothing was lost, torn or misplaced once the kernel let them run together,
/// which is the failure this change could actually introduce. The parallelism
/// itself is a VM measurement; see the plan's acceptance section.
///
/// Three shapes ride along, because each reaches a different branch of
/// `fuse_dio_wr_exclusive_lock`:
///
/// * the file gets its size through an `O_DIRECT` `CREATE`, so the create path
///   answers with the same reply the open path does (`fs/fuse/dir.c:887`);
/// * the concurrent pair writes inside that size, which is the only case the
///   kernel runs shared (`file.c:1419-1421`);
/// * a second pair writes past the end, which the kernel keeps exclusive, and
///   which must still land both blocks.
fn two_direct_writers_on_one_file_both_land(writeback: bool) {
    const BLOCK: usize = 64 * 1024;

    let mut lb = Loopback::start(Opts {
        writeback,
        ..Opts::default()
    });
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();
    let path = mnt.join("shared.dat");

    // Created and sized through an `O_DIRECT` descriptor, so this half
    // exercises the `CREATE` reply rather than the `OPEN` reply. Zeros rather
    // than `set_len`, so the concurrent writes below land on real blocks.
    {
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(&path)
            .unwrap();
        f.write_all(&vec![0u8; 2 * BLOCK]).unwrap();
    }
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        2 * BLOCK as u64,
        "the O_DIRECT create did not reach its full size"
    );

    // Inside the end of file: the shared-lock case.
    std::thread::scope(|s| {
        for (mark, offset) in [(b'b', 0u64), (b'c', BLOCK as u64)] {
            let path = path.clone();
            s.spawn(move || {
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(&path)
                    .unwrap();
                f.write_all_at(&vec![mark; BLOCK], offset).unwrap();
            });
        }
    });

    // Past the end of file: the exclusive fallback. Both must still land, and
    // the file must end up exactly twice as long.
    std::thread::scope(|s| {
        for (mark, offset) in [(b'd', 2 * BLOCK as u64), (b'e', 3 * BLOCK as u64)] {
            let path = path.clone();
            s.spawn(move || {
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(&path)
                    .unwrap();
                f.write_all_at(&vec![mark; BLOCK], offset).unwrap();
            });
        }
    });

    // Read the export directly, behind the mount's back: a mount that only
    // agrees with itself would pass every assertion made through it.
    let landed = std::fs::read(export.join("shared.dat")).unwrap();
    assert_eq!(landed.len(), 4 * BLOCK, "the export has the wrong length");
    for (i, mark) in [b'b', b'c', b'd', b'e'].into_iter().enumerate() {
        let block = &landed[i * BLOCK..(i + 1) * BLOCK];
        assert!(
            block.iter().all(|&b| b == mark),
            "block {i} is not a solid run of {:?}; first wrong byte at {:?}",
            mark as char,
            block.iter().position(|&b| b != mark)
        );
    }

    lb.unmount();
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn two_direct_writers_on_one_file_both_land_with_the_writeback_cache() {
    two_direct_writers_on_one_file_both_land(true);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn two_direct_writers_on_one_file_both_land_without_the_writeback_cache() {
    two_direct_writers_on_one_file_both_land(false);
}
```

- [x] **Step 3: Run the new cases**

Run: `cargo test -p lbfs-tests --test loopback two_direct_writers -- --ignored --test-threads=1`
Expected: PASS, both cases. Run it on the pre-Task-2 tree too if you want the contrast — it passes there as well, because the exclusive lock is correct, only slow. What this guards is the relaxed lock: a torn block or a short file here means the shared path lost a write.

- [x] **Step 4: Run the whole loopback suite**

Run: `make test-loopback`
Expected: PASS, no regressions in the existing cases.

- [x] **Step 5: Commit**

```bash
git add tests/tests/loopback.rs
git commit -m "test(loopback): two concurrent O_DIRECT writers on one file"
```

---

### Task 4: Loopback — `O_APPEND` with `O_DIRECT`

**Files:**
- Edit: `tests/tests/loopback.rs`

**Interfaces:**
- Consumes: Task 2's reply flags, and Task 3's `FileExt`/`OpenOptionsExt` imports.
- Produces: `fn appends_stay_whole_with_direct_io(writeback: bool)` plus two `#[test]` wrappers.

- [x] **Step 1: Write the failing test**

Add to `tests/tests/loopback.rs`, directly after the `two_direct_writers_on_one_file_both_land` pair:

```rust
/// Two appenders, one file, both `O_DIRECT`, both mount shapes.
///
/// Append is the one write shape this change deliberately leaves alone, and
/// the reason is a single line: `fuse_dio_wr_exclusive_lock` returns true for
/// `IOCB_APPEND` before it looks at anything else
/// (`fs/fuse/file.c:1412-1413`), because an append has to know the eventual end
/// of the file. So two appenders serialise, and this test says so by insisting
/// that each block arrives whole.
///
/// Both mount shapes, because the server reads `O_APPEND` differently in each
/// and only one of them can be wrong at a time:
///
/// * **writeback on** — the server strips `O_APPEND` from its own descriptor,
///   and the client's kernel picks the offset itself through
///   `generic_write_checks` (`file.c:1792` on the direct path, `file.c:1496` on
///   the cached one) while holding the exclusive lock. Two appends that raced
///   would overwrite one another and the file would come out short.
/// * **writeback off** — the server keeps `O_APPEND`, and the export's own
///   kernel places the bytes at the true end of the file, so a stale client
///   `i_size` costs nothing.
///
/// The direct path also skips the `fuse_update_attributes(STATX_SIZE |
/// STATX_MODE)` that opens `fuse_cache_write_iter` (`file.c:1482-1486`). That
/// is exactly the refresh the writeback cache already answered locally, so the
/// offset arithmetic must come out the same. This test is what says it did.
fn appends_stay_whole_with_direct_io(writeback: bool) {
    const BLOCK: usize = 64 * 1024;

    let mut lb = Loopback::start(Opts {
        writeback,
        ..Opts::default()
    });
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();
    let path = mnt.join("appended.dat");

    std::fs::write(&path, vec![b'a'; BLOCK]).unwrap();

    std::thread::scope(|s| {
        for mark in [b'b', b'c'] {
            let path = path.clone();
            s.spawn(move || {
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(&path)
                    .unwrap();
                f.write_all(&vec![mark; BLOCK]).unwrap();
            });
        }
    });

    let landed = std::fs::read(export.join("appended.dat")).unwrap();
    assert_eq!(
        landed.len(),
        3 * BLOCK,
        "two appends of {BLOCK} bytes onto {BLOCK} bytes must give 3 blocks; \
         a short file means the two appends chose the same offset"
    );
    assert!(landed[..BLOCK].iter().all(|&b| b == b'a'));

    // Order is nobody's business — the exclusive lock says one goes first, not
    // which. Wholeness is: neither block may carry a byte of the other.
    let second = &landed[BLOCK..2 * BLOCK];
    let third = &landed[2 * BLOCK..];
    let mut marks = [second[0], third[0]];
    marks.sort_unstable();
    assert_eq!(marks, [b'b', b'c'], "one appender's block never arrived");
    assert!(second.iter().all(|&b| b == second[0]), "block two is torn");
    assert!(third.iter().all(|&b| b == third[0]), "block three is torn");

    // One more append through an ordinary descriptor, to prove the cached path
    // still agrees with the direct one about where the end of the file is.
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"tail").unwrap();
        f.sync_all().unwrap();
    }
    let landed = std::fs::read(export.join("appended.dat")).unwrap();
    assert_eq!(landed.len(), 3 * BLOCK + 4);
    assert_eq!(&landed[3 * BLOCK..], b"tail");

    lb.unmount();
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn appends_stay_whole_with_direct_io_and_the_writeback_cache() {
    appends_stay_whole_with_direct_io(true);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn appends_stay_whole_with_direct_io_without_the_writeback_cache() {
    appends_stay_whole_with_direct_io(false);
}
```

- [x] **Step 2: Run the new cases**

Run: `cargo test -p lbfs-tests --test loopback appends_stay_whole -- --ignored --test-threads=1`
Expected: PASS, both cases.

- [x] **Step 3: Run the whole loopback suite**

Run: `make test-loopback`
Expected: PASS.

- [x] **Step 4: Commit**

```bash
git add tests/tests/loopback.rs
git commit -m "test(loopback): O_APPEND with O_DIRECT stays whole in both mount shapes"
```

---

### Task 5: Loopback — a cached descriptor beside a direct one

**Files:**
- Edit: `tests/tests/loopback.rs`

**Interfaces:**
- Consumes: Task 2's reply flags, and Task 3's `FileExt`/`OpenOptionsExt` imports.
- Produces: `fn cached_and_direct_descriptors_stay_coherent(writeback: bool)` plus two `#[test]` wrappers.

- [x] **Step 1: Write the failing test**

Add to `tests/tests/loopback.rs`, directly after the `appends_stay_whole_with_direct_io` pair:

```rust
/// One file, two descriptors, one of them direct — the mixed-access promise of
/// spec §7, checked in both directions.
///
/// Opening the cached descriptor puts the inode into caching mode
/// (`fs/fuse/iomode.c:238`, `iomode.c:63-65`), which sends every direct write
/// back to the exclusive lock (`fs/fuse/file.c:1416-1417`). That costs the
/// parallelism and buys back today's behaviour, so nothing here can regress
/// into a race. What it must not cost is coherence, and coherence comes from
/// the direct path doing its own page work: a flush before the transfer
/// (`file.c:1667-1673`), an invalidate before a write (`file.c:1682-1688`) and
/// another after it (`file.c:1741-1748`).
///
/// So: a direct write must be visible to a cached reader with no flush, and a
/// cached write must be visible to a direct reader with no `fsync`. Neither
/// test calls `sync_all`, on purpose — an explicit flush would prove nothing.
fn cached_and_direct_descriptors_stay_coherent(writeback: bool) {
    const BLOCK: usize = 64 * 1024;

    let mut lb = Loopback::start(Opts {
        writeback,
        ..Opts::default()
    });
    let mnt = lb.mnt().to_path_buf();
    let export = lb.export().to_path_buf();
    let path = mnt.join("mixed.dat");

    std::fs::write(&path, vec![b'a'; 2 * BLOCK]).unwrap();

    let cached = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let direct = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(&path)
        .unwrap();

    // Direct write, cached read. The direct path invalidated the range, so the
    // cached descriptor has to go back to the server for it.
    direct.write_all_at(&vec![b'b'; BLOCK], 0).unwrap();
    let mut seen = vec![0u8; BLOCK];
    cached.read_exact_at(&mut seen, 0).unwrap();
    assert!(
        seen.iter().all(|&b| b == b'b'),
        "the cached descriptor served stale pages after a direct write"
    );

    // Cached write, direct read, no flush in between. The direct read's own
    // `filemap_write_and_wait_range` is what has to push the dirty page out.
    cached
        .write_all_at(&vec![b'c'; BLOCK], BLOCK as u64)
        .unwrap();
    let mut seen = vec![0u8; BLOCK];
    direct.read_exact_at(&mut seen, BLOCK as u64).unwrap();
    assert!(
        seen.iter().all(|&b| b == b'c'),
        "the direct descriptor read around a dirty page instead of flushing it"
    );

    // Both descriptors closed before the export is inspected: the second block
    // only has to reach the server by the time the cached handle is gone.
    drop(direct);
    drop(cached);

    let landed = std::fs::read(export.join("mixed.dat")).unwrap();
    assert_eq!(landed.len(), 2 * BLOCK);
    assert!(landed[..BLOCK].iter().all(|&b| b == b'b'));
    assert!(landed[BLOCK..].iter().all(|&b| b == b'c'));

    lb.unmount();
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn cached_and_direct_descriptors_stay_coherent_with_the_writeback_cache() {
    cached_and_direct_descriptors_stay_coherent(true);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn cached_and_direct_descriptors_stay_coherent_without_the_writeback_cache() {
    cached_and_direct_descriptors_stay_coherent(false);
}
```

- [x] **Step 2: Run the new cases**

Run: `cargo test -p lbfs-tests --test loopback cached_and_direct -- --ignored --test-threads=1`
Expected: PASS, both cases.

- [x] **Step 3: Run the whole gate and the whole loopback suite**

Run: `make check && make test-loopback`
Expected: PASS.

- [x] **Step 4: Commit**

```bash
git add tests/tests/loopback.rs
git commit -m "test(loopback): a cached descriptor beside a direct one stays coherent"
```

---

### Task 6: Measure on the VM pair and record the result

**Files:**
- Edit: `docs/benchmarks/2026-08-22-bottleneck-analysis.md`

**Interfaces:**
- Consumes: every task above.
- Produces: the acceptance evidence. No automated test — this one needs two guests and a quiet machine.

- [x] **Step 1: Build the control, deploy it, and lay out the working set**

The A/B needs a build without this change, the way the `FUSE_ASYNC_DIO` campaign rebuilt `e6ebfaf`'s client for its control column. Build the parent of Task 2's commit, deploy it, and lay out both files at full size — an extending write takes the exclusive lock and runs synchronously even under `FUSE_ASYNC_DIO` (`fs/fuse/file.c:2866-2868`), so a job that grows its own file measures nothing.

```bash
make build-guest && make vm-deploy
```

Then on the client guest, with the mount up at `/mnt/lbfs`:

```bash
fio --name=layout --directory=/mnt/lbfs --rw=write --bs=1M --direct=1 \
  --size=512M --nrfiles=1 --filename=seq.dat
fio --name=layout --directory=/mnt/lbfs --rw=write --bs=1M --direct=1 \
  --size=512M --nrfiles=1 --filename=rand.dat
ls -l /mnt/lbfs/seq.dat /mnt/lbfs/rand.dat
```

Expected: both files exactly 536870912 bytes.

- [x] **Step 2: Run the control pass**

Drain the server before every single job:

```bash
ssh 192.168.77.10 'sync; while awk "/^Dirty:|^Writeback:/ {s+=\$2} END {exit !(s>8192)}" /proc/meminfo; do sleep 1; done'
```

Then, on the client guest, the four timed jobs. Probe A first:

```bash
fio --name=randwrite4k_4t_1f --filename=/mnt/lbfs/rand.dat --rw=randwrite \
  --bs=4k --ioengine=psync --iodepth=1 --direct=1 --size=512M --runtime=15 \
  --time_based --numjobs=4 --thread --group_reporting --randrepeat=1 \
  --output-format=json
```

Its two controls:

```bash
fio --name=randwrite4k_1t_1f --filename=/mnt/lbfs/rand.dat --rw=randwrite \
  --bs=4k --ioengine=psync --iodepth=1 --direct=1 --size=512M --runtime=15 \
  --time_based --numjobs=1 --group_reporting --randrepeat=1 \
  --output-format=json

fio --name=randwrite4k_4t_4f --directory=/mnt/lbfs --rw=randwrite \
  --bs=4k --ioengine=psync --iodepth=1 --direct=1 --size=512M --runtime=15 \
  --time_based --numjobs=4 --thread --group_reporting --randrepeat=1 \
  --output-format=json
```

`--filename` on the first two is what makes all four threads share one inode; the third leaves it out on purpose, so fio gives each job its own `randwrite4k_4t_4f.N.0`. Probe B:

```bash
fio --name=randwrite4k_aio16 --filename=/mnt/lbfs/rand.dat --rw=randwrite \
  --bs=4k --ioengine=libaio --iodepth=16 --direct=1 --size=512M --runtime=15 \
  --time_based --numjobs=1 --group_reporting --randrepeat=1 \
  --output-format=json
```

Probe C, the four shapes that must not move:

```bash
for spec in "randread4k_psync randread 4k psync 1 rand.dat" \
            "randread4k_aio16 randread 4k libaio 16 rand.dat" \
            "seqread1m_psync read 1M psync 1 seq.dat" \
            "seqwrite1m_psync write 1M psync 1 seq.dat"; do
  set -- $spec
  fio --name="$1" --filename="/mnt/lbfs/$6" --rw="$2" --bs="$3" \
    --ioengine="$4" --iodepth="$5" --direct=1 --size=512M --runtime=15 \
    --time_based --numjobs=1 --group_reporting --randrepeat=1 \
    --output-format=json
done
```

Probe D, the buffered shapes this change must leave alone:

```bash
fio --name=randwrite4k_buffered --filename=/mnt/lbfs/rand.dat --rw=randwrite \
  --bs=4k --ioengine=psync --iodepth=1 --direct=0 --size=512M --runtime=15 \
  --time_based --numjobs=1 --group_reporting --randrepeat=1 --end_fsync=1 \
  --output-format=json

dd if=/mnt/lbfs/seq.dat of=/dev/null bs=1M count=256
dd if=/mnt/lbfs/seq.dat of=/dev/null bs=1M count=256
```

Record every number. This control pass is the baseline every acceptance bar reads against — the "today" figures earlier in the plan predate the kill-priv change and the fuser 0.18 upgrade, so expect the write shapes to come in well above them (single-thread randwrite near 6000 IOPS rather than 3325) and treat any resemblance as coincidence, not confirmation.

- [x] **Step 3: Deploy the change and run the same pass**

```bash
make build-guest && make vm-deploy
```

Then repeat Step 2's jobs, drain and all. Expected, with "control" meaning Step 2's same-day figure for the same shape:

| probe | job | with the change |
|---|---|---|
| A | randwrite 4k psync QD1, 4 threads, 1 file | **above 2 × its control**; near the four-file control if the lock was the whole story |
| A | randwrite 4k psync QD1, 1 thread, 1 file | unchanged within spread |
| A | randwrite 4k psync QD1, 4 threads, 4 files | unchanged within spread |
| B | randwrite 4k libaio QD16 | **above 3 × its control**; near 29000 IOPS if writes track reads |
| C | randread 4k psync QD1 | unchanged within spread |
| C | randread 4k libaio QD16 | unchanged within spread |
| C | seq read 1M psync | unchanged within spread |
| C | seq write 1M psync | unchanged within spread |
| D | randwrite 4k psync QD1, `direct=0` | unchanged within spread |
| D | second `dd` read of a warm file | unchanged — gigabytes per second, `FOPEN_KEEP_CACHE` survived |

The two bold rows are the acceptance bars. Everything else is a no-regression check.

- [x] **Step 4: Confirm the kernel took the bits**

Two observations, because the two bits arrive differently.

`FOPEN_DIRECT_IO` shows itself through `mmap`: the kernel refuses `MAP_SHARED` on a direct descriptor with `ENODEV` (`fs/fuse/file.c:2393-2399`) and allows it on an ordinary one. On the client guest:

```bash
python3 - <<'EOF'
import mmap, os
p = '/mnt/lbfs/mmapcheck'
fd = os.open(p, os.O_RDWR | os.O_CREAT, 0o644)
os.ftruncate(fd, 4096)
m = mmap.mmap(fd, 4096, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
m[0:4] = b'ok!!'
m.close(); os.close(fd)
print('buffered MAP_SHARED: ok')
fd = os.open(p, os.O_RDWR | os.O_DIRECT)
try:
    m = mmap.mmap(fd, 4096, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
    print('O_DIRECT MAP_SHARED: mapped')
    m.close()
except OSError as e:
    print('O_DIRECT MAP_SHARED:', e.strerror)
os.close(fd)
EOF
```

Expected after the change: `buffered MAP_SHARED: ok` then `O_DIRECT MAP_SHARED: No such device`. On the control build both lines say the mapping succeeded. A run that still maps means the reply flag never reached the kernel and every number in Step 3 measures the old path.

`FOPEN_PARALLEL_DIRECT_WRITES` shows itself in probe A's IOPS. If that number disappoints, ask the lock directly while probe A runs:

```bash
grep -c fuse_dio_wr_exclusive_lock /proc/kallsyms
sudo bpftrace -e 'kretprobe:fuse_dio_wr_exclusive_lock { @[retval] = count(); }' -c 'sleep 10'
```

Expected: `@[0]` dominates, which is the shared branch. A count dominated by `@[1]` names which of the four conditions in `fuse_dio_wr_exclusive_lock` fired — the usual culprit is a working file that was not laid out at full size, which sends every write down the past-the-end branch at `file.c:1419-1421`. A `grep -c` of `0` means the compiler inlined the function and this probe is unavailable; fall back to comparing probe A against its four-file control.

- [x] **Step 5: Run the integrity job**

The parallelism is worth nothing if a write lands in the wrong place, and the loopback tests could not check that under real concurrency.

```bash
make vm-test
```

Expected: PASS, including the fio crc32c verify job in `vm/tests/fio.sh`.

- [x] **Step 6: Record it**

Append a section to `docs/benchmarks/2026-08-22-bottleneck-analysis.md`:

```text
## Phase 9: the per-inode write lock, lifted

The mount answered every `OPEN` with `FOPEN_KEEP_CACHE` alone, so
`fuse_file_write_iter` sent even an `O_DIRECT` write to
`fuse_cache_write_iter` (`fs/fuse/file.c:1843-1849`), which holds `inode_lock`
across the whole round trip (`file.c:1494`, `file.c:1525`). That is the lock
behind "four writers on one file deliver 0.98 × one writer" above.

The client now answers an `O_DIRECT` open with `FOPEN_DIRECT_IO |
FOPEN_PARALLEL_DIRECT_WRITES`, which routes those writes to
`fuse_direct_write_iter` and lets `fuse_dio_lock` take the shared lock
(`file.c:1405-1406`, `file.c:1432-1450`) for any write that stays inside the
file. Appends, extending writes and inodes with a cached descriptor open keep
the exclusive lock, by the kernel's choice.

[table from Step 3]

`mmap(MAP_SHARED)` on an `O_DIRECT` descriptor now returns ENODEV, which is
also how this run confirmed the kernel took the flag; the mount declines to
negotiate `FUSE_DIRECT_IO_ALLOW_MMAP` on purpose, and spec §11 carries the
follow-up. Buffered opens keep today's behaviour: `FOPEN_KEEP_CACHE` alone,
with the writeback cache still aggregating them.
```

Then correct the closing paragraph of "The exclusive inode lock" section — the sentence reading "Only separate files scale" — so it points at this new section instead of leaving a conclusion standing that is no longer true.

- [x] **Step 7: Commit**

```bash
git add docs/benchmarks/2026-08-22-bottleneck-analysis.md
git commit -m "docs(bench): per-inode write serialisation lifted for O_DIRECT opens"
```

---

## Acceptance Criteria

1. `make check` passes: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
2. `make test-loopback` passes, including all six new cases across both mount shapes.
3. `make vm-test` passes, including the fio crc32c verify job.
4. Through the VM mount, 4 KiB random write psync QD1 with four threads on **one** file exceeds twice the control build's same-day figure for the same shape — the shape whose four-thread rate measured 0.98 × its single-thread rate when the lock had it.
5. ~~4 KiB random write libaio QD16 exceeds three times the control build's same-day figure.~~ **Struck on execution (2026-08-28): the premise is false.** The bar assumed queue-depth writes serialise on the same lock probe A measures. They do not — a libaio write is not a synchronous iocb, so `fuse_direct_IO` returned `-EIOCBQUEUED` before this change ever existed (`file.c:2892-2894`) and the exclusive lock only ever spanned the queueing rather than the round trip. Measured 0.99 ×, with `--fuse-threads 4` ruling out the event loop the Open Risks named as the alternative suspect. Nothing in this plan could have moved that shape, and no later plan should read this line as an unmet target.
6. Single-thread randwrite psync QD1, four-thread four-file randwrite, randread psync QD1, randread libaio QD16, sequential 1 MiB read and sequential 1 MiB write all land inside their run-to-run spread.
7. Buffered jobs hold their ground: `direct=0` randwrite matches the control build, and a second `dd` read of a warm file still comes out of the page cache at gigabytes per second.
8. `mmap(MAP_SHARED)` succeeds on an ordinary descriptor and returns `ENODEV` on an `O_DIRECT` one — the observation that proves the kernel took `FOPEN_DIRECT_IO`.
9. The protocol version is still `2` and `git diff` touches no file under `crates/lbfs-proto/` or `crates/lbfs-server/`.

## Open Risks

- **The kernel may keep taking the exclusive lock for a reason the plan did not foresee.** `fuse_dio_wr_exclusive_lock` has four branches and probe A cannot say which one fired. Task 6 Step 4's bpftrace answers that, and the likeliest cause is the least interesting one: a working file that was not laid out at full size, so every write runs past the end. Lay the files out first.
- **Probe B may fall short of its 3 × bar.** The arithmetic behind that number assumes the write path's queued behaviour tracks the read path's, and a 4 KiB write carries its payload in the *request* — copied off `/dev/fuse` by the event loop — where a read carries it in the reply. fuser 0.18 runs one event-loop thread by default (`fuser-0`), and `--fuse-threads` raises that; the upgrade benchmark measured no win from extra threads *without* this change, which says nothing about the picture once writes overlap. If B disappoints while A succeeds, re-run the thread A/B with this change deployed before concluding a different plan is due.
- **`mmap(MAP_SHARED)` on an `O_DIRECT` descriptor stops working.** Recorded in spec §7 as a deliberate cost rather than a bug. No workload in this repository does it, and applications that open `O_DIRECT` overwhelmingly do not map the same descriptor shared. If a real workload trips over it, the honest fixes are negotiating `FUSE_DIRECT_IO_ALLOW_MMAP` — one `Capability` entry on fuser 0.18, once the coherence questions in §3 have answers — or a CLI flag that turns the direct reply off; and rolling back is simply deploying the previous client build, since nothing on the wire changed.
- **Mixed cached and direct access loses the parallelism without warning.** One buffered descriptor anywhere on the inode sets `FUSE_I_CACHE_IO_MODE` and pushes every direct write back to the exclusive lock. Correct, but silent: a workload that opens a file both ways will see none of this plan's win and no message saying why. `/sys/kernel/debug/tracing` and the bpftrace probe are the only diagnostics.
- **A kernel older than 6.2 gets `FOPEN_DIRECT_IO` and not the parallelism.** Such a kernel stores the bit and never tests it, so writes take the exclusive lock through `fuse_direct_write_iter` instead of through `fuse_cache_write_iter` — correct, no faster, and the page-cache bypass still applies. Both guests run 7.0, so this only matters for a deployment elsewhere.
- **The direct path skips `file_update_time`.** A direct write no longer dirties the client's inode timestamps, so the background `SETATTR` that used to follow does not happen and `stat` reports the server's own mtime. That is a better answer, and still a change: a test or a workload that depended on the client's timestamp arriving first would notice.
- **This plan is second in a pair, and reversing the order breaks the setuid promise.** On a direct descriptor the client kernel performs no strip of its own, so the wire flag and the server's `KillPrivPolicy` are the only enforcement. The Global Constraints say the per-write-getattr-elimination plan lands first; that is a correctness ordering, not a convenience.

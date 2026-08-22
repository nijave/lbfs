# Per-Write Metadata Round Trip Elimination Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut the second round trip that every `WRITE` through an lbfs mount drags behind it, taking 4 KiB random-write latency from 296 µs to roughly 205 µs without weakening the set-user-ID guarantee the mount owes its callers.

**Architecture:** The client asks its kernel for `FUSE_HANDLE_KILLPRIV_V2`, which lets the kernel latch `S_NOSEC` on each inode and stop probing `security.capability` before every write. In exchange lbfs takes on the promise that flag encodes, so the `WRITE` body grows a `kill_suidgid` flag that the bridge copies out of fuser's `write_flags`, and the server clears set-user-ID and set-group-ID itself whenever it holds `CAP_FSETID`, which is exactly when the backing kernel skips the strip. One independent change rides along: the node table remembers each node's file type, which drops a syscall from every xattr operation.

**Tech Stack:** Rust (edition 2021), tokio 1, fuser 0.15.1 (`abi-7-31`), io-uring 0.7, rustix 1 (adds the `thread` feature), postcard 1.1 + serde/serde_bytes, libc, tracing, tempfile; Linux 7.0 guests under libvirt.

**Spec:** `docs/superpowers/specs/2026-08-20-lbfs-design.md`

## Global Constraints

- Frame header: exactly 24 bytes, little-endian, layout per spec §3.1.
- Protocol magic `LBFS`; this plan moves the version constant from `1` to `2`, and both ends still demand an exact match.
- Defaults: port `9423`, window `128` (clamp 8..=1024), max body `64 KiB`. Leave `DEFAULT_MAX_IO_SIZE` alone — separate in-flight work in this tree raises it from 1 MiB to 4 MiB, and no task here touches it.
- Status field: `0` OK, `1..=4095` Linux errno, `>= 0xFF00` protocol statuses.
- Flags: bit 0 `NO_REPLY`; bit 1 reserved for `FORCE_SYNC` — define the constant, never set it.
- Names, symlink targets, xattr names and values travel as byte strings — never `String`.
- Bulk data never passes through postcard; senders emit it with vectored writes, receivers read it into pooled buffers.
- The server never follows symlinks; every path step uses `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`.
- The RPC layer reaches storage only through the `FileSystem` trait (spec §5.1). `LocalFs` never touches a frame; `rpc::dispatch` never touches a descriptor.
- Every task ends green: `make check` (fmt --check, clippy `-D warnings`, tests) passes before every commit.
- TDD: write the failing test first for every behavior.
- No `unsafe` outside `crates/lbfs-server/src/fs/local/uring.rs`.
- Commit after every task with the exact paths staged (no blanket `git add .`).

---

## Design and Context

Read this section before Task 1. It corrects the working hypothesis the file name carries: the extra round trip per write is a `GETXATTR`, not a `GETATTR`. The fix — `FUSE_HANDLE_KILLPRIV_V2` — is the same either way, but the reasoning, the tests and the server contract all hang off the corrected mechanism.

Kernel line numbers cite Linux **v7.0**, the version both guests run. Local copies of the FUSE sources sit under the session scratchpad `k/v70/`; `fs/inode.c`, `fs/attr.c`, `fs/open.c`, `security/commoncap.c` and `include/linux/fs.h` sit under `k/v70x/`. The host's `/usr/include/linux/fuse.h` (ABI 7.45) agrees with the uapi header on every constant this plan uses.

### 1. What the kernel does around each write, and what the flag changes

**The pre-write attribute refresh is real but cheap.** `fuse_cache_write_iter` (`fs/fuse/file.c:1471-1530`) opens with:

```c
	if (fc->writeback_cache) {
		/* Update size (EOF optimization) and mode (SUID clearing) */
		err = fuse_update_attributes(mapping->host, file,
					     STATX_SIZE | STATX_MODE);
```

The comment at `file.c:1483` names both halves of the mask. `STATX_SIZE` feeds the end-of-file check; `STATX_MODE` feeds `setattr_should_drop_suidgid` three lines later at `file.c:1489-1491`, which picks between the iomap buffered path and the write-through path.

That refresh hits the local cache on this mount. `fuse_update_get_attr` (`fs/fuse/dir.c:1520-1566`) forces a round trip only when `request_mask & inval_mask & ~cache_mask` holds (`dir.c:1544`) or when the attribute lifetime runs out (`dir.c:1547`). After a write, `fuse_write_update_attr` (`file.c:1169-1186`) marks `FUSE_STATX_MODSIZE` stale at `file.c:1183`, and `FUSE_STATX_MODSIZE` expands to `STATX_MTIME | STATX_CTIME | STATX_BLOCKS | STATX_SIZE` (`fs/fuse/fuse_i.h:1303,1306`) — **`STATX_MODE` is absent**. With the writeback cache on, `fuse_get_cache_mask` returns `STATX_MTIME | STATX_CTIME | STATX_SIZE` (`fs/fuse/inode.c:318-326`). The whole product collapses to zero:

```text
request  (SIZE|MODE)
& inval  (MTIME|CTIME|BLOCKS|SIZE)
& ~cache ~(MTIME|CTIME|SIZE)
= 0
```

Only the lifetime branch survives, so this refresh costs one `GETATTR` per `--ttl`, not one per write. Two independent facts confirm it. `--no-writeback` skips `file.c:1482` entirely, yet the same 2:1 ratio persists (10149 triples against 20303 reply frames, `docs/benchmarks/2026-08-22-bottleneck-analysis.md`, "Phase 7"). And the ratio is 2:1 rather than 3:1, so exactly **one** extra request rides each write.

**The one extra request is a `GETXATTR` of `security.capability`.** `fuse_cache_write_iter` calls `kiocb_modified(iocb)` at `file.c:1502`, which reaches `file_remove_privs_flags` (`fs/inode.c:2317-2341`):

```c
	if (IS_NOSEC(inode) || !S_ISREG(inode->i_mode))
		return 0;

	kill = dentry_needs_remove_privs(file_mnt_idmap(file), dentry);
	...
	if (!error)
		inode_has_no_xattr(inode);
```

Without `S_NOSEC` the short-circuit at `fs/inode.c:2324` misses, so `dentry_needs_remove_privs` (`fs/inode.c:2285-2302`) runs `security_inode_need_killpriv`, and the capability module answers it with a real xattr read — `cap_inode_need_killpriv` (`security/commoncap.c:326-333`) is one line of substance:

```c
	error = __vfs_getxattr(dentry, inode, XATTR_NAME_CAPS, NULL, 0);
```

On a FUSE inode that becomes `fuse_getxattr` (`fs/fuse/xattr.c:51`), which latches `fc->no_getxattr` only on `-ENOSYS`. lbfs answers `ENODATA` — the honest `fgetxattr` result — so the kernel repeats the probe on every single write, forever.

**The server answers that probe with the open/fstat/close triple the benchmark counted.** `LocalFs::xattr_fd` (`crates/lbfs-server/src/fs/local/mod.rs:425-442`) runs `fstat` on the node's `O_PATH` descriptor, then `reopen`, which is an `open(2)` of `/proc/self/fd/N` (`mod.rs:757-764`), and the `close` follows when the `Arc<OwnedFd>` drops. The `fgetxattr` itself rides the io_uring ring, so `strace` never shows it — which is exactly why the triple read as a `GETATTR`. Random reads show zero triples because the read path never calls `file_remove_privs`.

**`FUSE_HANDLE_KILLPRIV_V2` removes the probe.** `process_init_reply` sets both the connection bit and the superblock flag (`inode.c:1411-1414`):

```c
			if (flags & FUSE_HANDLE_KILLPRIV_V2) {
				fc->handle_killpriv_v2 = 1;
				fm->sb->s_flags |= SB_NOSEC;
			}
```

`SB_NOSEC` is the precondition `inode_has_no_xattr` checks (`include/linux/fs.h:3549-3553`), so the tail of `file_remove_privs_flags` (`fs/inode.c:2338-2339`) latches `S_NOSEC` on the inode after the first write, and every write after that returns at `fs/inode.c:2324` having sent nothing.

**Does the writeback cache cover `STATX_SIZE` locally, so the whole thing disappears?** Yes, and the answer holds both ways round. With the writeback cache on, `fuse_get_cache_mask` covers `STATX_SIZE` (`inode.c:322-325`), so the pre-write refresh stays local and the flag takes the write down to one round trip. With `--no-writeback` the kernel skips `fuse_update_attributes` altogether (`file.c:1482`), so the result is the same and slightly cleaner. Expect the identical win on both mount shapes.

**One residual round trip stays, on purpose.** `fuse_change_attributes_common` clears `S_NOSEC` on every attribute reply (`inode.c:307-315`), because another client could have set the bits behind the kernel's back. Each `GETATTR` or `LOOKUP` thus costs one extra `GETXATTR` on the next write. At `--ttl 1s` and 3400 writes per second that comes to roughly two extra frames per second, against 3400 today.

### 2. Feasibility — can fuser 0.15.1 reach bit 28? Yes.

This gate could have killed the approach, so check it first; the sources say it holds.

- **The bit fits.** `#define FUSE_HANDLE_KILLPRIV_V2 (1 << 28)` (`include/uapi/linux/fuse.h:480`, and `/usr/include/linux/fuse.h:476` on this host). Bit 28 sits below 32, so fuser's `u32` `fuse_init_in.flags` carries it. Nothing here needs `flags2` or `FUSE_INIT_EXT`, which fuser 0.15.1 lacks.
- **No minor-version check blocks it.** `process_init_reply` reads the bit inside a single `if (arg->minor >= 6)` (`inode.c:1335`) and applies it with no further guard (`inode.c:1411-1414`) — the same shape as `FUSE_ASYNC_DIO` at `inode.c:1373-1374`, which lbfs already exercises at a negotiated 7.31. The ABI-7.33 label on the flag describes the release that added it, not a runtime gate.
- **The kernel offers it unconditionally.** `fuse_new_init` puts `FUSE_HANDLE_KILLPRIV_V2` in the plain flag word at `inode.c:1505`.
- **fuser lacks the constant but accepts the raw bit.** `fuser::consts` stops at `FUSE_HANDLE_KILLPRIV` (`1 << 19`, `abi-7-26`, `src/ll/fuse_abi.rs:223`); no V2. `KernelConfig::add_capabilities` (`src/lib.rs:237-243`) compares the ask against `self.capabilities`, which holds the kernel's own `fuse_init_in.flags` verbatim (`src/ll/request.rs:977-979`, wired at `src/request.rs:156`). No whitelist of fuser-known names stands between the two, so a locally declared `1 << 28` passes.
- **Ruling: this works from fuser 0.15.1 with a local `const` plus `add_capabilities` — no fuser fork, no dependency bump.** For the record, the newest release is fuser 0.18.0, and it dropped the whole `abi-*` feature family, so moving there would rewrite the bridge's negotiation rather than bump a line. This plan stays on 0.15.1.
- The companion write flag needs no local constant at all: `fuser::consts::FUSE_WRITE_KILL_PRIV` is `1 << 2` behind `abi-7-31` (`src/ll/fuse_abi.rs:270-271`), which is the feature the workspace already enables (`Cargo.toml:35`), and `1 << 2` is the same bit the current uapi calls `FUSE_WRITE_KILL_SUIDGID` (`uapi/linux/fuse.h:527`, aliased at `:530`).

### 3. The contract lbfs takes on, and who fulfills each half

The uapi states the promise (`include/uapi/linux/fuse.h:429-433`): the filesystem kills suid, sgid and capabilities on write, chown and truncate; on write and truncate it kills suid and sgid only when the caller lacks `CAP_FSETID`; and it kills sgid only when the file carries group-execute permission.

The kernel signals each obligation:

| Obligation | Kernel signal | Source |
|---|---|---|
| write | `FUSE_WRITE_KILL_SUIDGID` in `fuse_write_in.write_flags` | `file.c:1205-1206` (page path), `file.c:1701-1703` (direct-I/O path) |
| truncate via `SETATTR` | `FATTR_KILL_SUIDGID` in `fuse_setattr_in.valid` | `dir.c:2231-2232` |
| chown via `SETATTR` | `FATTR_KILL_SUIDGID`, unconditional for non-directories | `dir.c:2221-2223` |
| `open(O_TRUNC)` | `FUSE_OPEN_KILL_SUIDGID` in `fuse_open_in.open_flags` | `file.c:38-41` |

Three notes on that table. lbfs never sees the `open` signal, because the client withholds `FUSE_ATOMIC_O_TRUNC` on purpose and truncation arrives as a `SETATTR` instead (`crates/lbfs-client/src/fuse.rs:345-349`). fuser exposes `write_flags: u32` on the `write` callback (`src/lib.rs:534-545`) but exposes nothing for `FATTR_KILL_SUIDGID` on `setattr` (`src/lib.rs:343-360`), so the server decides the truncate case on its own. And `file.c:1701-1703` sets the write flag whenever the caller lacks `CAP_FSETID`, with no `handle_killpriv_v2` condition, so lbfs's `O_DIRECT` writes already carry the bit today and the bridge already throws it away (`fuse.rs:862` names the parameter `_write_flags`).

**What the server does about it.** `vm/lbfs-server.service` runs the daemon as `User=ubuntu`, and cloud-init creates that user as an ordinary account (`vm/cloud-init/user-data.tmpl.yaml`). An unprivileged server holds no `CAP_FSETID`, and that single fact fulfills the whole contract with no code at all:

- **write** — the server's own `write(2)` runs `file_remove_privs_flags`, whose `setattr_should_drop_suidgid` returns a kill mask precisely because `capable(CAP_FSETID)` is false (`fs/attr.c:75`).
- **truncate** — `do_truncate` calls `dentry_needs_remove_privs` and folds the result into the `iattr`, same predicate.
- **chown** — `chown_common` sets `ATTR_KILL_SUID | ATTR_KILL_PRIV | setattr_should_drop_sgid(...)` for any non-directory (`fs/open.c:769-771`), with no capability check on the suid half, so `LocalFs`'s existing `chownat` (`mod.rs:861-870`) already covers this obligation and needs no new code.

The gap is a server that *does* hold `CAP_FSETID` — a root deployment, which the unit file does not produce but the binary permits. There the backing kernel skips the strip and lbfs would break a promise it made at `INIT`. To cover that case, the server picks a policy once at startup:

- **`Kernel`** (no `CAP_FSETID`): the backing kernel strips inside every syscall. Zero per-operation cost, and the hot path stays at exactly one syscall per write. This is what every benchmark and every test run measures.
- **`Explicit`** (holds `CAP_FSETID`): the server strips itself — one `statx` through the ring, then an `fchmod` only when a privileged bit is actually present.

**Why not virtiofsd's per-operation capability toggle.** virtiofsd drops `CAP_FSETID` from the effective set around the write, which works because it calls `pwrite` on the same thread. lbfs does not: the data-plane write is an SQE that the kernel executes either inline on the submitting task or on an `io-wq` worker, and `io-wq` workers are separate tasks created with `create_io_thread`, carrying a credential snapshot taken when the worker started rather than when the SQE lands. A toggle on a tokio thread would thus apply to some writes and skip others, depending on whether that particular write blocked. Nondeterministic security is worse than none, so this plan rejects the toggle. Read `crates/lbfs-server/src/fs/local/uring.rs` before disagreeing — the executor owns one ring thread and hands ownership of every buffer across it.

**Why not a permanent `CAP_FSETID` drop at startup.** It would restore the kernel strip for a root server in one line, but it also breaks an unrelated operation: `setattr_copy` clears `S_ISGID` from an explicit `chmod` when the process lacks `CAP_FSETID` and does not belong to the file's group. A root server exporting other users' trees would silently lose `chmod g+s`. This plan keeps the capability and pays for the strip only where it must.

**Race semantics this plan accepts.** Under `Explicit` the sequence is `statx`, then conditional `fchmod`, then the write — the VFS order, since `file_remove_privs_flags` runs before the copy. The three steps are not atomic. A `SETATTR` that re-sets `S_ISUID` between the `fchmod` and the write leaves a file holding both the new bytes and the privileged bit, for a window of one ring round trip. lbfs accepts that window: v1 exports to one client (spec §1), that client's own kernel serializes `write` against `setattr` on one inode under `i_rwsem`, and the outcome is strictly narrower than today's, where the server honors no strip contract at all.

**One obligation this plan narrows on purpose.** Under `Explicit` the server clears set-user-ID and set-group-ID but leaves `security.capability` in place. Finding out whether the attribute exists costs the same round trip this plan removes, and the client mounts with `nosuid` and `nodev` (`fuse.rs:438-439`), which disables file capabilities on that mount as thoroughly as it disables setuid. Task 1 records the narrowing in the spec so nobody rediscovers it as a bug.

**Applying the sgid rule.** The server follows the uapi wording — clear `S_ISGID` only when `S_IXGRP` is present — rather than the third branch of `setattr_should_drop_sgid` (`fs/attr.c:42-43`), which turns on whether the *caller* belongs to the file's group. The server does not know the caller's groups; v1 carries no credentials on the wire (spec §1 non-goals).

### 4. The independent cheap win, corrected

The brief expected `GETATTR` to cost an open/fstat/close triple. It does not. `LocalFs::getattr` (`mod.rs:921-927`) already resolves the node to its `O_PATH` descriptor and calls `statx_fd`, which is `AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW` through the ring's `statx` opcode (`mod.rs:330-339`), and `UringExecutor::statx` already exists with the `&Arc<OwnedFd>` convention (`uring.rs:283-309`). A `GETATTR` costs zero visible syscalls, which is exactly why the randread strace shows zero triples. Nothing needs fixing there.

The triple belongs to `LocalFs::xattr_fd` (`mod.rs:425-442`), and it has one removable third: the `fstat` exists solely to learn whether the node is a regular file or a directory. A file type never changes for a live inode, the node table already pins that inode with an `O_PATH` descriptor, and `lookup_impl` already runs a `statx` whose `stx_mode` carries the answer (`mod.rs:348-368`). Task 8 stores the type in the node and deletes the `fstat`. That leaves the reopen, which is inherent — `fgetxattr` refuses an `O_PATH` descriptor.

---

## File Map

| Path | Change |
|---|---|
| `docs/superpowers/specs/2026-08-20-lbfs-design.md` | version 2, `WRITE` flag, server kill-priv contract, client capability list |
| `crates/lbfs-proto/src/frame.rs` | `PROTOCOL_VERSION` becomes `2` |
| `crates/lbfs-proto/src/ops.rs` | `WriteRequest.kill_suidgid` |
| `crates/lbfs-client/src/fuse.rs` | `kill_suidgid` helper, `FUSE_HANDLE_KILLPRIV_V2` capability, `write` forwards the flag |
| `crates/lbfs-client/src/conn.rs` | `Connection::write` grows the flag |
| `crates/lbfs-client/src/bin/lbfs-bench.rs` | two call sites |
| `crates/lbfs-client/tests/mux.rs`, `tests/live.rs` | five call sites |
| `crates/lbfs-server/src/fs/mod.rs` | `FileSystem::write` grows the flag |
| `crates/lbfs-server/src/fs/local/killpriv.rs` | **new** — `KillPrivPolicy`, `stripped_mode` |
| `crates/lbfs-server/src/fs/local/mod.rs` | policy field, strip on write, strip on truncate, `xattr_fd` loses its `fstat` |
| `crates/lbfs-server/src/fs/local/nodes.rs` | `Node.file_type`, `NodeTable::file_type` |
| `crates/lbfs-server/src/rpc/dispatch.rs` | passes the flag through |
| `tests/src/lib.rs` | frame helper grows the flag |
| `tests/tests/loopback.rs` | suid/sgid stripping through a real mount |
| `Cargo.toml` | rustix gains the `thread` feature |
| `docs/benchmarks/2026-08-22-bottleneck-analysis.md` | records the measured result |

---

### Task 1: Spec — record the kill-priv contract and the `WRITE` flag

**Files:**
- Edit: `docs/superpowers/specs/2026-08-20-lbfs-design.md` (§3.1, §3.4, §5.3, §7, §11)

**Interfaces:**
- Consumes: nothing.
- Produces: the written contract every later task argues from. Names fixed here: wire field `kill_suidgid`, protocol version `2`, server policy names `Kernel` and `Explicit`.

- [ ] **Step 1: Bump the version sentence in §3.2**

Find this line in §3.2 step 1:

```text
1. `HELLO` (client → server): magic `LBFS`, protocol version (exact match
   required in v1; the field is the evolution mechanism), proposed limits.
```

Replace it with:

```text
1. `HELLO` (client → server): magic `LBFS`, protocol version — now `2`, and
   still an exact match, which is the whole point. Version `2` adds
   `kill_suidgid` to the `WRITE` body. postcard ignores trailing bytes, so a
   version-`1` server decoding a version-`2` `WRITE` would drop the flag and
   silently keep a set-user-ID bit the mount promised to clear. Refusing the
   handshake turns that into a startup failure an operator can see. Both ends
   deploy together, so the refusal costs nothing.
```

- [ ] **Step 2: Add the `WRITE` body note to §3.4**

Directly after the paragraph that begins `` `SETATTR` is a single op with an optional-field struct ``, add:

```text
`WRITE` carries one flag beside its `(node, fh, offset)` triple:
`kill_suidgid`, copied from the kernel's `FUSE_WRITE_KILL_SUIDGID`. The client
sets it whenever its kernel sets it; the server treats it as an instruction to
clear set-user-ID and set-group-ID before the bytes land. §5.3 says which side
performs the clearing.
```

- [ ] **Step 3: Add the kill-priv subsection to §5.3**

Append to §5.3, after the existing prose:

```text
**Killing privileged mode bits.** The client asks its kernel for
`FUSE_HANDLE_KILLPRIV_V2`, which stops the kernel probing
`security.capability` before every write and so removes one round trip per
write. In exchange the server owes the promise that flag encodes: clear
set-user-ID and set-group-ID on write, truncate and chown, clearing
set-group-ID only when the file also carries group-execute permission.

Who performs the clearing depends on one fact the server reads once at
startup — whether it holds `CAP_FSETID`.

* **`Kernel`** — no `CAP_FSETID`, which is how `vm/lbfs-server.service` runs it.
  The backing kernel clears the bits inside the server's own `write(2)`,
  `ftruncate(2)` and `fchown(2)`, so the server does nothing per operation and
  the write path stays at one syscall.
* **`Explicit`** — the server holds `CAP_FSETID`, so the backing kernel skips
  the strip and the server does it: one `statx`, then an `fchmod` only when a
  privileged bit is present. Truncate takes the same treatment. Chown needs no
  code either way, because `chown_common` clears the bits for every
  non-directory regardless of capability.

Two narrowings, both deliberate. The server leaves `security.capability`
alone: discovering whether it exists costs exactly the round trip this design
removes, and the client mounts `nosuid,nodev`, which disables file
capabilities on that mount. And the set-group-ID rule follows group-execute
alone rather than the caller's group membership, because v1 carries no caller
credentials on the wire.

Under `Explicit` the `statx`, `fchmod` and write are three steps rather than
one atomic action. A `SETATTR` racing between the second and the third leaves
new bytes under a set-user-ID bit for one round trip. v1 exports to a single
client whose kernel serializes those two operations per inode, and the result
is narrower than v1 shipped with, so the design accepts the window.
```

- [ ] **Step 4: Add the capability to §7**

In §7, find the sentence listing the capabilities the client requests and add `FUSE_HANDLE_KILLPRIV_V2` to it, then append:

```text
`FUSE_HANDLE_KILLPRIV_V2` is optional, not required. A kernel that refuses it
keeps today's behaviour: the kernel probes `security.capability` before each
write and performs its own strip through `SETATTR` (`fs/fuse/dir.c:2335`). The
mount stays correct and stays slow. Note that the kernel sets
`FUSE_WRITE_KILL_SUIDGID` on direct-I/O writes whether or not it granted the
capability, so a server honouring the flag on a mount that lost it strips more
often than the contract demands — never less.
```

- [ ] **Step 5: Trim the fast-follow in §11**

If §11 lists the per-write metadata round trip as future work, replace that bullet with:

```text
* **Server-side `security.capability` clearing under a privileged server.**
  `FUSE_HANDLE_KILLPRIV_V2` shipped without it (§5.3). A server holding
  `CAP_FSETID` clears set-user-ID and set-group-ID but leaves the capability
  attribute; picking it up means either a per-write probe or a per-node cache
  invalidated by `SETXATTR`.
```

If §11 has no such bullet, add the one above at its end.

- [ ] **Step 6: Check the prose gate**

Run: `git diff --stat docs/superpowers/specs/2026-08-20-lbfs-design.md`
Expected: one file changed, five hunks.

- [ ] **Step 7: Commit**

```bash
git add docs/superpowers/specs/2026-08-20-lbfs-design.md
git commit -m "docs(spec): protocol v2, WRITE kill_suidgid, server kill-priv contract"
```

---

### Task 2: Proto — `WriteRequest.kill_suidgid` and protocol version 2

**Files:**
- Edit: `crates/lbfs-proto/src/frame.rs`
- Edit: `crates/lbfs-proto/src/ops.rs`

**Interfaces:**
- Consumes: Task 1's spec wording.
- Produces: `PROTOCOL_VERSION: u32 = 2`; `WriteRequest { node: NodeId, fh: Fh, offset: u64, kill_suidgid: bool }`. Every later task builds that struct with all four fields.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `crates/lbfs-proto/src/ops.rs`:

```rust
    #[test]
    fn write_request_wire_stability_golden() {
        // Pins the postcard encoding: three u64 varints then a one-byte bool.
        // A version-1 peer decoding this body stops after the third varint and
        // drops the flag, which is why PROTOCOL_VERSION moved to 2.
        let set = WriteRequest {
            node: 1,
            fh: 2,
            offset: 3,
            kill_suidgid: true,
        };
        assert_eq!(postcard::to_allocvec(&set).unwrap(), vec![1, 2, 3, 1]);

        let clear = WriteRequest {
            node: 1,
            fh: 2,
            offset: 3,
            kill_suidgid: false,
        };
        assert_eq!(postcard::to_allocvec(&clear).unwrap(), vec![1, 2, 3, 0]);
    }

    #[test]
    fn write_request_round_trips_both_flag_states() {
        for kill_suidgid in [false, true] {
            round_trip(&WriteRequest {
                node: 9,
                fh: 4,
                offset: 1 << 40,
                kill_suidgid,
            });
        }
    }

    #[test]
    fn protocol_version_is_two() {
        // Version 1 bodies cannot carry the flag, and postcard ignores
        // trailing bytes rather than refusing them, so the handshake is the
        // only place that can catch a half-deployed pair.
        assert_eq!(crate::frame::PROTOCOL_VERSION, 2);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p lbfs-proto`
Expected: FAIL — `WriteRequest` has no field `kill_suidgid`, and `protocol_version_is_two` reports `1`.

- [ ] **Step 3: Write the code**

In `crates/lbfs-proto/src/frame.rs`, replace the version line:

```rust
/// Version 2 added `WriteRequest.kill_suidgid`. postcard ignores trailing
/// bytes instead of refusing them, so a version-1 peer would decode a
/// version-2 `WRITE` body cleanly and drop the flag — losing a set-user-ID
/// strip in silence. The exact-match handshake is what turns that into a
/// visible startup failure.
pub const PROTOCOL_VERSION: u32 = 2;
```

In `crates/lbfs-proto/src/ops.rs`, replace `WriteRequest`:

```rust
/// The request carries the payload in the frame's data segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteRequest {
    pub node: NodeId,
    pub fh: Fh,
    pub offset: u64,
    /// The kernel's `FUSE_WRITE_KILL_SUIDGID`, forwarded verbatim.
    ///
    /// True means the writer holds no `CAP_FSETID`, so the file must lose
    /// set-user-ID — and set-group-ID too when it carries group-execute —
    /// before these bytes land. Under `FUSE_HANDLE_KILLPRIV_V2` this is the
    /// only notice the server gets, because the kernel has stopped doing the
    /// strip itself.
    pub kill_suidgid: bool,
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p lbfs-proto`
Expected: PASS, including both golden byte strings.

Note: the rest of the workspace no longer compiles at this point — every `WriteRequest` literal is missing a field. Tasks 3 and 6 repair the client and the server; run `cargo test -p lbfs-proto` rather than `make check` until Task 6 lands.

- [ ] **Step 5: Commit**

```bash
git add crates/lbfs-proto/src/frame.rs crates/lbfs-proto/src/ops.rs
git commit -m "feat(proto)!: WRITE carries kill_suidgid, protocol version 2"
```

---

### Task 3: Client — forward the kernel's kill flag onto the wire

**Files:**
- Edit: `crates/lbfs-client/src/conn.rs` (`Connection::write`, around line 848)
- Edit: `crates/lbfs-client/src/fuse.rs` (imports at line 52, new helper, `write` at line 855)
- Edit: `crates/lbfs-client/src/bin/lbfs-bench.rs` (lines 208, 250)
- Edit: `crates/lbfs-client/tests/mux.rs` (lines 725, 873, 1132, 1140)
- Edit: `crates/lbfs-client/tests/live.rs` (lines 119, 228)
- Edit: `tests/src/lib.rs` (line 538)

**Interfaces:**
- Consumes: Task 2's `WriteRequest`.
- Produces: `Connection::write(&self, node: NodeId, fh: Fh, offset: u64, data: Vec<u8>, kill_suidgid: bool) -> Result<u32, Errno>`; `fn kill_suidgid(write_flags: u32) -> bool` in `fuse.rs`; the frame helper `Client::write(&mut self, node, fh, offset, data, kill_suidgid)` in `tests/src/lib.rs`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block at the bottom of `crates/lbfs-client/src/fuse.rs`:

```rust
    /// fuser names bit 2 `FUSE_WRITE_KILL_PRIV`; the current kernel uapi calls
    /// the same bit `FUSE_WRITE_KILL_SUIDGID` and keeps the older name as an
    /// alias. Pin the value so a fuser upgrade that renames it cannot silently
    /// change which bit the bridge reads.
    #[test]
    fn the_kill_flag_is_bit_two_and_nothing_else() {
        assert_eq!(fuser::consts::FUSE_WRITE_KILL_PRIV, 1 << 2);
        assert!(kill_suidgid(1 << 2));
        assert!(kill_suidgid(0xFFFF_FFFF));
        assert!(!kill_suidgid(0));
        // FUSE_WRITE_CACHE and FUSE_WRITE_LOCKOWNER must not read as a strip.
        assert!(!kill_suidgid(1 << 0));
        assert!(!kill_suidgid(1 << 1));
        assert!(!kill_suidgid((1 << 0) | (1 << 1)));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p lbfs-client the_kill_flag_is_bit_two`
Expected: FAIL — `cannot find function 'kill_suidgid' in this scope`.

- [ ] **Step 3: Add the helper and widen the import**

In `crates/lbfs-client/src/fuse.rs`, replace the `fuser::consts` import at lines 52-55:

```rust
use fuser::consts::{
    FOPEN_KEEP_CACHE, FUSE_ASYNC_DIO, FUSE_DO_READDIRPLUS, FUSE_READDIRPLUS_AUTO,
    FUSE_WRITE_KILL_PRIV, FUSE_WRITEBACK_CACHE,
};
```

Add the helper immediately after `fn open_flags()` (around line 412):

```rust
/// Whether this `WRITE` must clear set-user-ID and set-group-ID first.
///
/// The kernel sets `FUSE_WRITE_KILL_SUIDGID` — fuser's `FUSE_WRITE_KILL_PRIV`,
/// the same bit 2 — when the writing task holds no `CAP_FSETID`. On a mount
/// that negotiated `FUSE_HANDLE_KILLPRIV_V2` this bit is the *only* notice the
/// server gets, because the kernel has stopped clearing the bits through its
/// own `SETATTR`. Dropping it, which is what this bridge did before, means a
/// setuid binary in the export survives being overwritten.
///
/// The direct-I/O path sets the bit whether or not the kernel granted the
/// capability (`fs/fuse/file.c:1701-1703`), so forwarding it is safe on any
/// mount: at worst the server strips more often than the contract demands.
fn kill_suidgid(write_flags: u32) -> bool {
    write_flags & FUSE_WRITE_KILL_PRIV != 0
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p lbfs-client the_kill_flag_is_bit_two`
Expected: PASS.

- [ ] **Step 5: Widen `Connection::write`**

In `crates/lbfs-client/src/conn.rs`, replace the method at line 848:

```rust
    pub async fn write(
        &self,
        node: NodeId,
        fh: Fh,
        offset: u64,
        data: Vec<u8>,
        kill_suidgid: bool,
    ) -> Result<u32, Errno> {
        let (reply, _): (WriteReply, _) = self
            .call(
                Opcode::Write,
                &WriteRequest {
                    node,
                    fh,
                    offset,
                    kill_suidgid,
                },
                data,
            )
            .await?;
        Ok(reply.written)
    }
```

- [ ] **Step 6: Forward the flag from the callback**

In `crates/lbfs-client/src/fuse.rs`, replace the `write` callback body at lines 855-882:

```rust
    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let (conn, _) = self.ctx();
        let kill = kill_suidgid(write_flags);
        // The slice borrows the session's single receive buffer, which is
        // reused the moment this callback returns. The copy is what lets the
        // write outlive the callback.
        let data = data.to_vec();
        self.rt.spawn(async move {
            let Ok(offset) = u64::try_from(offset) else {
                reply.error(libc::EINVAL);
                return;
            };
            match conn.write(ino, fh, offset, data, kill).await {
                Ok(written) => reply.written(written),
                Err(e) => reply.error(errno(e)),
            }
        });
    }
```

- [ ] **Step 7: Repair the remaining call sites**

`crates/lbfs-client/src/bin/lbfs-bench.rs` line 208 becomes:

```rust
                conn.write(node, fh, at, block[..want as usize].to_vec(), false)
```

and line 250 becomes:

```rust
                        conn.write(node, fh, at, block.as_ref().clone(), false).await?;
```

`crates/lbfs-client/tests/live.rs` line 119 becomes:

```rust
    let written = conn.write(entry.node, fh, 3, b"!!".to_vec(), false).await.unwrap();
```

and line 228 becomes:

```rust
        conn.write(file.node, fh, 0, b"hello".to_vec(), false)
```

`crates/lbfs-client/tests/mux.rs` — add `, false` as the fifth argument at lines 725, 873, 1132 and 1140, so for example line 1132 reads:

```rust
    let err = conn.write(2, 7, 0, vec![0u8; 4097], false).await.unwrap_err();
```

`tests/src/lib.rs` line 538 becomes:

```rust
    pub async fn write(
        &mut self,
        node: NodeId,
        fh: Fh,
        offset: u64,
        data: &[u8],
        kill_suidgid: bool,
    ) -> Reply {
        self.call_data(
            Opcode::Write,
            &WriteRequest {
                node,
                fh,
                offset,
                kill_suidgid,
            },
            data,
        )
        .await
    }
```

Then add `, false` to every `.write(` call in `tests/tests/protocol.rs` — the compiler names each one.

- [ ] **Step 8: Run the client tests**

Run: `cargo test -p lbfs-client --lib`
Expected: PASS.

Note: `cargo test --workspace` still fails while the server has not caught up. Task 6 closes that.

- [ ] **Step 9: Commit**

```bash
git add crates/lbfs-client/src/conn.rs crates/lbfs-client/src/fuse.rs \
  crates/lbfs-client/src/bin/lbfs-bench.rs crates/lbfs-client/tests/mux.rs \
  crates/lbfs-client/tests/live.rs tests/src/lib.rs tests/tests/protocol.rs
git commit -m "feat(client): forward FUSE_WRITE_KILL_SUIDGID onto the wire"
```

---

### Task 4: Client — ask the kernel for `FUSE_HANDLE_KILLPRIV_V2`

**Files:**
- Edit: `crates/lbfs-client/src/fuse.rs` (constant, `capabilities()` at line 357, tests at line 1521)

**Interfaces:**
- Consumes: Task 3's bridge.
- Produces: `const FUSE_HANDLE_KILLPRIV_V2: u32 = 1 << 28;` in `fuse.rs`, and a `Capability` entry with `required: false`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/lbfs-client/src/fuse.rs`, beside the existing capability tests:

```rust
    /// The one capability this whole change exists for. Bit 28 per
    /// `include/uapi/linux/fuse.h`; fuser 0.15.1 has no constant for it, and
    /// `KernelConfig::add_capabilities` accepts any bit the kernel offered, so
    /// the local constant is the whole mechanism.
    #[test]
    fn killpriv_v2_is_always_requested_at_bit_twenty_eight() {
        assert_eq!(FUSE_HANDLE_KILLPRIV_V2, 1 << 28);
        for writeback in [true, false] {
            assert_eq!(
                requested(writeback) & FUSE_HANDLE_KILLPRIV_V2,
                FUSE_HANDLE_KILLPRIV_V2
            );
        }
    }

    /// A kernel that refuses it leaves a correct, slower mount, so refusing to
    /// mount over it would be an over-reaction. Only the writeback cache earns
    /// that, because only the writeback cache changes what the server already
    /// promised at HELLO.
    #[test]
    fn killpriv_v2_is_optional() {
        for writeback in [true, false] {
            let cap = capabilities(writeback)
                .into_iter()
                .find(|c| c.bit == FUSE_HANDLE_KILLPRIV_V2)
                .expect("the capability list carries it");
            assert!(!cap.required);
            assert_eq!(cap.name, "FUSE_HANDLE_KILLPRIV_V2");
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p lbfs-client killpriv_v2`
Expected: FAIL — `cannot find value 'FUSE_HANDLE_KILLPRIV_V2' in this scope`.

- [ ] **Step 3: Declare the constant**

In `crates/lbfs-client/src/fuse.rs`, add immediately above `fn capabilities(` (around line 357):

```rust
/// `FUSE_HANDLE_KILLPRIV_V2`, which fuser 0.15.1 does not name.
///
/// fuser's `consts` stops at `FUSE_HANDLE_KILLPRIV` (bit 19). Bit 28 arrived
/// with ABI 7.33 and fuser negotiates at most 7.31 — but the kernel does not
/// check the minor version for this flag. `process_init_reply` reads it inside
/// one `if (arg->minor >= 6)` and applies it with no further guard
/// (`fs/fuse/inode.c:1411-1414`), exactly as it does for `FUSE_ASYNC_DIO`, and
/// `fuse_new_init` offers it unconditionally (`inode.c:1505`). The value fits
/// in `u32`, so fuser's `fuse_init_in.flags` carries it without the `flags2`
/// extension fuser lacks, and `KernelConfig::add_capabilities` checks the ask
/// against the kernel's own offered bits rather than a list of names fuser
/// knows. A locally declared constant is therefore the whole mechanism.
const FUSE_HANDLE_KILLPRIV_V2: u32 = 1 << 28;
```

- [ ] **Step 4: Add the capability**

In `crates/lbfs-client/src/fuse.rs`, inside `capabilities()`, append this entry to the `vec![...]` after the `FUSE_ASYNC_DIO` entry:

```rust
        // The reason a write costs two round trips without it. The kernel
        // probes `security.capability` before every write to decide whether it
        // must strip set-user-ID (`cap_inode_need_killpriv`,
        // `security/commoncap.c:326-333`), and lbfs answers ENODATA, which the
        // kernel never latches. This flag sets `SB_NOSEC` on the superblock
        // (`fs/fuse/inode.c:1411-1414`), the inode latches `S_NOSEC` after the
        // first write, and every write after that short-circuits with no
        // request at all. The price is a promise: the server clears the bits
        // instead. See spec §5.3 and `fs::local::killpriv`.
        Capability {
            bit: FUSE_HANDLE_KILLPRIV_V2,
            name: "FUSE_HANDLE_KILLPRIV_V2",
            required: false,
        },
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p lbfs-client killpriv_v2`
Expected: PASS, both cases.

- [ ] **Step 6: Commit**

```bash
git add crates/lbfs-client/src/fuse.rs
git commit -m "feat(client): request FUSE_HANDLE_KILLPRIV_V2 to drop the per-write xattr probe"
```

---

### Task 5: Server — the kill-priv policy module

**Files:**
- Create: `crates/lbfs-server/src/fs/local/killpriv.rs`
- Edit: `crates/lbfs-server/src/fs/local/mod.rs` (module line only)
- Edit: `Cargo.toml` (rustix `thread` feature)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub enum KillPrivPolicy { Kernel, Explicit }` with `KillPrivPolicy::detect() -> KillPrivPolicy`, and `pub fn stripped_mode(mode: u32) -> Option<u32>`. Tasks 6 and 7 call both.

- [ ] **Step 1: Add the rustix feature**

In the workspace `Cargo.toml`, change the rustix line:

```toml
# `thread` is here for `capabilities()`: the server reads its own CAP_FSETID
# once at startup to learn whether the backing kernel will strip set-user-ID
# on its behalf. See `fs::local::killpriv`.
rustix = { version = "1", features = ["fs", "net", "process", "thread"] }
```

- [ ] **Step 2: Write the failing tests**

Create `crates/lbfs-server/src/fs/local/killpriv.rs` holding only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_modes_need_no_strip() {
        assert_eq!(stripped_mode(libc::S_IFREG | 0o644), None);
        assert_eq!(stripped_mode(libc::S_IFREG | 0o777), None);
        assert_eq!(stripped_mode(libc::S_IFREG | 0o000), None);
    }

    #[test]
    fn setuid_always_goes() {
        assert_eq!(
            stripped_mode(libc::S_IFREG | 0o4755),
            Some(libc::S_IFREG | 0o0755)
        );
        // No execute bits anywhere, and set-user-ID still goes.
        assert_eq!(
            stripped_mode(libc::S_IFREG | 0o4644),
            Some(libc::S_IFREG | 0o0644)
        );
    }

    /// The uapi rule (`include/uapi/linux/fuse.h:429-433`): set-group-ID dies
    /// only when the file carries group execute. Without it the bit is a
    /// mandatory-locking marker, and clearing it would change unrelated
    /// semantics.
    #[test]
    fn setgid_goes_only_with_group_execute() {
        assert_eq!(
            stripped_mode(libc::S_IFREG | 0o2755),
            Some(libc::S_IFREG | 0o0755)
        );
        assert_eq!(stripped_mode(libc::S_IFREG | 0o2644), None);
    }

    #[test]
    fn both_bits_go_together() {
        assert_eq!(
            stripped_mode(libc::S_IFREG | 0o6755),
            Some(libc::S_IFREG | 0o0755)
        );
        // Set-user-ID goes, set-group-ID stays: no group execute.
        assert_eq!(
            stripped_mode(libc::S_IFREG | 0o6745),
            Some(libc::S_IFREG | 0o2745)
        );
    }

    /// `setattr_should_drop_suidgid` guards its whole result on
    /// `S_ISREG(mode)` (`fs/attr.c:75`). Directories keep set-group-ID because
    /// that bit means inheritance there, not privilege.
    #[test]
    fn only_regular_files_lose_bits() {
        assert_eq!(stripped_mode(libc::S_IFDIR | 0o2775), None);
        assert_eq!(stripped_mode(libc::S_IFDIR | 0o4755), None);
        assert_eq!(stripped_mode(libc::S_IFLNK | 0o6777), None);
    }

    /// The suite runs unprivileged, so the backing kernel is the actor and the
    /// server does nothing per write. A run that reports `Explicit` here means
    /// the tests run as root, and the write-path assertions below would then
    /// measure the other branch.
    #[test]
    fn an_unprivileged_process_leaves_the_work_to_the_kernel() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        assert_eq!(KillPrivPolicy::detect(), KillPrivPolicy::Kernel);
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p lbfs-server killpriv`
Expected: FAIL — the module is not declared, so nothing compiles.

- [ ] **Step 4: Write the code**

Prepend to `crates/lbfs-server/src/fs/local/killpriv.rs`, above the test module:

```rust
//! Who clears set-user-ID and set-group-ID, and what "clear" means exactly.
//!
//! Once the client negotiates `FUSE_HANDLE_KILLPRIV_V2`, its kernel stops
//! probing `security.capability` before every write — one round trip per write
//! saved — and stops performing the strip through its own `SETATTR`. The
//! promise moves here.
//!
//! Most of the time this module does nothing per operation, and that is the
//! design rather than an oversight. `vm/lbfs-server.service` runs the daemon as
//! an ordinary user, and an unprivileged process cannot skip the kernel's own
//! strip: `setattr_should_drop_suidgid` returns a kill mask exactly when
//! `capable(CAP_FSETID)` is false (`fs/attr.c:75`), and the server's `write(2)`
//! and `ftruncate(2)` both run through it. Chown needs nothing from anybody —
//! `chown_common` sets `ATTR_KILL_SUID | ATTR_KILL_PRIV` for every
//! non-directory with no capability check at all (`fs/open.c:769-771`).
//!
//! A server holding `CAP_FSETID` is the case that needs code, because there the
//! kernel steps aside and the promise would go unkept.
//!
//! # Why not virtiofsd's toggle
//!
//! virtiofsd drops `CAP_FSETID` from its effective set around the write and
//! puts it back after. That works because virtiofsd calls `pwrite` on the
//! thread that dropped it. lbfs submits an SQE instead, and the kernel runs it
//! either inline or on an `io-wq` worker — a separate task whose credentials
//! were copied when the worker started, not when the SQE landed. A per-request
//! toggle would therefore apply to some writes and skip others depending on
//! whether that write happened to block, which is a worse failure than not
//! trying.

use rustix::thread::{capabilities, CapabilitySet};

/// Which side of the syscall boundary clears the privileged mode bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillPrivPolicy {
    /// The server holds no `CAP_FSETID`, so the backing kernel clears the bits
    /// inside the server's own syscalls. Nothing to do per operation, and the
    /// write path stays at one syscall.
    Kernel,
    /// The server holds `CAP_FSETID`, so the backing kernel skips the strip and
    /// the server performs it.
    Explicit,
}

impl KillPrivPolicy {
    /// Reads the effective capability set once, at startup.
    ///
    /// A capability set can only shrink without `CAP_SETPCAP`, and lbfs never
    /// raises one, so reading it once is honest for the life of the process.
    /// A failing `capget` picks the strict branch: doing redundant work is a
    /// cost, and skipping a promised strip is a hole.
    pub fn detect() -> KillPrivPolicy {
        match capabilities(None) {
            Ok(sets) if !sets.effective.contains(CapabilitySet::FSETID) => KillPrivPolicy::Kernel,
            _ => KillPrivPolicy::Explicit,
        }
    }
}

/// The mode a strip would leave behind, or `None` when nothing needs clearing.
///
/// Mirrors `setattr_should_drop_suidgid` (`fs/attr.c:63-79`) with one
/// deliberate narrowing. The kernel's `setattr_should_drop_sgid`
/// (`fs/attr.c:33-45`) also clears set-group-ID from a file whose group the
/// *caller* does not belong to, even with no group-execute bit. v1 carries no
/// caller credentials on the wire, so the server cannot evaluate that branch
/// and follows the rule the FUSE uapi actually states
/// (`include/uapi/linux/fuse.h:429-433`): group execute, or nothing.
///
/// `None` for anything that is not a regular file, matching the `S_ISREG`
/// guard at `fs/attr.c:75`. Set-group-ID on a directory means inheritance, not
/// privilege.
pub fn stripped_mode(mode: u32) -> Option<u32> {
    if mode & libc::S_IFMT != libc::S_IFREG {
        return None;
    }
    let mut out = mode;
    if mode & libc::S_ISUID != 0 {
        out &= !libc::S_ISUID;
    }
    if mode & libc::S_ISGID != 0 && mode & libc::S_IXGRP != 0 {
        out &= !libc::S_ISGID;
    }
    (out != mode).then_some(out)
}
```

In `crates/lbfs-server/src/fs/local/mod.rs`, add the module beside the existing ones near the top:

```rust
pub mod killpriv;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p lbfs-server killpriv`
Expected: PASS — six cases.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/lbfs-server/src/fs/local/killpriv.rs \
  crates/lbfs-server/src/fs/local/mod.rs
git commit -m "feat(server): kill-priv policy and the VFS strip rule"
```

---

### Task 6: Server — honor the flag on `WRITE`

**Files:**
- Edit: `crates/lbfs-server/src/fs/mod.rs` (`FileSystem::write`)
- Edit: `crates/lbfs-server/src/fs/local/mod.rs` (`LocalFs` field, constructors, `write`, tests)
- Edit: `crates/lbfs-server/src/rpc/dispatch.rs` (line 225)

**Interfaces:**
- Consumes: Tasks 2 and 5.
- Produces: `FileSystem::write(&self, node: NodeId, fh: Fh, offset: u64, data: PooledBuf, len: u32, kill_suidgid: bool) -> FsResult<u32>`; `LocalFs::killpriv` field; `LocalFs::strip_privileged_bits(&self, fd: &Arc<OwnedFd>) -> FsResult<()>`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block at the bottom of `crates/lbfs-server/src/fs/local/mod.rs`:

```rust
    /// The end of the promise, from the backend's side.
    ///
    /// Under `Kernel` the backing kernel does this inside the server's own
    /// `write(2)` and the test proves the outcome rather than the mechanism —
    /// which is the right thing to pin, because the mechanism switches with the
    /// server's capabilities and the outcome must not.
    #[tokio::test]
    async fn a_flagged_write_clears_set_user_id() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let path = dir.path().join("suid");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o4755))
            .unwrap();

        let e = fs.lookup(ROOT_NODE, b"suid").await.unwrap();
        let fh = fs.open(e.node, libc::O_WRONLY as u32).await.unwrap();
        let mut buf = fs.pool_for_test().get();
        buf.as_mut_slice()[..3].copy_from_slice(b"new");
        buf.set_len(3);
        assert_eq!(fs.write(e.node, fh, 0, buf, 3, true).await.unwrap(), 3);

        let mode = std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&path).unwrap());
        assert_eq!(mode & 0o7777, 0o0755, "set-user-ID survived a flagged write");
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    /// Set-group-ID with no group execute is a mandatory-locking marker, and
    /// neither the kernel nor the server may clear it.
    #[tokio::test]
    async fn a_flagged_write_keeps_a_mandatory_locking_marker() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let path = dir.path().join("mand");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o2644))
            .unwrap();

        let e = fs.lookup(ROOT_NODE, b"mand").await.unwrap();
        let fh = fs.open(e.node, libc::O_WRONLY as u32).await.unwrap();
        let mut buf = fs.pool_for_test().get();
        buf.as_mut_slice()[..3].copy_from_slice(b"new");
        buf.set_len(3);
        fs.write(e.node, fh, 0, buf, 3, true).await.unwrap();

        let mode = std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&path).unwrap());
        assert_eq!(mode & 0o7777, 0o2644);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p lbfs-server a_flagged_write`
Expected: FAIL — `this method takes 5 arguments but 6 arguments were supplied`.

- [ ] **Step 3: Widen the trait**

In `crates/lbfs-server/src/fs/mod.rs`, replace the `write` signature:

```rust
    /// `kill_suidgid` carries the kernel's `FUSE_WRITE_KILL_SUIDGID`: the
    /// writer holds no `CAP_FSETID`, so the file loses set-user-ID — and
    /// set-group-ID when it also carries group execute — before these bytes
    /// land. Under `FUSE_HANDLE_KILLPRIV_V2` the client's kernel does nothing
    /// about it, so this flag is the only notice a backend receives.
    async fn write(
        &self,
        node: NodeId,
        fh: Fh,
        offset: u64,
        data: PooledBuf,
        len: u32,
        kill_suidgid: bool,
    ) -> FsResult<u32>;
```

- [ ] **Step 4: Give `LocalFs` a policy and a strip**

In `crates/lbfs-server/src/fs/local/mod.rs`, add the import beside the existing ones:

```rust
use crate::fs::local::killpriv::{stripped_mode, KillPrivPolicy};
```

Add the field to the `LocalFs` struct, after `root_key`:

```rust
    /// Whether this process must clear set-user-ID itself, decided once at
    /// construction. See [`killpriv`].
    killpriv: KillPrivPolicy,
```

In `from_root_fd`, add `killpriv: KillPrivPolicy::detect(),` to the `LocalFs { ... }` literal, and log the choice once so a root deployment says so out loud — add this line immediately before the `Ok(LocalFs {`:

```rust
        let killpriv = KillPrivPolicy::detect();
        tracing::info!(
            policy = ?killpriv,
            "set-user-ID stripping policy chosen from this process's CAP_FSETID"
        );
```

then use `killpriv,` in the struct literal.

Add the helper to the `impl LocalFs` block, right after `statx_fd`:

```rust
    /// Clear set-user-ID and set-group-ID before a write lands.
    ///
    /// Runs only under [`KillPrivPolicy::Explicit`]; the caller checks. The
    /// order matches the VFS, where `file_remove_privs_flags`
    /// (`fs/inode.c:2317-2341`) runs before the copy rather than after it, so
    /// a crash between the two steps leaves the old bytes under a safe mode
    /// rather than new bytes under a privileged one.
    ///
    /// `fd` is the handle's own descriptor — a real open file, not the node's
    /// `O_PATH` — so `fchmod` takes it directly and no `/proc` detour is
    /// needed. `statx` first, because the common case has nothing to clear and
    /// an unconditional `fchmod` would bump `ctime` on every write.
    async fn strip_privileged_bits(&self, fd: &Arc<OwnedFd>) -> FsResult<()> {
        let st = self.statx_fd(fd).await.map_err(errno)?;
        let Some(mode) = stripped_mode(u32::from(st.stx_mode)) else {
            return Ok(());
        };
        let fd = Arc::clone(fd);
        tokio::task::spawn_blocking(move || {
            rustix::fs::fchmod(&*fd, rustix::fs::Mode::from_bits_truncate(mode & 0o7777))
                .map_err(rustix_errno)
        })
        .await
        .map_err(join_errno)?
    }
```

- [ ] **Step 5: Apply it in `LocalFs::write`**

Replace the head of `LocalFs::write`, from the signature through the `let fd` line:

```rust
    async fn write(
        &self,
        node: NodeId,
        fh: Fh,
        offset: u64,
        data: PooledBuf,
        len: u32,
        kill_suidgid: bool,
    ) -> FsResult<u32> {
        let fd = self.file_fd(node, fh)?;
        // Under `Kernel` this whole branch is dead code and the backing kernel
        // does the strip inside the `write(2)` below — which is the case every
        // deployment and every benchmark runs, so the hot path pays nothing.
        if kill_suidgid && self.killpriv == KillPrivPolicy::Explicit {
            self.strip_privileged_bits(&fd).await?;
        }
```

Leave the rest of the method unchanged.

- [ ] **Step 6: Pass the flag through dispatch**

In `crates/lbfs-server/src/rpc/dispatch.rs`, replace line 225:

```rust
            match fs
                .write(req.node, req.fh, req.offset, buf, len, req.kill_suidgid)
                .await
            {
```

- [ ] **Step 7: Repair the existing backend tests**

Add `, false` as the sixth argument to each `fs.write(` call in `crates/lbfs-server/src/fs/local/mod.rs` — lines 1862, 1895, 2144, 2173, 2212, 2236 and 2271. For example line 1862 becomes:

```rust
        assert_eq!(fs.write(entry.node, fh, 0, buf, 5, false).await.unwrap(), 5);
```

- [ ] **Step 8: Run the whole gate**

Run: `make check`
Expected: PASS. This is the first point since Task 2 where the workspace compiles end to end.

- [ ] **Step 9: Commit**

```bash
git add crates/lbfs-server/src/fs/mod.rs crates/lbfs-server/src/fs/local/mod.rs \
  crates/lbfs-server/src/rpc/dispatch.rs
git commit -m "feat(server): clear set-user-ID on a flagged write"
```

---

### Task 7: Server — honor the contract on truncate

**Files:**
- Edit: `crates/lbfs-server/src/fs/local/mod.rs` (`apply_setattr` at line 849, `setattr` at line 929, tests)

**Interfaces:**
- Consumes: Task 5's `stripped_mode` and `KillPrivPolicy`.
- Produces: `apply_setattr(fd: &OwnedFd, write_fd: Option<&OwnedFd>, args: &SetattrArgs, killpriv: KillPrivPolicy) -> Result<(), Errno>`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/lbfs-server/src/fs/local/mod.rs`:

```rust
    /// Truncate carries the same obligation as write
    /// (`include/uapi/linux/fuse.h:429-433`), and fuser exposes no
    /// `FATTR_KILL_SUIDGID` on its `setattr` callback, so the server decides
    /// this one on its own rather than reading a wire flag.
    #[tokio::test]
    async fn a_truncate_clears_set_user_id() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        let path = dir.path().join("suid");
        std::fs::write(&path, b"0123456789").unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o4755))
            .unwrap();

        let e = fs.lookup(ROOT_NODE, b"suid").await.unwrap();
        let attr = fs
            .setattr(
                e.node,
                SetattrArgs {
                    mode: None,
                    uid: None,
                    gid: None,
                    size: Some(4),
                    atime: TimeSet::Omit,
                    mtime: TimeSet::Omit,
                    fh: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(attr.size, 4);
        assert_eq!(attr.mode & 0o7777, 0o0755);
        let mode = std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&path).unwrap());
        assert_eq!(mode & 0o7777, 0o0755, "set-user-ID survived a truncate");
    }

    /// A `SETATTR` that sets a mode and no size keeps what the caller asked
    /// for. Only write and truncate carry the strip obligation; `chmod u+s` is
    /// a legitimate request and must survive.
    #[tokio::test]
    async fn a_chmod_without_a_truncate_keeps_set_user_id() {
        let (dir, fs) = test_fs(FsyncPolicy::Honor).await;
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        let e = fs.lookup(ROOT_NODE, b"f").await.unwrap();

        let attr = fs
            .setattr(
                e.node,
                SetattrArgs {
                    mode: Some(0o4755),
                    uid: None,
                    gid: None,
                    size: None,
                    atime: TimeSet::Omit,
                    mtime: TimeSet::Omit,
                    fh: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(attr.mode & 0o7777, 0o4755);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p lbfs-server a_truncate_clears a_chmod_without`
Expected: `a_chmod_without_a_truncate_keeps_set_user_id` passes; `a_truncate_clears_set_user_id` passes too when the suite runs unprivileged, because the backing kernel already strips. Record that result — that run exercises the `Kernel` branch, and Step 3 adds the `Explicit` branch the same test covers under root.

- [ ] **Step 3: Widen `apply_setattr`**

In `crates/lbfs-server/src/fs/local/mod.rs`, change the signature at line 849:

```rust
fn apply_setattr(
    fd: &OwnedFd,
    write_fd: Option<&OwnedFd>,
    args: &SetattrArgs,
    killpriv: KillPrivPolicy,
) -> Result<(), Errno> {
```

Add this block immediately before the `if let Some(size) = args.size {` step, so it sits where `do_truncate` puts its own call to `dentry_needs_remove_privs` — after an explicit mode change, before the resize:

```rust
    // Truncate carries the same set-user-ID obligation as write
    // (`include/uapi/linux/fuse.h:429-433`). Under `Kernel` the server's own
    // `ftruncate` already does it, because `do_truncate` folds
    // `dentry_needs_remove_privs` into the `iattr` and that predicate turns on
    // the caller lacking `CAP_FSETID`. Under `Explicit` the kernel steps aside
    // and this is the strip.
    if killpriv == KillPrivPolicy::Explicit && args.size.is_some() {
        let st = rustix::fs::statx(
            fd,
            "",
            rustix::fs::AtFlags::EMPTY_PATH,
            rustix::fs::StatxFlags::MODE,
        )
        .map_err(rustix_errno)?;
        if let Some(mode) = stripped_mode(u32::from(st.stx_mode)) {
            // The node descriptor is `O_PATH`, which `fchmod` refuses, so this
            // one goes through `/proc` like the explicit chmod above.
            rustix::fs::chmod(
                proc_path(fd),
                rustix::fs::Mode::from_bits_truncate(mode & 0o7777),
            )
            .map_err(rustix_errno)?;
        }
    }
```

Update the doc comment above `apply_setattr` so its numbered order still reads true — insert a step between 2 and 3:

```rust
/// 3. **the truncate strip**, when this process must perform it, positioned
///    exactly where `do_truncate` performs the kernel's;
/// 4. **size**, which is the only step needing write access;
/// 5. **times** last, so an explicit timestamp beats the implicit `mtime`
///    bump a truncate just caused.
```

- [ ] **Step 4: Pass the policy in**

In `LocalFs::setattr`, replace the `spawn_blocking` call:

```rust
        let owned = Arc::clone(&fd);
        let killpriv = self.killpriv;
        tokio::task::spawn_blocking(move || {
            apply_setattr(&owned, write_fd.as_deref(), &args, killpriv)
        })
        .await
        .map_err(join_errno)??;
```

- [ ] **Step 5: Run the gate**

Run: `make check`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/lbfs-server/src/fs/local/mod.rs
git commit -m "feat(server): clear set-user-ID on truncate under a privileged server"
```

---

### Task 8: Server — the node table remembers the file type

**Files:**
- Edit: `crates/lbfs-server/src/fs/local/nodes.rs`
- Edit: `crates/lbfs-server/src/fs/local/mod.rs` (`from_root_fd`, `lookup_impl`, `xattr_fd`)

**Interfaces:**
- Consumes: nothing from earlier tasks; independently reviewable.
- Produces: `NodeTable::new(root_fd: OwnedFd, root_key: FileKey, file_type: u32)`, `NodeTable::register(fd: OwnedFd, key: FileKey, file_type: u32) -> (NodeId, u64, Arc<OwnedFd>)`, `NodeTable::file_type(&self, node: NodeId) -> Option<u32>`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/lbfs-server/src/fs/local/nodes.rs`:

```rust
    /// `xattr_fd` needs the file type and nothing else about the mode, and a
    /// live inode never changes type. Storing it at registration means the
    /// xattr path stops paying for an `fstat` it can answer from memory.
    #[test]
    fn a_node_remembers_its_file_type() {
        let (dir, table) = table_over_tempdir();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();

        let ffd = open_path(&dir.path().join("f"));
        let (fid, _, _) = table.register(ffd, key_of(&open_path(&dir.path().join("f"))), libc::S_IFREG);
        let dfd = open_path(&dir.path().join("d"));
        let (did, _, _) = table.register(dfd, key_of(&open_path(&dir.path().join("d"))), libc::S_IFDIR);

        assert_eq!(table.file_type(fid), Some(libc::S_IFREG));
        assert_eq!(table.file_type(did), Some(libc::S_IFDIR));
        assert_eq!(table.file_type(ROOT_NODE), Some(libc::S_IFDIR));
        assert_eq!(table.file_type(9999), None);
    }
```

Change `table_over_tempdir` in the same module to pass the root's type:

```rust
    fn table_over_tempdir() -> (tempfile::TempDir, NodeTable) {
        let dir = tempfile::tempdir().unwrap();
        let root = open_path(dir.path());
        let key = key_of(&root);
        (dir, NodeTable::new(root, key, libc::S_IFDIR))
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p lbfs-server a_node_remembers_its_file_type`
Expected: FAIL — `this function takes 2 arguments but 3 arguments were supplied`.

- [ ] **Step 3: Write the code**

In `crates/lbfs-server/src/fs/local/nodes.rs`, add the field to `Node`:

```rust
struct Node {
    fd: Arc<OwnedFd>,
    key: FileKey,
    generation: u64,
    /// `st_mode & S_IFMT`, captured when the node was first registered.
    ///
    /// A live inode never changes type — no syscall exists that would do it —
    /// and the `O_PATH` descriptor above pins that inode for as long as the
    /// node lives, so this value cannot go stale. Permission bits are *not*
    /// stored, because `SETATTR` changes those and nothing here would notice.
    file_type: u32,
    /// The kernel's lookup count for this id, decremented by `FUSE_FORGET`.
    nlookup: u64,
}
```

Change `NodeTable::new`:

```rust
    /// Installs `root_fd` as `ROOT_NODE` with an immortal lookup count.
    pub fn new(root_fd: OwnedFd, root_key: FileKey, file_type: u32) -> NodeTable {
        let mut nodes = HashMap::new();
        let mut by_key = HashMap::new();
        nodes.insert(
            ROOT_NODE,
            Node {
                fd: Arc::new(root_fd),
                key: root_key,
                generation: 0,
                file_type,
                nlookup: u64::MAX,
            },
        );
        by_key.insert(root_key, ROOT_NODE);
        NodeTable(Mutex::new(Inner {
            nodes,
            by_key,
            next_id: ROOT_NODE + 1,
            next_generation: 1,
        }))
    }
```

Change `register`'s signature to `pub fn register(&self, fd: OwnedFd, key: FileKey, file_type: u32) -> (NodeId, u64, Arc<OwnedFd>)` and add `file_type,` to the `Node { ... }` literal it inserts. The dedup branch stays untouched: an existing node already carries the type, and the two agree because they name one inode.

Add the accessor after `get`:

```rust
    /// The stored `S_IFMT` bits, or `None` for an id this table never issued
    /// or has already forgotten.
    pub fn file_type(&self, node: NodeId) -> Option<u32> {
        let g = self.0.lock().unwrap();
        g.nodes.get(&node).map(|n| n.file_type)
    }
```

Add `, libc::S_IFREG` to the `register` calls in the existing tests of that module — `register_get_forget_lifecycle`, `hardlinks_dedup_to_one_node_with_bumped_refcount`, `generations_differ_when_id_slot_recycles_a_key` and `a_recycled_key_after_full_forget_gets_a_fresh_id_and_generation`.

- [ ] **Step 4: Feed the type from both registration sites**

In `crates/lbfs-server/src/fs/local/mod.rs`, in `from_root_fd`, replace the `NodeTable::new` call:

```rust
            nodes: NodeTable::new(root, root_key, u32::from(st.stx_mode) & libc::S_IFMT),
```

In `lookup_impl`, replace the `register` call:

```rust
        let (node, generation, _fd) = self
            .nodes
            .register(owned, key, u32::from(st.stx_mode) & libc::S_IFMT);
```

- [ ] **Step 5: Drop the `fstat` from `xattr_fd`**

Replace the body of `xattr_fd`, keeping its existing doc comment and adding one paragraph to it:

```rust
    /// The type check reads the node table rather than the filesystem. A live
    /// inode never changes type, so the value `lookup_impl` already captured
    /// answers this question exactly, and the xattr path drops one syscall per
    /// operation. That matters more than it looks: on a mount without
    /// `FUSE_HANDLE_KILLPRIV_V2`, the kernel probes `security.capability`
    /// before every single write.
    async fn xattr_fd(&self, node: NodeId) -> FsResult<Arc<OwnedFd>> {
        let fd = self.node_fd(node)?;
        match self.nodes.file_type(node) {
            Some(t) if t == libc::S_IFREG || t == libc::S_IFDIR => {}
            Some(_) => return Err(EOPNOTSUPP),
            // The descriptor above came out of the same table, so a miss here
            // means a FORGET landed between the two reads.
            None => return Err(Errno::ESTALE),
        }
        // Blocking: `reopen` is an `open(2)`, which has no io_uring opcode that
        // takes a `/proc` path.
        let opened = tokio::task::spawn_blocking(move || {
            reopen(&fd, rustix::fs::OFlags::RDONLY).map_err(errno)
        })
        .await
        .map_err(join_errno)??;
        Ok(Arc::new(opened))
    }
```

- [ ] **Step 6: Run the gate**

Run: `make check`
Expected: PASS, including the existing `getxattr_probes_the_length_and_refuses_a_short_buffer` and the symlink and FIFO xattr cases, which still see `EOPNOTSUPP`.

- [ ] **Step 7: Commit**

```bash
git add crates/lbfs-server/src/fs/local/nodes.rs crates/lbfs-server/src/fs/local/mod.rs
git commit -m "perf(server): node table remembers the file type, xattr_fd drops its fstat"
```

---

### Task 9: Loopback — set-user-ID stripping through a real mount

**Files:**
- Edit: `tests/tests/loopback.rs`

**Interfaces:**
- Consumes: Tasks 3, 4, 6 and 7.
- Produces: `fn privileged_bits_die_on_write(writeback: bool)` plus two `#[test]` wrappers, following the file's existing `file_content_round_trips(writeback: bool)` shape.

- [ ] **Step 1: Write the failing test**

Add to `tests/tests/loopback.rs`, beside the `file_content_round_trips` pair:

```rust
/// The promise `FUSE_HANDLE_KILLPRIV_V2` buys, checked end to end.
///
/// Asking the kernel for that capability tells it to stop clearing set-user-ID
/// itself, which is worth one round trip per write and worth nothing at all if
/// the bits then survive. So this walks the whole path: chmod through the
/// mount, write through the mount, and read the mode off the export directly,
/// behind the mount's back.
///
/// Both writeback settings, because the kernel reaches the wire flag by two
/// different routes. With the cache on, `fuse_cache_write_iter` sees a file
/// needing a strip and switches to the write-through path so the flag can ride
/// a synchronous request (`fs/fuse/file.c:1489-1491`, `file.c:1205-1206`). With
/// it off, `fuse_perform_write` gets there directly.
fn privileged_bits_die_on_write(writeback: bool) {
    let lb = Loopback::start(Opts {
        writeback,
        ..Opts::default()
    });
    lb.wait_ready();

    let seen = lb.mnt().join("suid");
    let real = lb.export().join("suid");

    std::fs::write(&seen, b"old").unwrap();
    std::fs::set_permissions(&seen, std::os::unix::fs::PermissionsExt::from_mode(0o4755)).unwrap();
    assert_eq!(
        std::fs::metadata(&real).unwrap().mode() & 0o7777,
        0o4755,
        "the chmod did not reach the export"
    );

    std::fs::write(&seen, b"new").unwrap();

    assert_eq!(
        std::fs::metadata(&real).unwrap().mode() & 0o7777,
        0o0755,
        "set-user-ID survived a write through the mount"
    );
    assert_eq!(std::fs::read(&real).unwrap(), b"new");

    // Set-group-ID with group execute goes the same way; without it the bit is
    // a mandatory-locking marker and stays.
    let exec = lb.mnt().join("sgid-exec");
    let exec_real = lb.export().join("sgid-exec");
    std::fs::write(&exec, b"old").unwrap();
    std::fs::set_permissions(&exec, std::os::unix::fs::PermissionsExt::from_mode(0o2775)).unwrap();
    std::fs::write(&exec, b"new").unwrap();
    assert_eq!(std::fs::metadata(&exec_real).unwrap().mode() & 0o7777, 0o0775);

    let mark = lb.mnt().join("sgid-mand");
    let mark_real = lb.export().join("sgid-mand");
    std::fs::write(&mark, b"old").unwrap();
    std::fs::set_permissions(&mark, std::os::unix::fs::PermissionsExt::from_mode(0o2664)).unwrap();
    std::fs::write(&mark, b"new").unwrap();
    assert_eq!(std::fs::metadata(&mark_real).unwrap().mode() & 0o7777, 0o2664);

    // Truncate carries the same obligation as write.
    let trunc = lb.mnt().join("suid-trunc");
    let trunc_real = lb.export().join("suid-trunc");
    std::fs::write(&trunc, b"0123456789").unwrap();
    std::fs::set_permissions(&trunc, std::os::unix::fs::PermissionsExt::from_mode(0o4755)).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&trunc)
        .unwrap()
        .set_len(4)
        .unwrap();
    assert_eq!(std::fs::metadata(&trunc_real).unwrap().mode() & 0o7777, 0o0755);
    assert_eq!(std::fs::metadata(&trunc_real).unwrap().len(), 4);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn privileged_bits_die_on_write_with_the_writeback_cache() {
    privileged_bits_die_on_write(true);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn privileged_bits_die_on_write_without_the_writeback_cache() {
    privileged_bits_die_on_write(false);
}
```

Add `use std::os::unix::fs::PermissionsExt;` to the imports if `from_mode` does not already resolve. The file already imports `MetadataExt` at line 58, which supplies `mode()`.

- [ ] **Step 2: Run the tests to verify they behave**

Run: `cargo test -p lbfs-tests --test loopback privileged_bits -- --ignored --test-threads=1`
Expected: PASS. Run this on the *pre-change* tree too if you want the contrast: it passes there as well, because an unprivileged server's own kernel does the strip. What it guards is the future — a `KillPrivPolicy` bug, a dropped wire flag, or a server that gains `CAP_FSETID`.

- [ ] **Step 3: Run the whole loopback suite**

Run: `make test-loopback`
Expected: PASS, no regressions in the existing cases.

- [ ] **Step 4: Commit**

```bash
git add tests/tests/loopback.rs
git commit -m "test(loopback): set-user-ID and set-group-ID die on write and truncate"
```

---

### Task 10: Measure on the VM pair and record the result

**Files:**
- Edit: `docs/benchmarks/2026-08-22-bottleneck-analysis.md`

**Interfaces:**
- Consumes: every task above.
- Produces: the acceptance evidence. No automated test — this one needs two guests and a quiet machine.

- [ ] **Step 1: Build and deploy**

Run: `make build-guest && make vm-deploy`
Expected: `deployed.` with `lbfs-server` active. If the pair is down, `make vm-up` first.

- [ ] **Step 2: Confirm the kernel granted the capability**

Run on the client guest, after mounting:

```bash
sudo journalctl -u lbfs-server -n 20 --no-pager | grep -i 'stripping policy'
```

Expected: `policy=Kernel`, because the unit runs as `ubuntu`.

Then mount and check the client's own log has no refusal line:

```bash
lbfs-client 192.168.77.10:9423 /srv/exports/data /mnt/lbfs 2>&1 | grep -i killpriv
```

Expected: no output. A line reading `capability=FUSE_HANDLE_KILLPRIV_V2 unsupported by this kernel` means the kernel refused the bit and the rest of this task measures nothing.

- [ ] **Step 3: Count the server's syscalls under a write load**

On the server guest, with a 4 KiB random-write job running against the mount from the client:

```bash
sudo timeout 12 strace -c -f -p "$(pgrep -x lbfs-server)"
```

Expected: `openat`, `fstat` and `close` counts near zero — a handful for connection setup, not thousands. Compare against `writev`, which should now sit near one per write rather than two. Before this change the same window showed 6430 triples against 12872 `writev` calls.

- [ ] **Step 4: Measure the four shapes**

Run the drained single-job driver used for the tables in the benchmark document: 4 KiB random write psync QD1, 4 KiB random read psync QD1, 4 KiB random read libaio QD16, and 1 MiB sequential read psync — each with `direct=1`, each after draining the server's dirty pages (`sync`, then poll `/proc/meminfo` until `Dirty + Writeback` falls under 8 MB).

Expected:

| job | before | after |
|---|---|---|
| randwrite 4k psync QD1 | 3365 IOPS, 296 µs | ~4800 IOPS, ~205 µs |
| randread 4k psync QD1 | 8322 IOPS, 119.3 µs | unchanged within run-to-run spread |
| randread 4k libaio QD16 | 40290 IOPS, 393 µs | unchanged within run-to-run spread |
| seq read 1M psync | 1580 MB/s, 632 µs | unchanged within run-to-run spread |

- [ ] **Step 5: Record it**

Append a section to `docs/benchmarks/2026-08-22-bottleneck-analysis.md`:

```text
## Phase 8: the per-write probe, removed

The extra round trip per write was a `GETXATTR` of `security.capability`, not
a `GETATTR`. The kernel issues it from `file_remove_privs`
(`fs/inode.c:2317-2341`) before every write on an inode that lacks `S_NOSEC`,
and a FUSE superblock only gains `SB_NOSEC` when the server negotiates
`FUSE_HANDLE_KILLPRIV_V2` (`fs/fuse/inode.c:1411-1414`). lbfs answers the probe
with ENODATA, which the kernel never latches, so it repeated forever. The
server's `xattr_fd` reopens the node through `/proc` to run `fgetxattr`, and
that reopen is the open/fstat/close triple the earlier window counted.

Asking for the flag removes the probe; honouring the promise it encodes moves
the set-user-ID strip onto the server. Numbers, same drained single-job driver:

[table from Step 4]

Syscall counts over a 12 s randwrite window: [counts from Step 3].
```

Correct the earlier "The per-write `GETATTR`" section's closing paragraph to point at the new section rather than leaving the mislabelled conclusion standing.

- [ ] **Step 6: Commit**

```bash
git add docs/benchmarks/2026-08-22-bottleneck-analysis.md
git commit -m "docs(bench): per-write security.capability probe removed"
```

---

## Acceptance Criteria

1. `make check` passes: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
2. `make test-loopback` passes, including both new `privileged_bits_die_on_write` cases.
3. Through the VM mount, 4 KiB random write psync QD1 drops from 296 µs to roughly 205 µs — the write round trip alone, with the second one gone.
4. `strace -c -f` on `lbfs-server` during a 4 KiB randwrite window shows `openat`/`fstat`/`close` counts near zero rather than one per write, and reply frames near one per write rather than two.
5. 4 KiB random read psync QD1, 4 KiB random read libaio QD16, and 1 MiB sequential read psync all land inside their existing run-to-run spread.
6. A file with mode `4755` in the export loses its set-user-ID bit when a client writes to it or truncates it; a file with mode `2664` keeps its set-group-ID bit through the same operations.
7. The client log carries no `FUSE_HANDLE_KILLPRIV_V2 unsupported by this kernel` line on a Linux 7.0 guest.

## Open Risks

- **The capability could go ungranted on some other kernel.** Nothing in v7.0 blocks it, and `fuse_new_init` offers it unconditionally, but a kernel built without it, or a future one that adds a minor-version check, would leave the mount correct and unchanged in speed. Step 2 of Task 10 is the check; the `required: false` entry is the fallback.
- **The measured win could fall short of 90 µs.** The arithmetic in the existing analysis prices a metadata round trip at 92 µs using the raw-RPC read, and the `GETXATTR` costs more than that because of the reopen and the `spawn_blocking` hop. If the measurement lands well under 90 µs, the probe was not the only extra request and the frame-to-triple ratio in Task 10 Step 3 will say so.
- **`Explicit` mode ships with no test coverage on an unprivileged runner.** The `stripped_mode` table test covers the rule, and the loopback test covers the outcome, but the `Explicit` branch itself only executes for a root server. Running `make test-loopback` under `sudo` once, by hand, is the way to exercise it; the suite cannot demand root.
- **`security.capability` survives under a privileged server.** Recorded in the spec as a narrowing rather than a bug, and defensible because the client mounts `nosuid`. A deployment that runs the server as root *and* relies on file capabilities inside the export would want the follow-up in §11.
- **The protocol version bump makes a partial deploy fail loudly.** That is the intent, but `vm/deploy.sh` pushes the server first and the client second, so a mount attempted between the two steps refuses with a version mismatch. Deploy both, then mount.
- **`fuse_change_attributes_common` clears `S_NOSEC` on every attribute reply**, so a workload that interleaves `stat` with writes pays one probe per `stat` rather than none. Metadata-heavy write loads will see a smaller win than fio's pure write loop.

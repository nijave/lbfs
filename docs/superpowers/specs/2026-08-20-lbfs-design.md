# lbfs — Network-Proxied FUSE Filesystem: Design

Date: 2026-08-20
Status: Approved pending final review

## 1. Overview

lbfs is a client–server network filesystem. The client is a Rust FUSE daemon
that proxies every VFS operation over TCP to a server, which executes the
operation against a local filesystem and returns the result. The model is
NFS-like: the client requests a server-side path at attach time, and the
server grants access only if that path matches a configured allowlist of glob
patterns.

### Goals

- Correct POSIX-ish semantics for the v1 operation set (below).
- An efficient wire protocol: pipelined, binary, zero framework overhead on
  the hot path, no payload copies beyond those inherent to sockets and FUSE.
- Fully asynchronous server I/O, using io_uring wherever the kernel provides
  an opcode.
- A hard internal boundary between the RPC/network layer and the filesystem
  backend, so backends and policy layers can swap in without touching
  transport code.
- A reproducible VM-based test environment with independently swappable
  kernels.

### Non-goals for v1 (with named extension points)

- Authentication/authorization. The trust model is "a network you would run
  plaintext NFS on." mTLS later = wrap the accepted TCP stream in rustls
  before the handshake; authorization = a `FileSystem` decorator (§5.2).
- Cache coherence when more than one client shares an export. We assume one
  remotely attached client per path but do not enforce it.
- POSIX byte-range locks (`flock`/`fcntl`) — intentionally deferred.
- `MKNOD` and `ACCESS` return `ENOSYS` (see §7 for the `ACCESS` rationale).
- Transparent reconnection — a fast-follow, see §11.

### Operating assumptions

- Workload: metadata-heavy (source trees, build artifacts) without penalizing
  general-purpose use; large-file streaming is uncommon but should sustain
  ~1 GiB/s when it happens.
- Network: LAN or cloud VPC; ≤0.25 ms p50 RTT, ≥1 Gbit/s (typically much
  more).
- Both endpoints are Linux. Server kernel must be recent enough for the
  io_uring metadata opcodes (Ubuntu 26.04 LTS qualifies).

## 2. Repository and Workspace Layout

```
lbfs/
├── Cargo.toml            # cargo workspace
├── Makefile              # check / build / vm-* targets
├── crates/
│   ├── lbfs-proto/       # wire contract only: no I/O, no tokio
│   ├── lbfs-server/      # binary: rpc module + fs module
│   └── lbfs-client/      # binary: fuser bridge + multiplexer
├── tests/                # cross-crate integration tests (no VM)
├── vm/                   # libvirt test environment (§9)
└── docs/superpowers/specs/
```

Tooling from day one: `rustfmt` (default config) and `clippy` with
`-D warnings` via workspace lint tables; `make check` runs
`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
`cargo test`. These are part of initial setup, not a later add.

## 3. Wire Protocol

### 3.1 Transport and framing

TCP with `TCP_NODELAY`; one connection per mount in v1. Nothing in the wire
format is connection-scoped, leaving room for NVMe-style
one-connection-per-core scaling later. All integers little-endian.

Every message is one frame:

```
0        8        10       12       16       20       24
├────────┼────────┼────────┼────────┼────────┼────────┤
│ request_id: u64 │op/state│ flags  │body_len│data_len│ reserved: u32
│                 │  u16   │  u16   │  u32   │  u32   │
└─────────────────┴────────┴────────┴────────┴────────┘
│ body: postcard-encoded op-specific struct (body_len bytes)
│ data: raw payload (data_len bytes)
```

- `request_id`: client-assigned correlation ID (`AtomicU64`). Responses may
  arrive in any order.
- `op/status`: opcode in requests. In responses: `0` = OK, `1..4096` = Linux
  errno, `>= 0xFF00` = protocol statuses (`VERSION_MISMATCH`,
  `ATTACH_DENIED`, `NOT_EXPORTED`).
- `flags`: bit 0 = `NO_REPLY` (used by FORGET). We reserve bit 1 now for the
  future `FORCE_SYNC` control flag (§11), so adding it later breaks nothing.
- `body`: postcard-encoded per-op struct. Small, metadata only.
- `data`: bulk payload — present only on `WRITE` requests, `READ`/`GETXATTR`/
  `LISTXATTR` responses, and `SETXATTR` requests. Never passes through the
  serializer: senders emit it with vectored writes directly from the source
  buffer; the server reads it directly into a pooled buffer, and the client
  reads each reply's payload into a fresh buffer — a deliberate v1 choice,
  since fuser's reply API copies the slice either way.

Protocol violations that desynchronize the stream (lengths exceeding
negotiated maxima, unknown opcodes, window overflow, malformed
HELLO/ATTACH/FORGET bodies) are connection-fatal: log and close. A
malformed body on a filesystem opcode inside a frame with honest lengths
keeps the stream in sync, so the server answers `EINVAL` and the session
survives. The protocol has no in-band error recovery.

### 3.2 Handshake and attach

1. `HELLO` (client → server): magic `LBFS`, proposed limits, and a protocol
   version — now `2`, and still an exact match, which is the whole point.
   Version `2` adds `kill_suidgid` to the `WRITE` body. postcard ignores
   trailing bytes, so a version-`1` server decoding a version-`2` `WRITE`
   would drop the flag and silently keep a set-user-ID bit the mount
   promised to clear. Refusing the handshake turns that into a startup
   failure an operator can see. Both ends deploy together, so the refusal
   costs nothing.
2. `HELLO` reply: settled protocol version, **max in-flight window**
   (default 128, clamped to [8, 1024]), **max I/O size** (default 1 MiB,
   matches FUSE `max_write`), max body size (64 KiB — bounds xattr values
   and readdir batches).
3. `ATTACH` (client → server): desired absolute server path as bytes. Server
   opens the path `O_PATH | O_DIRECTORY`, reads the descriptor's true
   resolved path from `/proc/self/fd/N`, matches that resolved path against
   the allowlist, and replies with root attributes. Open-then-verify, never
   match-then-open: matching before opening leaves a window where a
   component swapped to a symlink exports the wrong root. Root `NodeId` is 1
   (`FUSE_ROOT_ID`).
4. Only after a successful `ATTACH` does the client complete the FUSE mount.

The in-flight window bounds server memory (window × max frame) and gives the
client backpressure beyond TCP. At 0.25 ms RTT, ~4 in-flight 1 MiB reads
sustain 1 GiB/s; 128 leaves ample depth for metadata bursts.

### 3.3 Node and handle model

- `NodeId` (u64): server-assigned, session-scoped, paired with a
  `generation` (u64) to detect reuse; mirrors FUSE's inode protocol.
- `Fh` / `Dh` (u64): open file/directory handles from `OPEN`/`CREATE`/
  `OPENDIR`.
- The client batches `FORGET` frames; they carry `NO_REPLY` and decrement
  server-side node refcounts.
- **Directory listings snapshot at `OPENDIR`.** The server reads the whole
  directory once and the handle keeps that list, so `READDIR` and
  `READDIRPLUS` page through a listing that no later create or unlink
  disturbs. `rewinddir` replays the snapshot: offset 0 restarts the same
  list. Every other offset must be a cookie this handle handed out, and one
  the server never issued answers `EINVAL` rather than truncating the
  listing in silence.

### 3.4 Opcodes

| Group | Ops |
|---|---|
| Session | `HELLO`, `ATTACH` |
| Metadata | `LOOKUP`, `FORGET` (batched), `GETATTR`, `SETATTR`, `STATFS` |
| Namespace | `MKDIR`, `UNLINK`, `RMDIR`, `RENAME` (`NOREPLACE`/`EXCHANGE` flags → `renameat2`), `SYMLINK`, `READLINK`, `LINK` |
| File I/O | `OPEN`, `CREATE`, `READ`, `WRITE`, `FLUSH`, `RELEASE`, `FSYNC`, `FALLOCATE`, `LSEEK` (`SEEK_DATA`/`SEEK_HOLE`), `COPY_FILE_RANGE` (server-side copy; data never crosses the wire) |
| Directory | `OPENDIR`, `READDIR`, `READDIRPLUS` (entries with full attributes — one round trip stats a directory), `RELEASEDIR`, `FSYNCDIR` |
| Xattr | `GETXATTR`, `SETXATTR`, `LISTXATTR`, `REMOVEXATTR` |

`SETATTR` is a single op with an optional-field struct (mode, uid, gid,
size, atime, mtime, fh), covering chmod/chown/truncate/utimens exactly as
FUSE does.

`WRITE` carries one flag beside its `(node, fh, offset)` triple:
`kill_suidgid`, copied from the kernel's `FUSE_WRITE_KILL_SUIDGID`. The client
sets it whenever its kernel sets it; the server treats it as an instruction to
clear set-user-ID and set-group-ID before the bytes land. §5.3 says which side
performs the clearing.

Filenames, symlink targets, and xattr names travel as raw byte strings
(`OsStr` semantics) — never UTF-8-validated. A filesystem proxy must
round-trip whatever the backing filesystem holds.

### 3.5 Design lineage

The frame layout deliberately copies the pattern shared by NFS (`xid`),
SMB3 (`MessageId` + credits), 9p (`tag`), NBD (`cookie`), iSCSI (Initiator
Task Tag + `CmdSN` window), NVMe-oF (CID + queue depth), and FUSE itself
(`unique`): fixed binary header, correlation ID for out-of-order completion,
windowed flow control, bulk data out-of-band from the header encoding.
We evaluated and rejected general-purpose RPC frameworks (tarpc, Cap'n
Proto, gRPC) for the hot path; the analysis lives in the project
conversation log — summary: at 1 GiB/s the serde payload copy is affordable
(~0.1 core), but we judged owning the buffer lifecycle end-to-end (pooling,
vectored I/O, future registered buffers) and the wire format worth a few
hundred lines of correlation plumbing.

## 4. Server: RPC Layer (`lbfs-server::rpc`)

- Tokio accept loop; each connection gets a **session**: reader task
  (parses frames, enforces the window, spawns one tokio task per request),
  writer task (sole socket writer; responses arrive on an mpsc channel), and
  session state (negotiated limits, export root, node table). Out-of-order
  completion falls out of the task-per-request model — a slow `READ` never
  blocks `GETATTR`s behind it.
- The server accepts concurrent connections from day one. Session state
  (including the node table) is per-connection.
- Dispatch is a thin match: decode body → call `FileSystem` method → encode
  reply. The RPC layer knows nothing about how ops execute.

Configuration (TOML):

```toml
listen = "0.0.0.0:9423"
allowed_paths = ["/srv/exports/*", "/home/*/shared"]
max_inflight = 128
max_io_size = "1MiB"
fsync = "honor"        # or "ignore" — see §6
```

Allowlist: `globset` patterns matched against the **descriptor's resolved
path** — open `O_PATH | O_DIRECTORY` first, read `/proc/self/fd/N`, then
match — so a symlink swapped between check and open cannot smuggle a
path past the allowlist. The fd the server verified is the fd it exports.

## 5. Server: Filesystem Layer (`lbfs-server::fs`)

### 5.1 The `FileSystem` trait — the RPC↔storage boundary

One async method per protocol op, speaking plain Rust types — no frames, no
sockets:

```rust
#[async_trait]
trait FileSystem: Send + Sync {
    async fn lookup(&self, parent: NodeId, name: &OsStr) -> Result<Entry, Errno>;
    async fn read(&self, node: NodeId, fh: Fh, off: u64, buf: PooledBuf)
        -> Result<PooledBuf, Errno>;
    // ... one per op
}
```

### 5.2 Extension points

- Alternative backends: new `FileSystem` implementations.
- Authorization (future): a decorator wrapping any `FileSystem`, checking
  per-op policy against an identity established at connection time (e.g.,
  from a client certificate once mTLS lands).
- mTLS (future): rustls wrap of the accepted TCP stream before `HELLO`; one
  function boundary in `rpc`.

### 5.3 `LocalFs` (modeled on virtiofsd)

- **Node table:** `NodeId → { O_PATH fd, (dev, ino), generation, nlookup }`
  plus reverse map `(dev, ino) → NodeId` so hardlinks dedup to one node.
  `FORGET` decrements; fd closes at zero. Fd-relative operation means
  server-side renames don't break held nodes and no full path walks occur.
- **Path safety:** FUSE lookups are single-component by construction; every
  one is `openat2(parent_fd, name, RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`.
  The server never follows symlinks — `READLINK` returns the target and the
  client kernel resolves it. Escapes are structurally impossible, not
  filtered.
- **io_uring:** a `UringExecutor` — N dedicated threads, each owning a ring;
  async callers submit an op descriptor and await a oneshot. In v1 N is a
  constant in the source and no config key reaches it.
  Ring-routed: read, write, fsync, fallocate, openat2, statx, unlinkat,
  mkdirat, renameat, linkat, symlinkat, xattr ops. `spawn_blocking`-routed
  (no uring opcode exists): getdents, readlinkat, copy_file_range, lseek.
  Either way every trait method is async and never blocks the runtime; uring
  coverage grows as kernels add opcodes.
- **Buffers:** a pool of boxed slices sized to `max_io_size` — plain heap
  buffers, since dropping `O_DIRECT` from v1 removed the alignment
  constraint that would have called for anything else. WRITE
  payloads: socket → pooled buffer → ring → pool. READs: pool → ring →
  vectored socket write → pool. No allocation on the data path (groundwork
  for registered buffers later).

**Killing privileged mode bits.** The client asks its kernel for
`FUSE_HANDLE_KILLPRIV_V2`, which stops the kernel probing
`security.capability` before every write and so removes one round trip per
write. In exchange the server owes the promise that flag encodes: clear
set-user-ID and set-group-ID on write, truncate and chown, clearing
set-group-ID only when the file also carries group-execute permission.

Who performs the clearing depends on one fact the server reads once at
startup — whether it holds `CAP_FSETID`.

- **`Kernel`** — no `CAP_FSETID`, which is how `vm/lbfs-server.service` runs
  it. The backing kernel clears the bits inside the server's own `write(2)`,
  `ftruncate(2)` and `fchown(2)`, so the server does nothing per operation and
  the write path stays at one syscall.
- **`Explicit`** — the server holds `CAP_FSETID`, so the backing kernel skips
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

## 6. Durability Policy

`LocalFs` option `fsync = "honor" | "ignore"` (default `honor`):

- `honor`: `FSYNC`/`FSYNCDIR` execute real `fsync`/`fdatasync`; durability
  means durable on server disk.
- `ignore`: the server acknowledges `FSYNC`/`FSYNCDIR` immediately without
  touching disk, **and** masks `O_SYNC`/`O_DSYNC` out of `OPEN`/`CREATE`
  flags (otherwise sync-opened files would reintroduce the latency the
  option exists to remove). The same trade as an NFS `async` export: latency
  for crash-durability. Documented loudly.

Writes otherwise land in the server page cache (no `O_DIRECT` in v1).
A future control message will force a real sync regardless of this setting;
§3.1 reserves frame flag bit 1 for it now (§11).

## 7. Client (`lbfs-client`)

- **fuser ↔ tokio bridge:** the fuser session loop runs on its own thread;
  every callback captures the op parameters and owned `Reply` (both `Send`)
  and hands them to the tokio runtime, returning immediately. One FUSE
  dispatch thread keeps arbitrarily many ops in flight; FUSE concurrency
  maps 1:1 onto protocol pipelining.
- **Multiplexer:** writer task owns the socket write half (vectored writes);
  reader task completes a correlation table `request_id → oneshot`. The
  negotiated window is a semaphore acquired before send. `FORGET`s skip the
  table (no reply); the client batches them, flushing on count or timer.
- **Caching (all kernel-side, justified by the one-client assumption):**
  `entry_timeout`/`attr_timeout` default 1 s, both CLI-tunable (0 disables),
  and `entry_timeout` defaults to whatever `attr_timeout` is. The split
  reaches `ReplyEntry` replies only — `CREATE` and `READDIRPLUS` carry one
  lifetime and send it as both;
  **writeback cache** on (kernel aggregates small writes — the biggest win
  for build workloads); `keep_cache` so re-reads stay local; `readdirplus`
  on; `max_write`/`max_readahead` = negotiated max I/O size.
- **Kill-priv capability:** beside those cache flags the client requests one
  more capability at FUSE `INIT`: `FUSE_HANDLE_KILLPRIV_V2` (§5.3).

  `FUSE_HANDLE_KILLPRIV_V2` is optional, not required. A kernel that refuses
  it keeps today's behavior: the kernel probes `security.capability` before
  each write and performs its own strip through `SETATTR`
  (`fs/fuse/dir.c:2335`). The mount stays correct and stays slow. Note that
  the kernel sets `FUSE_WRITE_KILL_SUIDGID` on direct-I/O writes regardless
  of whether it granted the capability, so a server honoring the flag on a
  mount that lost it strips more often than the contract demands — never
  less.
- **Identity:** ownership, mode, and times pass through exactly as the
  server sees them (NFS-without-idmapping). `st_ino` inside the mount is
  the server's `NodeId`, not the backing inode — fuser derives the FUSE
  nodeid from `attr.ino`, and a wrong nodeid means ESTALE, so the node id
  wins. Hardlink identity survives because the server dedups nodes on
  `(st_dev, st_ino)`. Plain `READDIR` `d_ino` still carries the backing
  inode, so `ls -i` can disagree with `stat`. The kernel enforces
  permissions locally via `default_permissions` (hence server-side
  `ACCESS` is `ENOSYS`).
  `chown`/`chmod` pass through and succeed or fail by the server process's
  privilege. This is what mTLS-derived identity later replaces.
- **Lifecycle:** `connect → HELLO → ATTACH → mount`; pre-mount failures are
  clean CLI errors. SIGINT/SIGTERM: unmount, drain, exit. CLI:
  `lbfs-client <server:port> <remote-path> <mountpoint> [--attr-timeout N]
  [--entry-timeout N] [--allow-other] [--auto-unmount] [--no-writeback]
  [--fuse-threads N] [--fuse-clone-fd]`.
- **Connection loss:** all in-flight and later ops fail `EIO`; the mount
  stays present and cleanly unmountable. No transparent reconnect in v1
  (node/handle state is session-scoped server-side; honest reconnection
  needs session resumption — the first fast-follow, §11).

## 8. Error Handling and Edge Cases

- **Errno passthrough:** `LocalFs` maps `io::Error` → raw Linux errno →
  status field → FUSE reply unchanged. No invented taxonomy. Attach failures
  use distinct protocol statuses so the CLI can say "path not exported"
  instead of bare `EACCES`.
- **Staleness:** generation-checked `NodeId`s; requests against forgotten or
  recycled nodes return `ESTALE`. Server restart ⇒ connection drop ⇒ `EIO`
  until remount (until reconnection lands).
- **Validation before allocation:** `body_len`/`data_len` checked against
  negotiated maxima before any buffer use; violations are connection-fatal.
  Pooled buffers cap peer-driven allocation. Full adversarial hardening is
  deferred to the mTLS milestone.
- **I/O semantics:** server loops on short writes (partial-then-error
  reports bytes written if any, else the error); short reads only at EOF;
  `RENAME` flags pass to `renameat2` preserving atomicity.
- **Hangs:** aggressive TCP keepalive (~10 s) turns a dead server into a
  detected disconnect. No per-request timeouts in v1. FUSE `INTERRUPT` is
  accepted and ignored (standard for network FUSE; honoring interrupts on
  non-idempotent ops invites correctness bugs).
- **Accepted gaps (documented):** attrs stale up to `attr_timeout` under
  server-side mutation; no coherence for concurrent clients on one export;
  `FORGET`s lost at unmount are moot — the server drops the whole node table
  when the connection closes.

## 9. VM Test Environment (`vm/`)

Two libvirt/KVM guests from the Ubuntu 26.04 LTS cloud image —
`lbfs-server` and `lbfs-client` — on a libvirt NAT network with cloud-init
static IPs (host ssh access + archive reachability). Lifecycle targets:

```
make vm-up       # fetch base image (once), qcow2 overlays, virt-install both
make vm-deploy   # build + copy binaries/configs into guests
make vm-test     # run e2e suite over ssh
make vm-down     # destroy domains + overlays (base image kept)
```

**Swappable kernels, per-VM:**

1. **Direct kernel boot** (primary): libvirt `<kernel>/<initrd>/<cmdline>`;
   host-side `vm/kernels/<name>/` holds vmlinuz+initrd pairs;
   `make vm-up KERNEL=<name>` boots any kernel — including custom builds for
   io_uring experiments — without touching the guest image.
2. **In-guest kernels** via apt (`linux-generic`, HWE, mainline PPA) +
   GRUB default, for exactly-as-shipped distro kernels.

Client and server kernels are independent, so version asymmetry is testable.

**Deployment:** guest binaries come from a container build
(`make build-guest`: podman + a Debian-based rust image, gnu target,
`target/guest`). Debian's older glibc runs forward-compatibly on the
Ubuntu guests (max required symbol GLIBC_2.39 vs guest 2.43 — the client
reached 2.39 when the pure-Rust mount moved the `fusermount3` exec into
Rust, whose spawn path imports `pidfd_spawnp`). Neither
binary links anything past libc: the io-uring crate is direct syscalls,
and from fuser 0.16.0 the client mounts through the crate's pure-Rust
path, running `fusermount3` from the guests' `fuse3` package instead of
linking `libfuse3.so`. The original static-musl plan died on one host
fact that still holds: Fedora's packaged Rust ships no musl std.

**The `fuser` pin is exact on purpose.** `Cargo.toml` reads
`fuser = { version = "=0.18.0" }`, not a caret range. Releases after 0.18.0
come primarily from a coding agent with, in the maintainer's own words, "at
least a cursory review from a human", and the project stopped accepting pull
requests in July 2026 — so the only routes for anything upstream does not
choose to build are an issue and a fork. For a filesystem client a protocol
bug is a data bug, so the practice is: pin the exact version, read the release
diff before moving it (`src/ll/request.rs`, `src/ll/reply.rs` and the
`add_capabilities` list, where every hazard found so far has lived), land the
crate bump and any ABI-declaration change as separate commits, and run the
set-user-ID loopback cases either side of the move. The sizing, the hazards and
the pre-bump diff record live in
`docs/notes/2026-08-22-fuser-upgrade-assessment.md`.

## 10. Testing Strategy

TDD throughout. Layers:

1. **Unit (`cargo test`):** proto round-trips for every message;
   property-based (proptest) frame-codec tests — truncated frames,
   over-limit lengths, garbage opcodes must fail safely: no panics, no
   unbounded memory growth. Server: allowlist matching (incl.
   canonicalize-first symlink cases), node-table
   refcount/generation/hardlink dedup, `UringExecutor` against real temp
   files. Client: multiplexer correlation and window accounting against a
   scripted fake server.
2. **Protocol integration (no FUSE/VM):** raw frames against a real server
   exporting a tempdir — handshake negotiation, attach allow/deny, every
   opcode's happy + errno paths, `ESTALE` after forget, both fsync policies,
   connection-fatal violations. This layer pins the wire contract.
3. **Full-stack loopback (host, needs `/dev/fuse`):** real client mounting
   from a real server over localhost; std::fs operations through the mount.
4. **E2e in VMs:** smoke suite covering every v1 op; **fio** for both
   throughput/latency sanity against the 1 GiB/s ambition and, with its
   `crc32c` verify, the packaged data-integrity gate; disconnect test (kill
   server mid-I/O, assert clean `EIO` + unmountability). **fsx** stays an
   optional extra torture run. **pjdfstest** ships with neither: it wants an
   autotools build the guest contract does not carry, and root on the mount,
   which this design does not grant.

Layers 1–2 need no FUSE or root, so they can run in any future CI. `make
check` (fmt + clippy `-D warnings` + tests) is the standard local gate.

## 11. Fast-Follows and Future Work

Fast-follows (priority order):

1. **Reconnection / session resumption:** re-`ATTACH` on connection loss
   with re-establishment of node and handle state; requires a session-resume
   protocol extension (the `HELLO` version field is the vehicle).
2. **Forced-sync control:** force a real sync even while the server runs
   `fsync = "ignore"`. Two entry points: one reachable from user space on a
   live mount (an ioctl or a control xattr on the mount root), and one the
   client driver calls on its own at moments such as unmount. On the wire,
   both ride the reserved frame flag bit 1 on `FSYNC`/`FSYNCDIR` (or a
   dedicated opcode) so the server honors the sync no matter its policy —
   e.g., before snapshots.

Long-term direction: lbfs grows toward a single-writer, multi-reader,
volatile overlay filesystem tuned for CI and build systems. Build hosts
attach layers in real time rather than pulling data down locally before a
job runs — roughly a three-tier, disaggregated architecture that
side-steps the high transfer cost of sparsely changed or updated files.
The layered backend, read-only layer sharing, and OCI layer sourcing
below are the steps on that path.

Future work:

- Repeatable performance suite under `vm/`: one command that captures the
  network baseline (iperf3, single-stream and windowed), the server's disk
  baseline (fio on the export), and the same fio shapes through the mount,
  then reports the three-way comparison — automating the manual method
  behind `docs/benchmarks/`.
- Layered backend, a second `FileSystem` in the spirit of overlayfs: the
  server stacks an ordered list of directories; reads and writes both go
  to the highest layer that holds the file — no copy-up — and when no
  layer holds it, the write creates the file in the top layer.
- Read-only layers on top of the layered backend: servers can share a
  layer marked read-only. Read-only means immutable — while any attach
  holds a layer read-only, nothing changes it, so clients may cache its
  content and metadata with no coherence protocol. When any server holds
  a layer read-only and another server tries to mount that layer
  writable, the writable attach fails with a conflict error. Needs a
  coordination design for how servers detect the conflict across
  machines.
- OCI layer sourcing on top of the layered backend: the server pulls
  Docker/OCI images and unpacks each image layer into a directory that
  the layered backend then stacks — CI hosts mount an image's filesystem
  in real time without pulling the image themselves. Assess SOCI
  (Seekable OCI, the lazy-loading successor to eStargz) so the server
  can serve a layer's files on demand from an index instead of unpacking
  whole layers up front.
- Container-workload benchmark: mount an assembled container filesystem
  through lbfs — once against a cold server cache, once warm — and race
  it against a traditional containerd flow on the client (pull, unpack,
  start), measuring time-to-first-file and time-to-ready for typical
  images. This prices the real-time-attach thesis against the status
  quo.
- Consistency model for shared layers, the layered backend's first
  principle: one writer per writable layer, any number of readers on
  immutable layers. Readers cache immutable layers as long as they
  like; the writer's private top layer never needs cross-client
  invalidation.
- Two writable-layer kinds for the layered backend: a scratch layer the
  server throws away on detach (the CI job workspace), and a
  to-be-persisted layer that a job fills and then seals into a durable,
  shareable layer (the product of a build). Sealing turns the layer
  immutable and eligible for the read-only sharing above.
- Research how overlayfs answers the layered-semantics questions before
  designing the backend: deletions over lower layers (whiteouts),
  readdir merge, rename and hardlink across layers, opaque directories,
  and its xattr conventions — then decide which answers carry over.
  Server-side layer lifecycle (refcounts, eviction, pinning, disk
  quotas) and content-addressed dedup by OCI digest have no overlayfs
  precedent and need their own design.
- containerd snapshotter plugin: expose lbfs mounts through the
  snapshotter interface the way stargz and SOCI do, so container
  runtimes consume lbfs without changes. Doubles as the harness for the
  container-workload benchmark above.
- Multi-client scale benchmark and server fairness: N readers over
  shared layers is the load shape the direction note targets. Measure
  it, and give the server per-client limits so one job cannot starve
  the rest — per-connection windows exist, server-wide policy does not.
- Production observability: a metrics endpoint (Prometheus shape) with
  per-op latencies, per-layer hit rates, and per-client counters —
  distinct from the eBPF development rig below.
- Read-only attach for clients: a mount mode the kernel enforces on the
  client, pairing with shared immutable layers.
- Feasibility analysis — kernel-module client: a native lbfs kernel
  driver speaking the wire protocol (the shape of the in-kernel NFS
  client) instead of FUSE. The NFS comparison prices the userspace
  round-trip tax at roughly 60 µs per 4k write: kernel NFS answers a
  complete write in ~111 µs while lbfs's own RPC layer alone costs
  146 µs. Weigh that win against cheaper routes to part of it (FUSE
  passthrough, FUSE-over-io_uring via a fuser fork) and against the
  maintenance cost of a kernel module; the VM harness with swappable
  kernels exists for exactly this experiment.
- Server-side kernel integration survey: find the ~54 µs write/read
  asymmetry in the raw RPC path (4k write 146 µs vs read 92 µs with no
  FUSE involved), then assess zero-copy sends from page cache to socket
  (splice/sendfile, MSG_ZEROCOPY), io_uring SQPOLL and provided buffers,
  and whether an in-kernel serving path (the knfsd shape) pays for
  itself. Complements the io_uring registered-buffers item below.
- mTLS (rustls stream wrap) and an authorization `FileSystem` decorator.
- Performance test rig capturing detailed kernel-level metrics — eBPF
  (bpftrace/BCC) or similar: per-opcode latency histograms, io_uring
  submission/completion queue depths, FUSE request latency breakdowns,
  off-CPU time, network round-trip decomposition. Runs inside the VM
  environment against the fio/fsx workloads.
- `cargo miri test` for the pure-logic crates (proto, node table); miri
  cannot execute io_uring/syscall layers.
- Fuzzing the frame decoder (cargo-fuzz) with the mTLS/hardening milestone.
- `MKNOD`, `ACCESS`, byte-range locks.
- io_uring registered buffers/files; multi-connection (per-core queue)
  scaling, NVMe-oF-style.
- `mount.lbfs` helper for fstab integration.
- CI wiring for test layers 1–2.
- **Server-side `security.capability` clearing under a privileged server.**
  `FUSE_HANDLE_KILLPRIV_V2` shipped without it (§5.3). A server holding
  `CAP_FSETID` clears set-user-ID and set-group-ID but leaves the capability
  attribute; picking it up means either a per-write probe or a per-node cache
  invalidated by `SETXATTR`.
- **`fallocate` kill-priv under a privileged server.** Punch-hole and
  zero-range change file data, yet the server's `Explicit` policy strips
  only on write, truncate, and `copy_file_range`. Under the shipped
  unprivileged default the backing kernel covers it; a `CAP_FSETID` server
  using `fallocate` keeps suid bits it promised to clear. Same shape as the
  `copy_file_range` fix on the kill-priv branch — the strip call and a
  forced-policy test.

Noted and deferred:

- Client-side persistent layer cache and prefetch: deferred — clients
  run one job and vanish, with low launch cost, so a per-host cache has
  little to feed.
- uid/gid mapping for heterogeneous hosts on shared layers: undecided;
  revisit with the authorization decorator.
- Wire compression (zstd per data segment): deferred — the target
  network is fast; revisit if deployment ever leaves the LAN/VPC class.
- Protocol upgrade/compat policy: deferred — client and server ship and
  upgrade together, and a server upgrades by starting fresh and seeding
  again from OCI data, so cross-version pairs never occur.

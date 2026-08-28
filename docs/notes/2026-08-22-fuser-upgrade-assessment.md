# fuser upgrade assessment, 2026-08-22

Sizing exercise, not a plan. It answers two questions: what the three released
upgrade steps cost against *our* code and what they buy, and whether the four
things no fuser release carries are worth building ourselves.

> Correction (2026-08-22): the upgrade plan at
> `docs/superpowers/plans/2026-08-22-fuser-two-step-upgrade.md` checked this
> note against the published crates by compiling them and found eight deltas.
> The largest: `ForgetOne` is private at 0.18.0, so the `batch_forget`
> override goes away rather than migrating; `join()` at 0.18.0 no longer
> unmounts, so callers need `umount_and_join()`; `Config` is
> `#[non_exhaustive]`, so struct expressions with `..Default::default()`
> fail to build. Smaller: `lseek` keeps `offset: i64`; `rename`,
> `copy_file_range`, and `setattr` carry three more bitflags conversions;
> the dependency audit belongs at 0.18.0 (0.16.0's dependency list matches
> 0.15.1); the TTL complaint lives at `fuse.rs:465-467`, not `:29`; the
> stale-prose list is longer than the one below. The plan is the executable
> authority where the two disagree.

Sources read first-hand for this note:

- vendored `fuser-0.15.1` under `~/.cargo/registry/src/index.crates.io-*/`
- tags `v0.16.0` and `v0.18.0` from `raw.githubusercontent.com/cberner/fuser`
- a full clone of `cberner/fuser` at `origin/HEAD` = `c0420fc` (2026-08-07),
  used to cherry-pick the unreleased kill-priv work onto `v0.18.0` and build it
- `crates/lbfs-client/src/{fuse.rs,main.rs,conn.rs}`, `tests/tests/loopback.rs`
- `docs/benchmarks/2026-08-22-bottleneck-analysis.md`
- `docs/superpowers/plans/2026-08-22-per-write-getattr-elimination.md`
- `libfuse` master `lib/fuse_uring.c` (1042 lines), as the io_uring reference

A companion research note lives in the session scratchpad as
`fuser-crate-research.md`; this note verifies its claims against our tree rather
than repeating them.

## Headline

Three findings drive everything below.

**No upgrade step moves a single measured number.** The bridge's own dispatch
thread never exceeds 15.6% of a core in any run in the bottleneck campaign, so
the multi-threaded event loop that 0.17.0 added has nothing to relieve. Treat
0.16.0 and 0.18.0 as API and bug-fix work whose payoff arrives one release
later, not as performance work.

**The kill-priv plan should run first, on 0.15.1, exactly as written.** It
routes around the same forwarding hole that makes released 0.18.0 dangerous, so
it neither needs the upgrade nor breaks on it. Rework at upgrade time comes to
about four lines. The plan also carries the largest measured win on the table:
roughly 90 µs off a 296 µs 4 KiB write.

**Only two of the four unbuilt items deserve any of our time**, and both are
ours rather than upstream's: a sysctl probe at mount time, and a client-side
buffer pool that attacks the same per-megabyte copy cost splice would, for a
fraction of the price and with no fork.

---

## Part A — sizing the released upgrade steps

### A1. 0.15.1 → 0.16.0

The research note's claim holds. I diffed the trait declaration on tag
`v0.16.0` against the vendored 0.15.1: `pub trait Filesystem` still takes
`&mut self`, still takes `Request<'_>`, still spells node ids and handles as
`u64`, still returns `Result<(), c_int>` from `init`. Every one of our 35
callbacks compiles untouched.

**Every line of ours that changes:**

| file:line | today | after |
|---|---|---|
| `Cargo.toml:35` | `fuser = { version = "0.15", features = ["abi-7-31"] }` | `fuser = { version = "=0.16.0", features = ["abi-7-31"] }`, then a second commit flipping to `abi-7-40` |
| `crates/lbfs-client/src/fuse.rs:336` | `bit: u32,` in `struct Capability` | `bit: u64,` |
| `crates/lbfs-client/src/fuse.rs:1521` | `fn requested(writeback: bool) -> u32` | `-> u64` |
| `Makefile:~40` comment + `apt-get install libfuse3-dev` | build container needs libfuse3 headers | headers no longer needed once `libfuse` stays off |
| `README.md:13,31,36` | "the client links libfuse3" | the client shells out to `fusermount3` instead |
| `vm/up.sh:127` comment | "the client's libfuse3 link" | stale wording |

That is it. The nine `libc::E*` call sites, the `fuser::consts` imports, the
`FOPEN_*` word and `MountOption` all keep their 0.15.1 types: 0.16.0 widened
only the INIT flag family to `u64` (verified in `src/ll/fuse_abi.rs` on the
tag — `FOPEN_*` and `FUSE_WRITE_KILL_PRIV` stay `u32`).

**Effort: 1-2 hours**, of which the build-and-deploy check is most of it.

**The `libfuse` feature question.** 0.15.1 sets `default = ["libfuse"]`;
0.16.0 sets `default = []`. Leaving the feature off switches
`fuser_mount_impl` from `libfuse3` to `pure-rust`, which drops the
`libfuse3.so.4` link and execs `fusermount3` instead. Both guests already carry
that binary — `vm/lib.sh:53` installs the `fuse3` package and `vm/test.sh`
calls `fusermount3` directly — so the deploy keeps working and the build
container stops needing `libfuse3-dev`. Our own test harness says as much
already: `tests/tests/loopback.rs:116` requires `fusermount3` on `PATH`, and
`crates/lbfs-client/tests/loopback_cli.rs:45` repeats it. Turning the feature
off is the right call, and the loopback suite covers the three behaviours that
could differ — `auto_unmount`, the `max_read=` custom option, and the
unmount-on-drop ordering `main.rs:172` depends on.

**Risk notes.**

- *Declaring 7.40 on a crate that skipped 7.32-7.39.* On tag `v0.16.0` the only
  INIT constants above bit 25 are `FUSE_INIT_EXT` (30), `FUSE_INIT_RESERVED`
  (31) and `FUSE_PASSTHROUGH` (37). Announcing 7.40 while naming none of the
  bits between them is safe only because every skipped feature turns on an INIT
  flag we never ask for. Land the crate bump and the ABI bump as two commits so
  a revert of one does not drag the other.
- *New transitive dependency.* 0.16.0 pulls `nix` 0.29, and `abi-7-40` pulls
  `nix/ioctl`. Worth one look from whatever audits our dependency tree.
- *Toolchain.* 0.16.0 declares edition 2024 and `rust-version = "1.85"`. Our
  `rust-toolchain.toml` says `stable`, so this passes today and pins nothing.
- *What 7.40 actually buys lbfs: nothing yet.* Passthrough needs a local
  backing descriptor, which a network client does not have. The value of this
  step is that it stages the next one and drops a shared-library link.

### A2. 0.16.0 → 0.18.0

This is the real work. Signature changes come from `src/lib.rs` on tag
`v0.18.0`, counted against our tree with `grep` rather than estimated.

**Our sites, counted:**

| change | count | where |
|---|---|---|
| `&mut self` → `&self` | 33 of 35 (`init`/`destroy` keep `&mut self`) | `fuse.rs:626`-`1207` |
| `Request<'_>` → `Request` | 34 | same block |
| `ino`/`fh`/`parent`/`newparent`: `u64` → `INodeNo`/`FileHandle` | 49 parameters | same block |
| `conn.*` call lines needing `.0` on those parameters | 33 | `fuse.rs:630`-`1198` |
| `reply.error(libc::E*)` → `Errno::*` | 8, plus the `errno()` helper at `fuse.rs:495` | |
| `u64::try_from(offset)` guards deleted | 5 | `read`, `write`, `fallocate`, `lseek`, `copy_file_range` |
| `as i64` casts on directory offsets deleted | 3 | `readdir`, `readdirplus` |
| `fuser::consts::{...}` import → `InitFlags`/`FopenFlags` | 1 import block, 5 names | `fuse.rs:52-55` |
| `Capability { bit: u64 }` → `InitFlags` | struct + 4 entries + 6 tests | `fuse.rs:335-401`, `1521-1578` |
| `mount_options() -> Vec<MountOption>` → `Config` | 1 function + 6 tests + 2 callers | `fuse.rs:426`, `main.rs:148`, `loopback.rs:341` |
| `spawn_mount2` → `spawn_mount` | 2 | `main.rs:151`, `loopback.rs:341` |
| `BackgroundSession::join()` now returns `io::Result` | 1 | `loopback.rs:434` |
| `fuse_forget_one` → `ForgetOne`, fields → methods | 2 lines | `fuse.rs:639-642` |
| `FileAttr.ino: INodeNo`, `generation: Generation` | 3 sites + 3 tests | `fuse.rs:145`, `501`, `821`, `1302` |
| `open_flags() -> u32` → `FopenFlags` | 1 function + 2 tests | `fuse.rs:410`, `1576` |
| `init` returns `io::Result<()>` | 1 signature, 2 error returns | `fuse.rs:552`, `566`, `605` |

Three notes on that table.

The 33 `.0` unwrappings do **not** have to touch 33 argument lists. One line at
the top of each callback — `let (ino, fh) = (ino.0, fh.map(|h| h.0));` — leaves
every body untouched and keeps the diff readable. Prefer that shape.

The `Errno` clash is real and worth planning for: `fuse.rs:65` imports
`lbfs_proto::Errno`, and 0.18.0 re-exports `fuser::Errno` from `crate::ll`.
Alias one at the import, do not rename the other.

Two callbacks keep a raw `i32` in 0.18.0 where the neighbours moved to
bitflags: `create(flags: i32)` and `setxattr(flags: i32)`. Our `flags as u32`
casts at `fuse.rs:820` and `fuse.rs:1089` survive unchanged; `open`, `read`,
`release`, `opendir`, `releasedir` and `write` all move to `OpenFlags`.

**The `MountOption` → `Config` restructure** is the one piece that is not
mechanical. `AllowOther` and `AllowRoot` no longer exist as options; the
`acl: SessionACL` field replaces them, which reshapes `mount_options()` into
something like:

```rust
pub fn session_config(max_io_size: u32, allow_other: bool, auto_unmount: bool) -> Config {
    Config {
        mount_options: vec![/* FSName, DefaultPermissions, CUSTOM, NoSuid, NoDev, AutoUnmount */],
        acl: if allow_other { SessionACL::All } else { SessionACL::Owner },
        n_threads: None,
        clone_fd: false,
        ..Default::default()
    }
}
```

`Config` carries `#[non_exhaustive]`, so the struct literal needs `..Default::default()`.
Of the six unit tests at `fuse.rs:1583-1632`, four survive with the field
swapped in, one (`access_widening_options_are_opt_in`) splits into an option
assertion plus an ACL assertion, and one
(`the_option_list_holds_no_duplicates_or_conflicts`) loses half its point,
since nobody can build the `AllowOther`/`AllowRoot` pair it guards against any
more.

**Effort: 1.5-3 days.** One day for the sweep, half a day for the tests, and
the rest for `make test-loopback` plus a VM re-measure to prove nothing moved.

#### What `n_threads` and `clone_fd` would mean for us

`Config { n_threads, clone_fd }` exists in released 0.18.0 (I read the struct
on the tag). `Session::run` spawns `n_threads` event loops, each with its own
`FuseReadBuf`, and `clone_fd: true` gives each one a private `/dev/fuse`
descriptor through `FUSE_DEV_IOC_CLONE`.

Our bridge already spawns every callback onto tokio, so the only thing a second
event loop adds is a second reader of `/dev/fuse`. The measurements say that
reader is not the constraint anywhere:

| shape | what the session thread costs | headroom above it |
|---|---|---|
| seq read 1 MiB, 1544 MB/s | not the busiest thread; a tokio worker leads at 27.9% of a core | client box 61.8% idle |
| seq write 1 MiB, 869 MB/s | 15.6% of a core — the busiest client thread in that run | client box 73.6% idle |
| randwrite 4k, 3463 IOPS | client total 30.6%, busiest thread a tokio worker at 12.8% | client box 77.9% idle |

Per shape, then:

- **4 KiB QD1 (119 µs read / 296 µs write).** One request in flight by
  definition. A second reader thread cannot help, and the 27 µs FUSE slice on a
  read is kernel path plus one `read`/`writev` pair, not queueing behind another
  request. Expect zero.
- **4 KiB QD16 (40.3k IOPS against a 48.8k raw-RPC ceiling).** The 8.5k IOPS
  gap is 21%, and the session thread is nowhere near a core at that rate: the
  1 MiB write run shows ~180 µs of session-thread time per operation, and the
  megabyte copy dominates that, so a 4 KiB operation costs it single-digit
  microseconds. One thread saturates somewhere north of 100k IOPS — well past
  the protocol's own ceiling. Expect zero to low single digits.
- **Streaming.** The session thread scales to roughly 41% of a core if the
  mount ever reached the 2272 MB/s raw ceiling. Still one thread, still not
  full. And the guest has two vCPUs, one of which the tokio workers already
  occupy, so a second event loop competes rather than adds.
- **Where it *would* pay:** a guest with four or more vCPUs, running many
  files concurrently (the four-threads-four-files shape that scaled 2.66×),
  after the RPC layer's own ceiling moves. None of those describes today.

**Memory cost.** `BUFFER_SIZE = MAX_WRITE_SIZE + 4096` and `MAX_WRITE_SIZE` is
16 MiB in 0.18.0, unchanged from 0.15.1 — and the buffer does not shrink to the
negotiated `max_write`, and `n_threads: Some(4)` reserves 64 MiB of resident
buffer on a 1962 MB guest, about 3%. That guest already shows write throughput
swinging 4× on server page-cache pressure, so 64 MiB is not free.

**Recommendation:** take the upgrade, ship `n_threads: None` and
`clone_fd: false`, and put both behind a hidden flag so somebody can measure a
future four-vCPU guest without another code change.

#### What A2 does buy

- `ReplyEntry::entry_with_ttls()` splits the entry TTL from the attribute TTL,
  which is exactly the conflation our own module doc complains about at
  `fuse.rs:29`. For a single-client export a long name TTL with a short
  attribute TTL is worth trying on build workloads.
- Unsigned offsets delete five guard blocks that can never fire.
- `time_from_system_time` on the reply path stops mangling pre-epoch fractional
  seconds. That is half of the README limitation at `README.md:211`; 0.18.0
  fixes the outbound half, and the inbound half (`system_time_from_time`) waits
  for the next release (commit `e48279f`, dated two days after the 0.18.0 tag).
- The path to 0.19, where the kill-priv forwarding, `inc_epoch()` and
  `remaining_capacity()` live.

**Risk notes.**

- *Unmount ordering.* `main.rs:172` treats `drop(session)` as "unmount, drain,
  exit", and `tests/tests/loopback.rs:434` joins the session thread instead.
  0.18.0's `BackgroundSession` has no `Drop` of its own; the `Mount` it holds
  unmounts on drop, same as 0.15.1, so the drain still happens inside
  `umount(2)`. Upstream commit `bceb68b` ("Wait for the session to end when
  dropping BackgroundSession") lands *after* 0.18.0, so the behaviour we rely on
  stays exactly as today. Prove it with the loopback suite anyway; a
  silent change here loses data.
- *A 0.18.0 hazard we sidestep by accident.* `FUSE_SETXATTR_EXT` is public and
  negotiable on 0.18.0, and the parser still reads the old 8-byte layout, which
  panics the session thread and wedges the mount. Commit `40e5006` fixes it
  after the release. We never ask for that capability, so we never hit it — but
  this makes a second example of the same class as the kill-priv hole, and it
  explains why the `add_capabilities` list deserves a careful read at every bump.
- *`add_capabilities` on 0.18.0 checks the kernel only.* I read the function on
  the tag: it compares the ask against the kernel's advertised set and nothing
  else. The refusal list that would have caught both hazards arrives in
  `f39068e`, post-release.

### A3. Interplay with the written plans

**Only one plan exists in the tree.**
`docs/superpowers/plans/2026-08-22-per-write-getattr-elimination.md` is there
(79 KB, ten tasks). No `2026-08-22-parallel-direct-writes.md` exists anywhere
under the checkout. Notes on that idea sit at the end of this section.

#### Does 0.15.1 show us every kill signal the plan needs?

I checked all three against the vendored source. The plan's reading is correct
on every point.

| signal | visible in 0.15.1? | evidence |
|---|---|---|
| `FUSE_WRITE_KILL_SUIDGID`, `fuse_write_in.write_flags` bit 2 | **Yes** | `fuse_abi.rs:771` carries `write_flags: u32`; `consts::FUSE_WRITE_KILL_PRIV = 1 << 2` at `fuse_abi.rs:271` under `abi-7-31`, the feature we already turn on. Our `write()` names the parameter `_write_flags` at `fuse.rs:862` and throws it away. |
| `FATTR_KILL_SUIDGID`, `fuse_setattr_in.valid` bit 11 | **No** | The `FATTR_*` list at `fuse_abi.rs:144-167` runs bits 0-10 then jumps to the macOS bits at 28-31. Bit 11 has no name, and more to the point the raw `valid` word never reaches anything behind the `Filesystem` trait — fuser decodes it into `Option<...>` arguments and drops the rest. |
| `FUSE_OPEN_KILL_SUIDGID`, `fuse_open_in.open_flags` | **No, and unreachable anyway** | 0.15.1 declares the second word of `fuse_open_in` as `unused: u32` (`fuse_abi.rs:691-696`). It would not matter if it did: the kernel sets that bit only when `O_TRUNC` survives into `inarg.flags`, which needs `FUSE_ATOMIC_O_TRUNC`, and we withhold that on purpose (`fuse.rs:345-349`). The kernel strips `O_TRUNC` and sends a `SETATTR` instead. |

The plan takes the one signal that exists, and covers the truncate case
server-side through `KillPrivPolicy::Explicit`. That is the only route
available, on any fuser version we could pin today.

One semantic note on the server-side truncate strip, worth a line in the spec
rather than a code change: the kernel sets `FATTR_KILL_SUIDGID` on a truncate
only when the *caller* lacks `CAP_FSETID`, while the plan's server strips
whenever `args.size.is_some()`. That over-strips for a privileged caller. It
errs toward clearing privilege, which is the safe direction, and the wire
carries no credentials in v1 anyway.

#### Does the plan dodge the 0.18.0 forwarding bug?

**Yes, in full, and by construction rather than by luck.** The upstream
commit that documents the bug names precisely the two signals the plan already
handles elsewhere:

> Of the three signals it sends, only `FUSE_WRITE_KILL_SUIDGID` reaches the
> filesystem: `FATTR_KILL_SUIDGID` on a chown or truncate is not among the
> `FattrFlags` setattr decodes, and `FUSE_OPEN_KILL_SUIDGID` sits in the
> `open_flags` field of `fuse_open_in`, which fuser does not read.
> — `f39068e`, "Refuse capabilities fuser cannot honor"

The plan consumes the one signal fuser does forward, covers truncate in the
server, and cannot see the open signal because the kernel never sends it to a
mount without `FUSE_ATOMIC_O_TRUNC`. The chown obligation rides the server's
own `chownat`. Every hole the upstream commit lists is a hole the plan already
fills from the other side.

The practical consequence: **once the plan lands, released 0.18.0 stops being
dangerous for us.** The refusal list in `f39068e` would refuse our ask, which is
why a bump past 0.18.0 wants either the patched build in B3 or the release that
carries `923f7a4`.

#### Cheaper before or after the upgrade?

Before. The plan's whole fuser surface is three things:

1. `const FUSE_HANDLE_KILLPRIV_V2: u32 = 1 << 28;` — a local constant, because
   fuser 0.15.1 names nothing above bit 19.
2. one `Capability` entry with `required: false`.
3. reading `write_flags` in `write()` and comparing against
   `consts::FUSE_WRITE_KILL_PRIV`.

Rework at each later step:

| step | what the plan's client code needs |
|---|---|
| → 0.16.0 | change the local const and the `Capability` field to `u64`. Two lines. The plan's own test `assert_eq!(FUSE_HANDLE_KILLPRIV_V2, 1 << 28)` still passes. |
| → 0.18.0 | **delete** the local const: `InitFlags::FUSE_HANDLE_KILLPRIV_V2` exists on the tag at bit 28, and `WriteFlags::FUSE_WRITE_KILL_SUIDGID` exists at bit 2. The plan's code gets shorter. |

Four lines of churn, against a plan that is the largest measured win available:
about 90 µs off 296 µs on a 4 KiB write, and the same probe removed from every
write at every size. Delaying it behind two or three days of upgrade work buys
nothing.

#### Recommended order

1. **Plan 1 (kill-priv), on 0.15.1, unchanged.** Ten tasks, thirteen files,
   ~90 µs per write.
2. **The tripwire test from B3**, either as plan 1's Task 9 or as the minimal
   standalone version if the upgrade somehow runs first.
3. **0.15.1 → 0.16.0**, two commits (crate bump; then ABI bump), `libfuse`
   feature off.
4. **0.16.0 → 0.18.0**, one focused branch, `n_threads: None`.
5. **B4's sysctl probe**, small and independent, whenever it fits.
6. **Revisit KILLPRIV_V2 forwarding** at 0.19, or take the B3 patch build if
   0.19 slips past the point where we want `remaining_capacity()`.

#### On the missing parallel-direct-writes plan

The idea it names is sound and needs no fuser change at all. The measured
problem is real — four threads on one file give 0.98× the throughput of one and
exactly 4× the latency, which is the kernel's per-inode exclusive lock. The
kernel's answer is `FOPEN_PARALLEL_DIRECT_WRITES`, bit 6 of the `open_flags`
word in the `OPEN` reply (`/usr/include/linux/fuse.h:389`). On 0.15.1 our
`open_flags()` at `fuse.rs:410` already returns a raw `u32`, so setting bit 6
takes a local constant and one `|`. On 0.18.0 the flag has a name:
`FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES`. Upstream issue #368 asks for the
constant and nothing more.

The correctness question is ours, not fuser's: the kernel drops the inode lock
on the strength of that bit, so the server must handle concurrent direct writes
to one inode safely. Ours does — writes carry explicit offsets into `pwrite`
through the ring — but overlapping ranges lose their ordering guarantee, and the
flag governs the direct-I/O path only, so it changes nothing for the
writeback-cached path a build workload uses. Worth writing up as its own small
plan; it does not belong in the upgrade.

---

## Part B — the items no release carries

### B1. splice(2) on `/dev/fuse`

**Sketch.** fuser reads with `nix::unistd::read` and replies with
`nix::sys::uio::writev`, on `v0.18.0` and on `origin/HEAD` alike. Zero-copy
needs three pieces: a pipe-backed receive buffer, chosen once the kernel grants
`FUSE_SPLICE_READ` (`channel.rs`, `read_buf.rs`, `session.rs`); a request parser
can take the header out of the pipe and leave the payload in it, which today
assumes one aligned `&[u8]` (`ll/request.rs`); and a `write_buf`-shaped
`Filesystem::write` plus a splice-capable reply path (`lib.rs`, `reply.rs`).
polyfuse's `main` branch does exactly this and is the only Rust prior art —
`impl SpliceRead for Device`, `RequestBuf::new_pipe()` — in a crate that has had
no commits since 2025-12-18 and warns readers off its branch. Call it 600-900
lines in the fork, plus roughly 200 in our `conn.rs`, which today builds an
owned `Vec` and hands it to `write_vectored`.

**What it removes, in our numbers.** Per 1 MiB read the client copies twice:
socket into a `Vec` via `read_exact`, then that `Vec` into `/dev/fuse` via
`reply.data()`. Per 1 MiB write it copies twice as well: `/dev/fuse` into
fuser's session buffer, then `data.to_vec()` at `fuse.rs:871`. At a guest memcpy
rate around 8 GB/s each copy costs roughly 130 µs per megabyte. Set that against
what the FUSE layer adds per megabyte today — 213 µs on reads, ~444 µs on writes
— and one copy is over half the read overhead. Attribution from the CPU table
agrees: 3.1 GB/s of copy traffic during a 1544 MB/s streaming read would take
about 0.39 of a core if copies ran at 8 GB/s, against 0.557 of a core measured
for the whole client process.

Upper bound if every copy vanished: seq read 1580 → roughly 2000 MB/s, seq write
burst 869 → roughly 1100 MB/s. Sustained writes would not move at all, because
the server's page-cache flush caps them at 361 MB/s. Small operations would not
move either — a 4 KiB copy is well under a microsecond.

**Carrying cost.** Upstream refuses pull requests, so this is a permanent fork,
against a crate that took 52 commits in the 16 days after its last release. A
600-900 line diff across `channel.rs`, `read_buf.rs`, `session.rs`,
`ll/request.rs`, `lib.rs` and `reply.rs` touches the files that churn most.
Budget a day per release to rebase, forever.

**Call: skip the fork. File on issue #298 instead**, and take the cheaper local
version described below.

**The 80/20 that is ours.** Half of the write-side copy cost is our own
`data.to_vec()`, and a good part of *that* is not the copy but the fresh 1 MiB
`Vec` behind it: a new heap block per write means 256 first-touch page faults
per write, every write. A pooled buffer on the client — the server already has `PooledBuf` —
keeps the memcpy but retires the faults, needs no crate change, and is worth
measuring before anyone costs out a fork. Size: S, ours, and it belongs on the
list above.

### B2. FUSE_OVER_IO_URING transport

**Sketch.** This is a transport rewrite, not a feature. fuser has the
capability bit and nothing behind it, `origin/HEAD` refuses to negotiate it
(`f39068e`), issue #380 has sat with zero comments since 2025-08-15, and fuser
depends on `nix` rather than on any io_uring crate, so the fork starts by adding
a dependency. The work: register one ring per queue with
`IORING_OP_URING_CMD` + `FUSE_IO_URING_CMD_REGISTER`, map the per-entry header
and payload buffers the kernel expects, and turn the read/reply session loop
into a completion loop driven by `COMMIT_AND_FETCH`.

A reference exists, and it has a known size: libfuse master carries `lib/fuse_uring.c` at
1042 lines plus an 86-line header. polyfuse's `main` does **not** have a ring
of its own — its io_uring presence is the capability bit only. That leaves a
port from C into a crate whose session loop it replaces: 1200-1800 lines of new
code, no upstream landing zone, permanent divergence.

**What it would buy lbfs.** The transport removes the `read`/`writev` syscall
pair per request and lets a request stay on one core. Our FUSE slice is 27 µs of
a 119 µs 4 KiB read and 102 µs of a 393 µs QD16 read. Even halving those gives
about 11% on QD1 and 13% at QD16 — and at QD16 the raw-RPC ceiling is only 21%
above where we already sit, so most of that gain has nowhere to go. Streaming
would gain little, since its cost is copies rather than syscalls.

**Call: skip.** Size XL, upside ~10%, permanent fork. Revisit only if fuser
builds it or if kernel 7.x makes the ring the fast path by default.

### B3. The KILLPRIV_V2 gap, and a tripwire against it

**Do the master commits apply on `v0.18.0`? Yes — I tried it.** Branching from
the tag and cherry-picking four commits in order:

| commit | conflicts | nature |
|---|---|---|
| `38f4e1e` (4096-byte floor in `set_max_write()`) | `CHANGELOG.md` | trivial |
| `40e5006` Parse the extended `fuse_setxattr_in` layout | `CHANGELOG.md`, `src/session.rs` | one 6-line hunk, caused by an unrelated `ECONNABORTED` commit landing first |
| `f39068e` Refuse capabilities fuser cannot honor | `CHANGELOG.md` | trivial |
| `923f7a4` (wires up FUSE_HANDLE_KILLPRIV_V2) | none | clean |

Resolved that way, `cargo build` succeeds and `cargo test --lib` reports 62
passed, 0 failed. The two you actually want (`f39068e`, `923f7a4`) will not
build alone — `923f7a4` sits on the parser plumbing `40e5006` adds — so the
patch set is those four, not two.

**Cheapest path, ranked.**

1. **Wait for 0.19.** Release cadence has been 10 months, then 5, then 5, so
   late 2026 is plausible, though upstream announces nothing. Cost: nothing.
   Risk: the wait.
2. **`[patch]` to a git rev on master.** Zero rebase effort, but it pulls all 52
   post-release commits, including further breaking signatures (`Owner` on
   creation calls, `Option<u32>` from `Request::uid()`, `statx`, `tmpfile`) and
   code that carries "at least a cursory" human review by the maintainer's own
   statement.
3. **`[patch]` to our own branch: `v0.18.0` + those four commits.** Bounded,
   readable, reproducible from the recipe above, and it keeps the 0.18.0 API we
   just migrated to. Downside: `923f7a4` adds a `kill_suid_gid` argument to
   `setattr`, `open` and `create`, so our bridge gains three parameters it can
   ignore — the server already covers all three cases.

**Recommendation: (1), with (3) held in reserve.** We do not need the forwarding
fix, because the kill-priv plan covers every signal from the other side. The
only reason to reach for (3) is if a bump lands us past `f39068e` — whose
refusal list would reject our `FUSE_HANDLE_KILLPRIV_V2` ask outright — before
0.19 ships the code that reverses the refusal.

#### The tripwire test

The question is whether we can catch broken kill-signal forwarding on any future
bump, from our own suite.

**A real FUSE mount is available.** `tests/tests/loopback.rs` mounts for real —
`Loopback::start` calls `fuser::spawn_mount2` at line 341, `require_fuse()`
checks `/dev/fuse` at 107, and `wait_ready()` polls until the mount answers.
Cases carry `#[ignore]` and run through `make test-loopback`. The test needs
no new harness: it writes through `lb.mnt()` and reads the mode back off
`lb.export()`, behind the mount's back.

**Plan 1's Task 9 is already that test**, and it goes further than the brief
asks: set-user-ID on write, set-group-ID with group-execute on write,
set-group-ID *without* group-execute staying put, and the truncate variant, each
run against both writeback settings. About 75 lines plus two `#[test]`
wrappers, styled after the existing `file_content_round_trips(writeback: bool)`
pair.

**One caveat about what it proves, which the plan itself half-states.** On our
deployment the server runs unprivileged, so its own kernel strips the bits
inside `write(2)` and `truncate(2)` whatever the client forwards. The assertion
holds even if fuser drops every kill signal — the plan notes that it passes on
the pre-change tree for exactly this reason. As an end-to-end *guarantee* check
that makes it sound and worth having; as a *forwarding* check, blind by default.

Closing that gap costs about five lines: give `KillPrivPolicy::detect()` a
test-only override (an environment variable, or a constructor the test calls) so
one variant runs the server under `Explicit`, where the strip comes only from
the forwarded `kill_suidgid` flag. With that in place, a fuser bump that stops
forwarding `write_flags` fails the write case loudly. The truncate case still
proves the contract rather than the forwarding, because the server decides that
one from `args.size` with no fuser signal involved.

**Should a minimal standalone version go in before plan 1?** Only if the
upgrade runs first, and its value is modest either way: without
`FUSE_HANDLE_KILLPRIV_V2` negotiated, the client's own kernel strips the bits,
so the assertion is close to a tautology. A 35-40 line version — one writeback
setting, write plus truncate, no production changes at all — costs almost
nothing and locks the observable guarantee against every later change including
the upgrade. Add it if the order slips; otherwise let Task 9 supersede it.

### B4. `max_pages_limit` and readahead awareness — ours, not theirs

fuser knows nothing about `fs.fuse.max_pages_limit`, has no way to learn what
the kernel clamped `max_write` to, and has no open issue asking for either. It
cannot have one: the INIT reply travels one way, and the kernel's clamp in
`process_init_reply` leaves no trace fuser could read. Nothing upstream could
ship would help. Same story for readahead — `set_max_readahead` checks the value
the kernel offered in `fuse_init_in`, and the bdi's `read_ahead_kb` cap lands
afterwards.

Both belong in our mount path, as a probe plus a warning:

```
at startup, before the mount:
  read /proc/sys/fs/fuse/max_pages_limit  -> pages
  ceiling = pages * page_size
  if negotiated max_io_size > ceiling:
      warn with the exact `sysctl -w fs.fuse.max_pages_limit=N` to run

after the mount answers:
  find the mount's dev in /proc/self/mountinfo
  read /sys/class/bdi/<major:minor>/read_ahead_kb
  if it is below max_io_size / 1024: warn, naming the file to raise
```

Around 60-80 lines split between `fuse.rs` and `main.rs`, with unit tests over
the two parsing helpers and no privilege needed to read either path. Both guests
report `fs.fuse.max_pages_limit = 256` (1 MiB) and `read_ahead_kb = 128` today,
so the probe would fire on both counts immediately and the operator would stop
having to rediscover it from a benchmark run.

Tidiness is not the sharpest reason to build it. Separate in-flight work
raises `DEFAULT_MAX_IO_SIZE` from 1 MiB to 4 MiB. `set_max_write(4 MiB)` will
return `Ok` and the kernel will quietly clamp writes to 1 MiB, while the
`max_read=4194304` mount option at `fuse.rs:430` stays at 4 MiB. Whether the
kernel then issues reads the multiplexer answers with `EINVAL` is an open
question, and the probe is what turns it from a mystery into a warning line.

**Call: build it.** Size S, ours, independent of every other item here.

### B5. Master-only conveniences

**`ReplyDirectory::remaining_capacity()` (commit `a7e90b4`) — the one worth
wanting.** I confirmed the gap it closes: in 0.15.1 `DirEntList` keeps
`max_size` private with no accessor, so the reply buffer's size is invisible to
us. That invisibility is the whole reason we pin `READDIR_PAGE_BYTES` at
4096 behind a 25-line comment explaining why a wider ask wastes server work
(`fuse.rs:69-93`).

What the accessor would delete or shrink:

| today | with `remaining_capacity()` |
|---|---|
| `READDIR_PAGE_BYTES = 4096` and its 25-line rationale | ask for exactly the room remaining |
| `first_entry_overflow()` — 13 lines plus a `debug_assert` | unreachable by construction; keep as a cheap guard or drop |
| `PageOutcome`, `consume_readdirplus_page`'s forget-payback | keep as a safety net, but it stops firing |
| 5 unit tests plus 2 loopback cases built on the overflow shape | most survive; the overflow-specific ones lose their subject |

Net maybe 40 lines simpler. The real prize is round trips, not lines: glibc
calls `getdents64` with a 32 KiB buffer, so a listing that fits one kernel reply
costs us eight server round trips at 4 KiB each. At ~92 µs per round trip, a
1000-name directory costs roughly 2.3 ms today against roughly 0.4 ms if we
sized the ask to the buffer.

**And we can have most of that today, without upstream.** Ramp the ask: start
at 4 KiB, and each time a page is fully consumed, double the next ask up to some
cap. The existing forget-payback already handles the over-fetch the ramp will
occasionally cause. Ten lines in `readdir`/`readdirplus`, no crate change, and it
converges on the same round-trip count that `remaining_capacity()` would reach
directly. **Call: build the ramp, skip the `[patch]`, and adopt the accessor for
free at 0.19.**

**`Notifier::inc_epoch()` (commits `9b16065`, `576968f`).** One notification
invalidates the whole dentry cache, at ABI 7.44. Relevant to a network client
whose backing store changed wholesale — but v1 assumes one client owns the
export, so nothing changes behind the mount's back and we send no invalidations
at all today. No value until that assumption relaxes. **Call: defer.**

**`Notifier::expire_entry()` (`4c65119`) and `FUSE_HAS_EXPIRE_ONLY`.** Same
reasoning. **Defer.**

**`system_time_from_time` pre-epoch fix (`e48279f`).** Fixes the inbound half of
the README limitation at line 211 — a `utimensat` with a pre-1970 fractional
time arrives at our `setattr` shifted by up to two seconds. A one-file, 56-line
change. Not worth a `[patch]` on its own; fold it in if we ever build the B3
patch branch anyway. **Call: defer.**

---

## Dependency governance

Two upstream changes in July 2026 alter how we should consume this crate, and
neither is about code quality.

**Upstream stopped taking pull requests.** The README's Contribution section now
opens with:

```text
Pull requests are no longer being accepted. Please file an issue, or fork the
project instead.
```

The open-PR queue reads as zero because the door has closed, not because
throughput is high. That takes every "send a patch upstream" option in this note
off the table; the choice narrows to a fork we carry forever, or an issue we
file and wait on.

**Development moved to a coding agent after 0.18.0.** The README's Use of AI
section states that 0.18.0 is the last release developed primarily without
coding agents, that future releases will come primarily from a coding agent, and
that changes will get *"at least a cursory review from a human, and a full review
from a coding agent."* The authorship record matches: all 52 commits between
`v0.18.0` and `origin/HEAD` carry `Claude <noreply@anthropic.com>` as author,
53 of the last 56 commits. The policy reversed sharply — a commit forbidding
AI-authored commits in May 2026 gave way to one allowing them on 2026-07-22, five
days after the pull-request door closed.

Output quality under that regime looks high from here. `f39068e` and `923f7a4`
reason from kernel sources and cite line numbers; `40e5006` diagnoses a real
parser hang. But for a filesystem client, a protocol bug is a data bug, and
"cursory" human review is the maintainer's own word. Three practices follow:

- **Pin exact versions.** Write `fuser = { version = "=0.18.0", ... }`, never
  `"0.18"`. A caret range lets a patch release we have not read reach the guest
  binary through a routine `cargo update`.
- **Read the release diff before any bump.** Not the CHANGELOG — the diff, with
  attention to `src/ll/request.rs`, `src/ll/reply.rs` and the
  `add_capabilities` list, which is where all three post-0.18.0 hazards in this
  note live.
- **File issues, do not send patches.** The tracker is the contribution channel
  now, and a coding agent is visibly working it: the maintainer's own August
  2025 ABI-gap checklist is what the recent commits have been clearing. A
  well-specified issue is plausibly the highest-leverage move available. Two are
  worth writing: an owned-buffer or `write_buf` callback (add to #298, which has
  sat unanswered since 2024-10-02), and `max_pages_limit` awareness or at least
  a read-back of what the kernel accepted (untracked today).

---

## Ranked recommendations

1. **Run the kill-priv plan on 0.15.1 now.** ~90 µs off a 296 µs write, four
   lines of rework at the later upgrade, and it makes released 0.18.0 safe for
   us by covering every signal fuser fails to forward.
2. **Take plan 1's Task 9 tripwire, plus the five-line `Explicit` override.**
   It costs almost nothing on top of the plan and turns a fuser bump that stops
   forwarding `write_flags` into a red test rather than a silent privilege
   leak. If the upgrade somehow jumps the queue, land a 35-40 line standalone
   version first instead.
3. **Build the readdir page ramp (B5) and the client write-buffer pool (B1's
   local half).** Two small, self-contained changes in our own code that attack
   the two costs the crate cannot help with: readdir round trips and a fresh
   heap block per write.
4. **Then 0.15.1 → 0.16.0 → 0.18.0, with `n_threads: None`.** Budget two to
   three days, expect no measured change, and take it for the API, the
   pre-epoch reply fix and the road to 0.19. Gate the bump on
   `make test-loopback` passing with the tripwire in it.

Everything else — splice, io_uring, the KILLPRIV_V2 patch branch, `inc_epoch` —
waits.

## Open questions

- With `DEFAULT_MAX_IO_SIZE` at 4 MiB and `fs.fuse.max_pages_limit` at 256, does
  the kernel issue reads above the negotiated ceiling that the multiplexer then
  refuses with `EINVAL`? The B4 probe surfaces the mismatch; only a VM run
  answers the question.
- Does raising `fs.fuse.max_pages_limit` past 256 on both guests move the
  streaming numbers, given that those shapes look per-megabyte-cost bound rather
  than per-operation bound? Worth one experiment before anyone prices splice.
- What is the guest's real memcpy rate? The copy attribution in B1 rests on an
  assumed 8 GB/s. A five-minute microbenchmark on the guest would firm up every
  number in that section.
- Does 0.19 land before we want `remaining_capacity()` and the kill-priv
  forwarding? If it slips past roughly the end of 2026, the four-commit patch
  branch in B3 becomes the cheaper option.
- Would a four-vCPU guest change the `n_threads` answer? Every measurement here
  comes from two vCPUs with tokio workers already on one of them.

## Addendum, pre-bump: the 0.18.0 diff read

Recorded before pinning `=0.18.0`, per the dependency-governance practice above.
Eight things in Part A did not survive contact with the tag. Two of them are
blockers rather than adjustments.

**Blockers.**

- `ForgetOne` is not nameable downstream. `src/lib.rs:34` imports it privately
  and `src/lib.rs:90` declares `mod forget_one;` private, so both
  `fuser::ForgetOne` and `fuser::forget_one::ForgetOne` are `error[E0603]`. The
  type appears in a public trait signature that no outside crate can write.
  Deleting our `batch_forget` override, rather than migrating it, is the only
  move available; the trait's default body loops the slice calling `forget`,
  which is what the override did.
- `BackgroundSession::join()` no longer unmounts. At 0.15.1 it dropped the
  `Mount` first and then joined (`src/session.rs:273-282`); at 0.18.0 it joins
  only, leaving the session thread parked on `/dev/fuse`. `umount_and_join()`
  is the replacement (`src/session.rs:571-576`).

**Adjustments.**

- `Config` is `#[non_exhaustive]`, so a struct expression from another crate is
  `error[E0639]` — `..Default::default()` does not help. Build it from
  `Config::default()` and assign each field.
- `lseek` keeps `offset: i64`, and `ReplyLseek::offset` keeps it too. Four
  `u64::try_from` guards go, not five.
- `rename` takes `RenameFlags`, `copy_file_range` takes `CopyFileRangeFlags`,
  and `setattr`'s trailing flags become `Option<BsdFileFlags>`. `RenameFlags`
  decodes with `from_bits_retain`, so unknown `renameat2` bits still reach the
  server verbatim.
- The dependency arrivals are here, not at 0.16.0, whose table is identical to
  0.15.1's. New to the tree: `num_enum`, `ref-cast`. Moved: `nix` 0.29 → 0.31
  with `poll`, `socket`, `uio`, `mount`, `process` and `ioctl` added.
- `entry_with_ttls` covers `ReplyEntry` only. `ReplyCreate::created` and
  `ReplyDirectoryPlus::add` still send one lifetime as both.
- This release leaves the nodeid-versus-`st_ino` conflation at
  `crates/lbfs-client/src/fuse.rs:29` in place: `entry_with_ttls` still
  derives the nodeid from `attr.ino`. The README limitation about `ls -i`
  stands.

**Unchanged and relied upon.** `add_capabilities` still checks the ask against
the kernel's advertised bits and nothing else — there is no refusal list on this
tag — so the `FUSE_HANDLE_KILLPRIV_V2` ask survives the bump. `MAX_WRITE_SIZE`
is still 16 MiB and `BUFFER_SIZE` is still `MAX_WRITE_SIZE + 4096`, one per
event-loop thread. `InitFlags::FUSE_HANDLE_KILLPRIV_V2` (bit 28),
`WriteFlags::FUSE_WRITE_KILL_SUIDGID` (bit 2) and
`FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES` (bit 6) all exist and match the
values the two earlier plans declare by hand.

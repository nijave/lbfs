# fuser Two-Step Upgrade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the client's FUSE dependency from `fuser` 0.15.1 to a pinned `=0.18.0` in two graded steps, drop the `libfuse3.so` link the first step retires, and take up the three things the second step makes reachable — split entry and attribute TTLs, the crate's own names for the two flag bits we spell by hand today, and an off-by-default knob for extra event-loop threads.

**Architecture:** Step one pins `=0.16.0`, which keeps every `Filesystem` signature byte-for-byte and asks for three source edits, then declares ABI 7.40 in a second commit. That step also flips `fuser`'s `libfuse` feature off, which swaps a shared-library mount for a pure-Rust one that shells out to `fusermount3`. Step two pins `=0.18.0`, where the trait takes `&self`, node ids and handles become newtypes, flag words become bitflags, and `Vec<MountOption>` becomes `Config`. The mechanical sweep lands as one commit with no behaviour change; each thing the new API buys lands as its own commit after it.

**Tech Stack:** Rust (edition 2021), tokio 1, fuser `=0.16.0` then `=0.18.0`, io-uring 0.7, rustix 1, postcard 1.1 + serde/serde_bytes, libc, clap 4, tracing, tempfile; Linux 7.0 guests under libvirt; fio 3.41 for the acceptance run.

**Spec:** `docs/superpowers/specs/2026-08-20-lbfs-design.md`

**Assessment this plan executes:** `docs/notes/2026-08-22-fuser-upgrade-assessment.md`

## Global Constraints

- **This plan runs fourth.** Two client plans land before it: `docs/superpowers/plans/2026-08-22-per-write-getattr-elimination.md`, then `docs/superpowers/plans/2026-08-22-parallel-direct-writes.md`. Every line number and code block below describes the tree those two leave behind. Section 1 of Design and Context gives the ordering argument.
- **Pin the exact version.** Write `fuser = { version = "=0.16.0", ... }` and later `"=0.18.0"`, never `"0.16"` or `"0.18"`. Upstream ships releases developed primarily by a coding agent under, in the maintainer's own words, "at least a cursory review from a human". A caret range lets a patch release nobody here has read reach the guest binary through a routine `cargo update`.
- **Read the release diff before every bump.** Not the CHANGELOG — the diff, with attention to `src/ll/request.rs`, `src/ll/reply.rs` and the `add_capabilities` list. Task 2 Step 1 and Task 5 make this a step with a written record rather than an intention.
- **The set-user-ID tripwire passes before and after every version change.** `privileged_bits_die_on_write_with_the_writeback_cache` and `privileged_bits_die_on_write_without_the_writeback_cache` in `tests/tests/loopback.rs` arrive with Task 9 of the per-write-getattr-elimination plan. Run both immediately before and immediately after each of the two `Cargo.toml` version edits. They are what turns a crate that stops forwarding a kill signal into a red test rather than a silent privilege leak.
- **Every task ends green:** `make check` (`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`) and `make test-loopback` both pass before every commit. Run `cargo fmt --all` first; the code blocks below carry rustfmt's output as best a document can, and a stray line width should cost a reformat rather than an argument.
- **The crate bump and the ABI-declaration bump are separate commits**, so reverting one does not drag the other. Task 2 pins `=0.16.0` at `abi-7-31`; Task 3 alone moves to `abi-7-40`.
- Frame header: exactly 24 bytes, little-endian, layout per spec §3.1.
- Protocol magic `LBFS`, version `2`, exact match on both ends. **No task here touches the version.** Nothing in this plan adds a wire field or an opcode, and `git diff` over the whole plan touches no file under `crates/lbfs-proto/` or `crates/lbfs-server/`.
- Names, symlink targets, xattr names and values travel as byte strings — never `String`.
- The RPC layer reaches storage only through the `FileSystem` trait (spec §5.1).
- TDD: write the failing test first for every behaviour. Where the change is a version bump, the compiler is the failing test — run it and read the errors before editing anything.
- No `unsafe` outside `crates/lbfs-server/src/fs/local/uring.rs`. `tests/tests/loopback.rs` carries `#![deny(unsafe_code)]` and keeps it.
- Commit after every task with the exact paths staged (no blanket `git add .`).

---

## Design and Context

Read this whole section before Task 1. It carries eight corrections to the assessment note, each one found by compiling or by reading the tag rather than by reasoning about it, and two of them would stop the migration dead if met for the first time mid-sweep.

Every claim below about `fuser` internals comes from the tag itself. Clone once and keep it; the tasks return to it:

```bash
git clone https://github.com/cberner/fuser /tmp/fuser-review
cd /tmp/fuser-review && git show v0.18.0:src/lib.rs | head
```

### 1. Where this plan sits among the four, and why

Four pieces of client work are in flight. The order holds, and each step of it has a reason that is not scheduling convenience.

| # | Plan | Why here |
|---|---|---|
| 1 | `2026-08-22-per-write-getattr-elimination.md` (kill-priv) | Largest measured win on the table — roughly 90 µs off a 296 µs 4 KiB write — and it costs about four lines of rework at each later bump. It also routes around released 0.18.0's kill-signal hole by construction, covering from the server side every signal the crate fails to forward, which is what makes 0.18.0 safe to pin at all. |
| 2 | `2026-08-22-parallel-direct-writes.md` | Needs no crate change: bit 6 of the `OPEN` reply's flag word, which fuser 0.15.1 copies out verbatim. Waiting for the upgrade buys it nothing, and its correctness argument rests on plan 1's server-side strip, so it must follow plan 1 and can precede this one. |
| 3 | This plan, step 1 (`=0.16.0`, ABI 7.40) | Cheap, reversible, and it retires the `libfuse3.so` link. Verified below by building the whole workspace and running both loopback suites against it. |
| 4 | This plan, step 2 (`=0.18.0`) | The real work. It rests on step 1 having already answered the `libfuse` feature question and on plans 1 and 2 having settled which raw constants exist, since step 2 is where two of them get deleted. |

The assessment's section A3 records that the parallel-writes plan "does not exist" anywhere in the checkout. That reading dates from a moment when the file was still in progress. The file exists, the history holds it, and its Task 2 leaves `fn open_flags(app_flags: i32) -> u32` plus a local `FOPEN_PARALLEL_DIRECT_WRITES` behind — the exact end state Task 6 below migrates.

**No step of this plan moves a measured number.** The bridge's own dispatch thread never exceeds 15.6% of a core in any run in `docs/benchmarks/2026-08-22-bottleneck-analysis.md` (line 223, the 1 MiB sequential write at 869 MB/s, where the dispatch thread leads every other client thread in the run). Treat both steps as API and bug-fix work whose payoff arrives one release later.

### 2. Step 1, verified by building it

The assessment's A1 table says the whole of 0.15.1 → 0.16.0 comes to three source lines. That claim holds. The check: copy the tree, apply exactly those three edits, run the gate.

| what ran | result |
|---|---|
| `cargo check --workspace --all-targets` | clean |
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace` | pass |
| `cargo test -p lbfs-tests --test loopback -- --ignored` | 18 passed, 0 failed |
| `cargo test -p lbfs-client --test loopback_cli -- --ignored` | 2 passed, 0 failed |

The three edits: `Cargo.toml:35`, `Capability { bit: u32 }` at `crates/lbfs-client/src/fuse.rs:336`, and `fn requested(writeback: bool) -> u32` at `fuse.rs:1521`. The plan below adds a fourth, because by the time it runs the kill-priv plan has put a local `const FUSE_HANDLE_KILLPRIV_V2: u32 = 1 << 28;` in the file and that word widens with the rest.

Why only those: on tag `v0.16.0` the INIT flag family alone became 64 bits wide. `pub const FUSE_DO_READDIRPLUS: u64`, `FUSE_READDIRPLUS_AUTO: u64`, `FUSE_ASYNC_DIO: u64`, `FUSE_WRITEBACK_CACHE: u64`, `FUSE_ATOMIC_O_TRUNC: u64` (`src/ll/fuse_abi.rs:167-191`), and `add_capabilities(u64) -> Result<(), u64>` (`src/lib.rs:260`). The `FOPEN_*` word and `FUSE_WRITE_KILL_PRIV` stay `u32` (`fuse_abi.rs:148-149`, `:239`), so nothing the parallel-writes plan or the kill-priv plan's `write()` comparison rests on moves.

### 3. The `libfuse` feature, and what turning it off changes

0.15.1 sets `default = ["libfuse"]`; 0.16.0 sets `default = []` (`Cargo.toml`, both tags). With the feature off on Linux, `build.rs` prints `cargo:rustc-cfg=fuser_mount_impl="pure-rust"` with no `pkg-config` probe at all, which the build above confirms — read `target/debug/build/fuser-*/output`. The pure-Rust path runs `fusermount3` as a child process (`src/mnt/fuse_pure.rs:113`, `:248`) for both the mount and the unmount.

Consequences, each one checked:

- **The build container stops needing `libfuse3-dev`.** `Makefile:49` installs it today; nothing links against it afterwards. Today's `target/guest/release/lbfs-client` reports `libfuse3.so.4` in `ldd`; after the change it reports no FUSE library at all.
- **The guests still need the `fuse3` package**, because `fusermount3` comes from it. `vm/lib.sh:53` lists it in `GUEST_PACKAGES` and `vm/test.sh:325` calls the binary directly. Leave that line alone.
- **The host still needs `fusermount3` for `make test-loopback`**, which it already did: `tests/tests/loopback.rs:116` and `crates/lbfs-client/tests/loopback_cli.rs:45` both assert the binary sits on `PATH`. Their failure messages credit libfuse3 for the shell-out and want rewording, since the shell-out becomes fuser's own.
- **Three behaviours the loopback suite covers could have differed** — `auto_unmount`, the `max_read=` custom option, and the unmount-on-drop ordering `crates/lbfs-client/src/main.rs:172` leans on. Every case in both suites passed on the pure-Rust path in the run above — 18 in `loopback` and 2 in `loopback_cli`, the counts on today's tree before the two earlier plans add theirs.
- **0.18.0 keeps the same answer.** Its `build.rs` selects `pure-rust` on Linux whenever the `libfuse` feature is off, so the decision made in step 1 carries forward untouched.

### 4. Step 2: the API map, checked against the tag and against a compiler

Signatures below come from `git show v0.18.0:src/lib.rs`. A throwaway crate depending on `fuser = "=0.18.0"` compiled every shape in Task 6's code blocks first, naming each type and calling each reply method. What follows describes code that builds, not a reading of a header.

| change | our sites |
|---|---|
| `&mut self` → `&self` | 33 of 35 callbacks; `init` and `destroy` keep `&mut self` |
| `Request<'_>` → `Request` | 34 (everything but `destroy`, which takes no request) |
| `ino`/`fh`/`parent`/`newparent` → `INodeNo`/`FileHandle` | 49 parameters, both newtypes being `pub struct X(pub u64)` |
| `reply.error(libc::E*)` → `Errno::*` | 8 direct `libc::E*` sites plus the `errno()` helper at `fuse.rs:495` |
| `u64::try_from(offset)` guards deleted | **4**, not 5 — see correction a |
| `as i64` / `as u64` casts on directory offsets deleted | 4: `fuse.rs:946`, `:969`, `:995`, `:1018` |
| `fuser::consts::{...}` import block deleted whole | 7 names by then — see correction h |
| `Capability { bit: u64 }` → `InitFlags` | struct, 5 entries, 6 tests |
| `mount_options() -> Vec<MountOption>` → `session_config() -> Config` | 1 function, 6 tests, 2 callers |
| `spawn_mount2` → `spawn_mount` | `main.rs:151`, `loopback.rs:341` |
| `BackgroundSession::join()` → `umount_and_join()` | `loopback.rs:434` — see correction b |
| `batch_forget` override **deleted** | `fuse.rs:639-643` — see correction c |
| `FileAttr.ino: INodeNo`, `generation: Generation` | `fuse.rs:147`, `:501`, `:822`, `:1022`, plus 3 tests |
| `open_flags() -> u32` → `FopenFlags` | 1 function, 4 tests |
| `init` returns `io::Result<()>` | 1 signature, 2 error returns |

Two callbacks keep a raw `i32` where their neighbours moved to bitflags: `create(flags: i32)` and `setxattr(flags: i32)`. The `flags as u32` casts at `fuse.rs:820` and `:1089` survive word for word. `open`, `read`, `write`, `release`, `opendir` and `releasedir` all take `OpenFlags`, which is `pub struct OpenFlags(pub i32)` — a public field, so `.0` reaches the number.

The 33 unwrappings do **not** need 33 argument lists rewritten. One line at the top of each callback — `let (ino, fh) = (ino.0, fh.map(|h| h.0));` — shadows the newtyped parameters with the raw numbers and leaves every body untouched. Task 6 uses that shape throughout.

### 5. Eight corrections to the assessment note

Compiling the code or reading the tag turned up each of these. Two are hard blockers.

**a. `lseek` keeps `offset: i64`.** The assessment counts five deleted `u64::try_from` guards, naming `read`, `write`, `fallocate`, `lseek` and `copy_file_range`. On the tag, `fn lseek(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: i64, whence: i32, reply: ReplyLseek)` still hands over a signed offset, and `ReplyLseek::offset` still takes one. Four guards go; `fuse.rs:1154` stays exactly as written, and so does the `off as i64` on the reply at `:1161`.

**b. `join()` at 0.18.0 does not unmount, and calling it would hang the loopback suite.** This is a blocker, not a return-type change. At 0.15.1, `BackgroundSession::join(self)` destructures the session, runs `drop(_mount)` — which unmounts — and only then joins the thread (`v0.15.1:src/session.rs:273-282`). At 0.18.0, `join(self) -> io::Result<()>` joins the thread and nothing else; the `mount: Option<Mount>` field is still live while `guard.join()` blocks, so the session thread waits on `/dev/fuse` forever. `umount_and_join()` is the replacement: it takes the `Mount`, unmounts, then joins (`v0.18.0:src/session.rs:571-576`). `tests/tests/loopback.rs:434` must call that one.

**c. No outside crate can name `ForgetOne`, so the `batch_forget` override has to go.** The assessment reads this as "`fuse_forget_one` → `ForgetOne`, fields → methods, 2 lines". On the tag, `src/lib.rs:34` says `use crate::forget_one::ForgetOne;` — a private import — and `src/lib.rs:90` says `mod forget_one;`, a private module. Neither `fuser::ForgetOne` nor `fuser::forget_one::ForgetOne` compiles downstream; both are `error[E0603]`, confirmed against the published crate. The type appears in a public trait method signature that nothing outside the crate can spell. Deleting our override is both the available answer and a behaviour-preserving one: the trait's default body loops the slice calling `self.forget(req, node.nodeid(), node.nlookup())`, and our `forget` is `self.conn.send_forget(ino, nlookup)`, which is what our override did per node.

**d. `Config` is `#[non_exhaustive]`, which forbids the struct expression the assessment sketches.** `..Default::default()` does not rescue a struct expression for a non-exhaustive type from another crate; the compiler answers `error[E0639]: cannot create non-exhaustive struct using struct expression`. The working shape starts from `Config::default()` and assigns each field, which Task 6 spells out.

**e. Three more callbacks move to typed flags than the assessment lists.** Beyond the `OpenFlags` group it names: `rename` takes `RenameFlags`, `copy_file_range` takes `CopyFileRangeFlags`, and `setattr`'s trailing `flags` becomes `Option<BsdFileFlags>`. Only `rename` reaches a value we use — `conn.rename(..., flags)` wants `flags.bits()`. The crate decodes that word with `from_bits_retain` (`src/ll/request.rs:1496-1497`), so unknown `renameat2` bits survive the round trip and `RENAME_WHITEOUT` or anything newer still reaches the server verbatim.

**f. The dependency audit belongs at step 2, not step 1.** The assessment flags 0.16.0 for pulling `nix` 0.29. That crate is already in `Cargo.lock` at 579 — 0.15.1 depends on it — and 0.16.0's `[dependencies]` table is byte-identical to 0.15.1's. The `abi-7-40` feature adds `nix/ioctl`, a feature of a crate already present. The real arrivals are at 0.18.0: `num_enum` and `ref-cast` are new to the tree, `nix` goes 0.29 → 0.31 and gains `poll`, `socket`, `uio`, `mount`, `process` and `ioctl`, and `bitflags` and `parking_lot` are already present at compatible versions.

**g. The entry-versus-attribute TTL complaint is not at `fuse.rs:29`.** That line belongs to a different conflation: the module doc's `st_ino` section, which records that `ReplyEntry::entry`, `ReplyCreate::created` and `ReplyDirectoryPlus::add` all derive the FUSE nodeid from `attr.ino`. That one 0.18.0 does **not** fix — `entry_with_ttls` still calls `new_entry(attr.ino, ...)` — so the `ls -i` limitation at `README.md:203` stands. The TTL statement this plan acts on is at `crates/lbfs-client/src/fuse.rs:465-467`: `/// Both the entry and the attribute timeout (spec §7). Zero disables kernel caching of both.` Spec §7 has always written it as two knobs, `entry_timeout`/`attr_timeout`; the code collapsed them because 0.15.1 offered one argument.

**h. The stale-prose site list is longer than the assessment's four.** Beyond `Makefile`, `README.md:13,31,36` and `vm/up.sh:127`: `docs/superpowers/specs/2026-08-20-lbfs-design.md:362,364,367` states that the container build needs `libfuse3-dev`, that the client links the guest's `libfuse3.so.4`, and that fuser's default features link libfuse3 — all three go false. `tests/tests/loopback.rs:117` and `:397` and `crates/lbfs-client/tests/loopback_cli.rs:46` credit libfuse3 for the shell-out and the lazy unmount, which becomes fuser's own pure-Rust path. And by the time this plan runs the `fuser::consts` import block holds seven names, not the five the assessment counted, because the kill-priv plan adds `FUSE_WRITE_KILL_PRIV` and the parallel-writes plan adds `FOPEN_DIRECT_IO`.

### 6. What the crate offers at 0.18.0 that we take, and one hazard we miss by accident

`InitFlags::FUSE_HANDLE_KILLPRIV_V2 = 1 << 28` (`src/ll/flags/init_flags.rs:66`), `WriteFlags::FUSE_WRITE_KILL_SUIDGID = 1 << 2` (`write_flags.rs`) and `FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES = 1 << 6` (`fopen_flags.rs`) all exist on the tag. Both raw constants the earlier plans declare become crate-provided names, which Task 7 takes up.

`add_capabilities` on 0.18.0 compares the ask against the kernel's advertised set and nothing else (`src/lib.rs:342-350`); there is no `UNSUPPORTED_CAPABILITIES` list on the tag. That keeps the kill-priv ask working across the bump, and the tripwire in the Global Constraints is what says so out loud.

One hazard sits in released 0.18.0 and we miss it by accident: `FUSE_SETXATTR_EXT` is negotiable while the parser still reads the old 8-byte layout, which panics the session thread and wedges the mount. We never ask for that capability. The lesson is the practice, not the bug — the `add_capabilities` list earns a careful read at every bump, which is why Task 3 adds a test that fails if anything above bit 25 other than `FUSE_HANDLE_KILLPRIV_V2` ever appears in the ask.

### 7. The two TTLs, and how far the split reaches

`ReplyEntry::entry_with_ttls(&attr_ttl, &entry_ttl, &attr, generation)` — note that order, attribute lifetime first — is the only reply path on 0.18.0 that separates them. `ReplyCreate::created` still takes one `ttl` and sends it as both, and `ReplyDirectoryPlus::add` does the same (`src/reply.rs`, `new_create` and `DirEntryPlus::new` each receive `*ttl` twice). That leaves the split reaching `lookup`, `mkdir`, `symlink` and `link` — the four callbacks routed through `reply_entry` — and nothing else.

That is still worth having. A build workload re-resolves the same paths far more often than it re-stats them, so a long name lifetime with a short attribute lifetime cuts `LOOKUP` traffic without letting a stale size or mode sit around. Where the split does not reach, both numbers stay at the attribute lifetime, which is today's behaviour exactly. Task 8 ships it defaulting to the attribute timeout, so a mount that names no new flag behaves as it does now.

### 8. What `n_threads` and `clone_fd` would buy, and what they cost

`Config { n_threads, clone_fd }` exists on the tag. `Session::run` spawns `n_threads` event loops (`src/session.rs:257-294`), each with its own `FuseReadBuf`, and `clone_fd: true` gives each one a private `/dev/fuse` descriptor through `FUSE_DEV_IOC_CLONE`.

Our bridge already spawns every callback onto tokio, so a second event loop adds only a second reader of `/dev/fuse`. The measurements say that reader is not the constraint anywhere:

| shape | what the session thread costs | headroom above it |
|---|---|---|
| seq read 1 MiB, 1544 MB/s | not the busiest thread; a tokio worker leads at 27.9% of a core | client box 61.8% idle |
| seq write 1 MiB, 869 MB/s | 15.6% of a core — the busiest client thread in that run | client box 73.6% idle |
| randwrite 4k, 3463 IOPS | client total 30.6%, busiest thread a tokio worker at 12.8% | client box 77.9% idle |

The memory bill is real: `BUFFER_SIZE = MAX_WRITE_SIZE + 4096` and `MAX_WRITE_SIZE` is 16 MiB on 0.18.0, unchanged from 0.15.1 (`src/session.rs:55`, `src/read_buf.rs:8`). The buffer does not shrink to the negotiated `max_write`, so each thread holds 16 MiB resident. Four threads reserve 64 MiB on a 1962 MB guest, about 3%, on a guest whose write throughput already swings fourfold on server page-cache pressure.

Task 9 ships the knob off and Task 10 measures it. The expected reading on today's shapes is no change: the guest has two vCPUs, one of them already carrying tokio workers, so a second event loop competes rather than adds. Record the numbers either way — the point of the knob is that a four-vCPU guest earns a measurement later without another code change, and a measurement nobody wrote down is one somebody repeats.

---

## File Map

| Path | Change |
|---|---|
| `Cargo.toml` | `fuser` pin moves `0.15` → `=0.16.0` (Task 2) → `abi-7-40` (Task 3) → `=0.18.0` (Task 6); the comment above it changes twice |
| `crates/lbfs-client/src/fuse.rs` | flag word widens to `u64` (Task 2); a capability-range test (Task 3); the whole 0.18.0 sweep (Task 6); two local constants deleted (Task 7); `entry_ttl` field and `entry_with_ttls` (Task 8); `session_config` gains two parameters (Task 9) |
| `crates/lbfs-client/src/main.rs` | `spawn_mount` and `session_config` (Task 6); `--entry-timeout` (Task 8); `--fuse-threads`, `--fuse-clone-fd`, `event_loop_threads` (Task 9) |
| `tests/tests/loopback.rs` | the drain case (Task 1); the libfuse wording (Task 4); `spawn_mount`, `session_config`, `umount_and_join` (Task 6); `Opts.entry_ttl` (Task 8) |
| `crates/lbfs-client/tests/loopback_cli.rs` | the libfuse wording (Task 4) |
| `Makefile` | `build-guest` stops installing `libfuse3-dev` (Task 4) |
| `README.md` | build prerequisites, the musl section, the client flag table, the dependency-governance paragraph (Tasks 4, 8, 9, 11) |
| `docs/superpowers/specs/2026-08-20-lbfs-design.md` | §7 the two TTLs; §9 the deployment paragraph; §12 the pinned version and the governance practice (Tasks 4, 8, 11) |
| `docs/notes/2026-08-22-fuser-upgrade-assessment.md` | the 0.18.0 pre-bump diff record (Task 5) |
| `docs/benchmarks/2026-08-22-bottleneck-analysis.md` | the post-upgrade measurement (Task 10) |

---

### Task 1: Loopback — every write reaches the export by the time the unmount returns

**Files:**
- Edit: `tests/tests/loopback.rs` (a new case beside `file_content_round_trips`)

**Interfaces:**
- Consumes: the existing `Loopback` fixture, `Opts`, `lb.mnt()`, `lb.export()`, `lb.unmount()`.
- Produces: `fn writes_reach_the_export_by_the_time_the_unmount_returns(writeback: bool)` plus two `#[test]` wrappers.

This case lands **before** any version change, on 0.15.1, so the two bumps have a before-and-after to fail against. `crates/lbfs-client/src/main.rs:169-173` treats `drop(session)` as "unmount, drain, exit" — `umount(2)` syncs the superblock, so the kernel writes back every dirty page as ordinary `WRITE` callbacks, serviced by a session thread that is still running against a connection that is still open. Nothing in the suite names that contract today.

This case also catches correction b. If Task 6 reaches for `join()` where 0.15.1 had `join()`, the session thread never ends, `Loopback::try_unmount` times out, and this goes red with a message that says so.

- [ ] **Step 1: Write the test**

Add to `tests/tests/loopback.rs`, beside the `file_content_round_trips` pair:

```rust
/// Unmounting is the drain, and the drain is what the shipped binary leans on.
///
/// `crates/lbfs-client/src/main.rs` treats `drop(session)` as "unmount, drain,
/// exit": `umount(2)` syncs the superblock before it detaches, so whatever the
/// client kernel still holds comes back through this bridge as ordinary
/// `WRITE` callbacks, on a session thread that is still running and a
/// connection that is still open. Every other case in this file reads its data
/// back through the mount, which proves the round trip and not the teardown.
/// This one reads only from the export, and only after the unmount has
/// returned, so a teardown that detached early would show up as missing bytes
/// rather than as a passing test.
///
/// It doubles as the guard on the unmount path itself. `Loopback::unmount`
/// gives the session thread a bounded time to end; a crate whose unmount stops
/// waking that thread fails here with a timeout instead of hanging the run.
fn writes_reach_the_export_by_the_time_the_unmount_returns(writeback: bool) {
    let mut lb = Loopback::start(Opts {
        writeback,
        ..Opts::default()
    });
    lb.wait_ready();

    // Many small files plus one large one: the small ones exercise the
    // per-file teardown path, the large one spans enough pages that the
    // writeback thread, rather than the closing descriptor, carries some of it.
    let mut expected: Vec<(std::path::PathBuf, Vec<u8>)> = Vec::new();
    for i in 0..64u8 {
        let body = vec![i; 64 << 10];
        std::fs::write(lb.mnt().join(format!("small-{i}")), &body).unwrap();
        expected.push((lb.export().join(format!("small-{i}")), body));
    }
    let big: Vec<u8> = (0..(8u32 << 20)).map(|n| (n % 251) as u8).collect();
    std::fs::write(lb.mnt().join("big"), &big).unwrap();
    expected.push((lb.export().join("big"), big));

    lb.unmount();

    for (path, body) in expected {
        let landed = std::fs::read(&path).unwrap_or_else(|e| {
            panic!("{} is missing after the unmount: {e}", path.display())
        });
        assert_eq!(
            landed.len(),
            body.len(),
            "{} is short after the unmount",
            path.display()
        );
        assert!(landed == body, "{} holds the wrong bytes", path.display());
    }
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn writes_reach_the_export_on_unmount_with_the_writeback_cache() {
    writes_reach_the_export_by_the_time_the_unmount_returns(true);
}

#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn writes_reach_the_export_on_unmount_without_the_writeback_cache() {
    writes_reach_the_export_by_the_time_the_unmount_returns(false);
}
```

- [ ] **Step 2: Run the two new cases**

Run: `cargo test -p lbfs-tests --test loopback writes_reach_the_export -- --ignored --test-threads=1`
Expected: PASS, both. They pass on today's tree by design — this is a characterisation test, and its job starts at the next two commits.

- [ ] **Step 3: Run the whole loopback suite**

Run: `make test-loopback`
Expected: PASS, the two new cases among them, and no regression in the existing ones. The suite's size depends on how many cases the two earlier plans added, so count regressions rather than totals.

- [ ] **Step 4: Commit**

```bash
git add tests/tests/loopback.rs
git commit -m "test(loopback): the unmount drains before it returns"
```

---

### Task 2: Pin `fuser` at `=0.16.0` and widen the INIT flag word

**Files:**
- Edit: `Cargo.toml:35` (the pin; leave the feature list at `abi-7-31`)
- Edit: `crates/lbfs-client/src/fuse.rs` (`Capability.bit`, the kill-priv constant, `fn requested`)

**Interfaces:**
- Consumes: the tree the two earlier plans leave — `const FUSE_HANDLE_KILLPRIV_V2: u32 = 1 << 28;` above `fn capabilities`, `Capability { bit: u32, name, required }`, `fn requested(writeback: bool) -> u32` in the test module.
- Produces: the same three items with `u64` in place of `u32`, and `fuser = { version = "=0.16.0", features = ["abi-7-31"] }`.

- [ ] **Step 1: Read the release diff and write down what it says**

```bash
git clone https://github.com/cberner/fuser /tmp/fuser-review 2>/dev/null || true
cd /tmp/fuser-review && git fetch --tags
git diff v0.15.1..v0.16.0 --stat | tail -5
git diff v0.15.1..v0.16.0 -- src/ll/request.rs src/ll/reply.rs | wc -l
git diff v0.15.1..v0.16.0 -- src/lib.rs | grep -n "add_capabilities" -A 12
git show v0.16.0:Cargo.toml | sed -n '/^\[features\]/,/^\[\[/p'
```

What to confirm before going on, in this order: `pub trait Filesystem` still takes `&mut self` and `Request<'_>`; `add_capabilities` reads `u64`; `default = []` where 0.15.1 had `default = ["libfuse"]`; and `abi-7-40 = ["abi-7-36", "nix/ioctl"]`. Paste the four answers into the commit message body at Step 8. If any of them has changed, stop and re-price the task rather than editing.

- [ ] **Step 2: Run the tripwire on the pre-bump tree**

Run: `cargo test -p lbfs-tests --test loopback privileged_bits -- --ignored --test-threads=1`
Expected: PASS, both cases. This is the "before" half of the Global Constraint. A red result here means something earlier broke, and this task must not start.

- [ ] **Step 3: Pin the crate**

In `Cargo.toml`, replace line 35 only, leaving the comment block above it as it stands:

```toml
fuser = { version = "=0.16.0", features = ["abi-7-31"] }
```

- [ ] **Step 4: Let the compiler name the damage**

Run: `cargo check -p lbfs-client 2>&1 | grep -E "^error" | head`
Expected: `error[E0308]: mismatched types` at the `add_capabilities` call and at the `Capability` entries — the `u32`/`u64` mismatch and nothing else.

- [ ] **Step 5: Widen the three words**

In `crates/lbfs-client/src/fuse.rs`, change the field on `struct Capability`:

```rust
struct Capability {
    bit: u64,
    name: &'static str,
    /// Whether a kernel without it makes the mount wrong rather than merely
    /// slow.
    required: bool,
}
```

Change the local constant the kill-priv plan added, and extend its doc comment with the reason the type moved:

```rust
/// `FUSE_HANDLE_KILLPRIV_V2`, which fuser 0.16.0 does not name.
///
/// fuser's `consts` stops at `FUSE_HANDLE_KILLPRIV` (bit 19). Bit 28 arrived
/// with ABI 7.33 and fuser negotiates at most 7.40 while naming no constant
/// between bits 26 and 36 — but the kernel does not check the minor version
/// for this flag. `process_init_reply` reads it inside one `if (arg->minor >=
/// 6)` and applies it with no further guard (`fs/fuse/inode.c:1411-1414`),
/// exactly as it does for `FUSE_ASYNC_DIO`, and `fuse_new_init` offers it
/// unconditionally (`inode.c:1505`). `KernelConfig::add_capabilities` checks
/// the ask against the kernel's own offered bits rather than a list of names
/// fuser knows, so a locally declared constant is the whole mechanism.
///
/// The word is `u64` from 0.16.0 on: that release widened the whole INIT flag
/// family and `add_capabilities` with it, to make room for the bits above 31
/// that `fuse_init_in.flags2` carries.
const FUSE_HANDLE_KILLPRIV_V2: u64 = 1 << 28;
```

And in the test module, the helper at line 1521:

```rust
    fn requested(writeback: bool) -> u64 {
        capabilities(writeback).iter().fold(0, |all, c| all | c.bit)
    }
```

- [ ] **Step 6: Run the gate**

Run: `cargo fmt --all && make check`
Expected: PASS. `cargo test --workspace` reports no failures; clippy is silent.

- [ ] **Step 7: Run both loopback suites, tripwire included**

Run: `make test-loopback`
Expected: PASS, every case in both suites, `privileged_bits_die_on_write_*` and `writes_reach_the_export_on_unmount_*` included.

- [ ] **Step 8: Commit the crate bump alone**

```bash
git add Cargo.toml Cargo.lock crates/lbfs-client/src/fuse.rs
git commit -m "build(deps): pin fuser at =0.16.0

The INIT flag family widened to u64 in that release, so Capability::bit,
the local FUSE_HANDLE_KILLPRIV_V2 constant and the test helper widen with
it. Nothing else moves: the Filesystem trait still takes &mut self and
Request<'_>, node ids and handles are still u64, and init still returns
Result<(), c_int>.

Release diff read before the bump: src/ll/request.rs and src/ll/reply.rs
carry no signature change our bridge can see; add_capabilities is the one
public signature that moved; default features drop libfuse; abi-7-40 pulls
nix/ioctl. Pinned exactly rather than as a range, per the dependency
governance note in docs/notes/2026-08-22-fuser-upgrade-assessment.md."
```

---

### Task 3: Declare ABI 7.40

**Files:**
- Edit: `Cargo.toml:27-35` (the comment block and the feature list)
- Edit: `crates/lbfs-client/src/fuse.rs` (one new test in the test module)

**Interfaces:**
- Consumes: Task 2's pin.
- Produces: `features = ["abi-7-40"]`, and `fn the_only_high_capability_asked_for_is_killpriv_v2()` in the test module.

Announcing 7.40 tells the kernel this client understands every feature up to that level, while 0.16.0 names no INIT constant between bit 26 and bit 36 — the only ones above bit 25 on the tag are `FUSE_INIT_EXT` (30), `FUSE_INIT_RESERVED` (31) and `FUSE_PASSTHROUGH` (37). That gap is harmless because every feature in it turns on an INIT flag nobody asks for, and the kernel turns on nothing nobody asked for. Bit 28 is the single exception, and this client asks for it on purpose. The new test is what keeps that argument true after somebody adds a capability without reading this paragraph.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/lbfs-client/src/fuse.rs`, beside the other capability tests:

```rust
    /// Declaring ABI 7.40 is a claim about what this client understands, and
    /// fuser 0.16.0 names no INIT constant between bit 26 and bit 36 — above
    /// bit 25 the tag carries only `FUSE_INIT_EXT` (30), `FUSE_INIT_RESERVED`
    /// (31) and `FUSE_PASSTHROUGH` (37). The claim holds because a feature
    /// nobody asks for is a feature the kernel leaves off. Bit 28 is the one
    /// high bit this client does ask for, declared locally and answered by the
    /// server's own set-user-ID strip. Anything else appearing up here is a
    /// feature being negotiated with no code behind it.
    #[test]
    fn the_only_high_capability_asked_for_is_killpriv_v2() {
        for writeback in [true, false] {
            let high = requested(writeback) & !((1u64 << 26) - 1);
            assert_eq!(
                high, FUSE_HANDLE_KILLPRIV_V2,
                "an unexpected capability above bit 25 (writeback={writeback})"
            );
        }
    }
```

- [ ] **Step 2: Run it to see it pass on the current ask**

Run: `cargo test -p lbfs-client --lib the_only_high_capability`
Expected: PASS. The test is a guard rather than a driver here; it fails the moment the capability list grows a high bit, which is the point.

- [ ] **Step 3: Declare 7.40**

In `Cargo.toml`, replace the comment block at lines 27-34 and the pin at line 35 together:

```toml
# The ABI level is a claim about what this client understands, and fuser gates
# its own constants on it. Every kernel-cache knob spec §7 asks for lives above
# the 7.8 default: `FUSE_DO_READDIRPLUS`/`FUSE_READDIRPLUS_AUTO` are 7.21,
# `FUSE_WRITEBACK_CACHE` is 7.23, `batch_forget` is 7.16, and `max_pages` (what
# carries a negotiated `max_write` past 128 KiB) is 7.28. 7.40 is the highest
# level 0.16.0 offers, and it costs nothing above 7.31 today: the release names
# no constant between bits 26 and 36, and a feature nobody asks for is one the
# kernel leaves off. `the_only_high_capability_asked_for_is_killpriv_v2` in
# `fuse.rs` is what keeps that true. The kernel negotiates down to whatever it
# supports, so asking for 7.40 costs nothing on an older one.
#
# `libfuse` is deliberately absent from the feature list. 0.16.0 made it opt-in
# (`default = []`), and leaving it off selects fuser's pure-Rust mount, which
# runs `fusermount3` instead of linking `libfuse3.so`. See README "Why a
# container and not a musl target".
fuser = { version = "=0.16.0", features = ["abi-7-40"] }
```

- [ ] **Step 4: Run the gate**

Run: `cargo fmt --all && make check`
Expected: PASS. `Cargo.lock` gains the `ioctl` feature on the `nix` entry already present at line 579; no new crate appears.

- [ ] **Step 5: Confirm no libfuse link survives**

Run: `cargo build --release -p lbfs-client && ldd target/release/lbfs-client | grep -i fuse || echo "no fuse library"`
Expected: `no fuse library`.

- [ ] **Step 6: Run both loopback suites, tripwire included**

Run: `make test-loopback`
Expected: PASS. This is the run that proves the pure-Rust mount path behaves: `auto_unmount`, the `max_read=` option, the lazy unmount and the drain all go through it now.

- [ ] **Step 7: Commit the ABI bump alone**

```bash
git add Cargo.toml Cargo.lock crates/lbfs-client/src/fuse.rs
git commit -m "build(deps): declare FUSE ABI 7.40

Separate from the crate bump so a revert of one does not drag the other.
0.16.0 names no INIT constant between bits 26 and 36, which is safe only
while this client asks for nothing in that range beyond bit 28; the new
test says so and fails if that stops being true."
```

---

### Task 4: Drop libfuse3 from the build, and correct the prose it leaves behind

**Files:**
- Edit: `Makefile:23-51`
- Edit: `README.md:13`, `README.md:29-38`
- Edit: `docs/superpowers/specs/2026-08-20-lbfs-design.md:360-367`
- Edit: `vm/up.sh:127`
- Edit: `tests/tests/loopback.rs:115-119`, `tests/tests/loopback.rs:396-402`
- Edit: `crates/lbfs-client/tests/loopback_cli.rs:44-48`

**Interfaces:**
- Consumes: Task 3's feature list.
- Produces: a `build-guest` target that installs nothing, and six documents that describe the mount the client actually performs.

The client no longer links `libfuse3.so.4`. It runs `fusermount3` as a child process, which the guests already carry through the `fuse3` package (`vm/lib.sh:53`) and the host already needs for `make test-loopback`. **Leave `GUEST_PACKAGES` alone** — dropping `fuse3` would remove the binary the mount now runs.

- [ ] **Step 1: Strip the build container**

In `Makefile`, replace the comment block and the `build-guest` recipe at lines 23-51:

```make
# Binaries for the VM guests, built in a container rather than here.
#
# Not musl, and not the host toolchain: a distro-packaged rustc has no musl std
# to build against. A container with the guests' own libc family is both
# simpler and closer to what the guests run — Debian's glibc is older than
# Ubuntu 26.04's, which is the direction that works.
#
# The container installs nothing. The io-uring crate issues raw syscalls, so
# liburing is not involved, and since fuser 0.16.0 the client mounts through
# its pure-Rust path — it runs `fusermount3` rather than linking libfuse3, so
# there are no FUSE headers to find at build time and no FUSE library to load
# at run time. The guests still need the `fuse3` package for the `fusermount3`
# binary itself; `vm/lib.sh` asks for it.
#
# The registry cache is a named volume and `target/guest` is inside the mounted
# checkout, so the second build is a rebuild rather than a redownload. SELinux
# labelling is switched off for the mount instead of relabelling the checkout
# out from under the developer.
GUEST_IMAGE ?= docker.io/library/rust:1-trixie

build-guest:
	podman run --rm \
	  --security-opt label=disable \
	  -v "$(CURDIR)":/work \
	  -v lbfs-guest-cargo:/usr/local/cargo/registry \
	  -w /work \
	  $(GUEST_IMAGE) \
	  bash -euc 'RUSTUP_TOOLCHAIN=$$(rustup default | cut -d" " -f1) \
	    cargo build --release --target-dir target/guest -p lbfs-server -p lbfs-client'
```

- [ ] **Step 2: Build for the guests and check the link**

Run: `make build-guest && ldd target/guest/release/lbfs-client | grep -i fuse || echo "no fuse library"`
Expected: `no fuse library`. Before this change the same command printed `libfuse3.so.4 => not found` on a Fedora host, because the guest links a library the host does not carry.

- [ ] **Step 3: Correct the README**

In `README.md`, replace line 13's sentence:

```markdown
The host build needs a stable Rust toolchain and nothing else. `make check`
runs the standard gate — `cargo fmt --check`, `cargo clippy -D warnings`, and
the workspace tests:
```

and replace the body of the "Why a container and not a musl target" section at lines 31-38:

```markdown
A distro-packaged rustc has no musl std to build against, so a static musl
binary is not on the table on this host. A container running the guests' own
libc family solves that and stays closer to what the guests run. Debian
trixie's glibc is older than Ubuntu 26.04's, which is the direction that works.
Neither binary links anything beyond libc: the `io-uring` crate issues raw
syscalls, and the client mounts through fuser's pure-Rust path, which runs
`fusermount3` as a child process rather than linking `libfuse3.so`. The guests
still install the `fuse3` package, because that is where `fusermount3` comes
from.
```

- [ ] **Step 4: Correct the spec's deployment paragraph**

In `docs/superpowers/specs/2026-08-20-lbfs-design.md`, replace the sentences at lines 360-367:

```markdown
**Deployment:** guest binaries come from a container build
(`make build-guest`: podman + a Debian-based rust image, gnu target,
`target/guest`). Debian's older glibc runs forward-compatibly on the
Ubuntu guests (max required symbol GLIBC_2.34 vs guest 2.43). Neither
binary links anything past libc: the io-uring crate is direct syscalls,
and from fuser 0.16.0 the client mounts through the crate's pure-Rust
path, running `fusermount3` from the guests' `fuse3` package instead of
linking `libfuse3.so`. The original static-musl plan died on one host
fact that still holds: Fedora's packaged Rust ships no musl std.
```

- [ ] **Step 5: Correct the VM comment**

In `vm/up.sh`, replace line 127 inside the package-check comment:

```sh
  # suite and the client's mount path depend on, and refuse to report a
```

- [ ] **Step 6: Correct the two test messages**

In `tests/tests/loopback.rs`, replace the second assertion of `require_fuse()`:

```rust
    assert!(
        which("fusermount3").is_some(),
        "the loopback suite needs `fusermount3` on PATH: fuser's pure-Rust \
         mount path runs it for both the unprivileged mount and the unmount. \
         Install fuse3."
    );
```

and the paragraph in the `unmount` doc comment at line 397:

```rust
    /// **Every file opened on the mount must be closed first.** `fusermount3`
    /// unmounts with `MNT_DETACH`, which takes the mountpoint out of the mount
```

In `crates/lbfs-client/tests/loopback_cli.rs`, replace the same assertion:

```rust
    assert!(
        which("fusermount3").is_some(),
        "this test needs `fusermount3` on PATH: fuser's pure-Rust mount path \
         runs it for the mount and the unmount. Install fuse3."
    );
```

- [ ] **Step 7: Run the gate and both loopback suites**

Run: `make check && make test-loopback`
Expected: PASS.

- [ ] **Step 8: Deploy and run the end-to-end suite**

Run: `make vm-deploy && make vm-test`
Expected: every PASS line, including the fio `crc32c` verify job and the disconnect drill. If the pair is down, `make vm-up` first. This is the step that proves a binary with no libfuse link mounts on a real guest.

- [ ] **Step 9: Commit**

```bash
git add Makefile README.md docs/superpowers/specs/2026-08-20-lbfs-design.md \
  vm/up.sh tests/tests/loopback.rs crates/lbfs-client/tests/loopback_cli.rs
git commit -m "build: drop the libfuse3 link, and say so everywhere it was claimed"
```

---

### Task 5: Read the 0.18.0 release diff and record what it says

**Files:**
- Edit: `docs/notes/2026-08-22-fuser-upgrade-assessment.md` (append one section)

**Interfaces:**
- Consumes: nothing.
- Produces: a dated record of the diff read, and the eight corrections in a place the next reader will find.

This is the Global Constraint made into a step. The record matters more than usual here, because the note it appends to is the document the next bump takes its pricing from, and its A2 table carries five claims that do not survive contact with the tag.

- [ ] **Step 1: Read the three areas that matter**

```bash
cd /tmp/fuser-review
git diff v0.16.0..v0.18.0 --stat | tail -5
git diff v0.16.0..v0.18.0 -- src/ll/request.rs | grep -E "^[-+].*fn |^[-+].*from_bits" | head -40
git diff v0.16.0..v0.18.0 -- src/ll/reply.rs src/reply.rs | grep -E "^[-+].*pub fn " | head -40
git show v0.18.0:src/lib.rs | sed -n '/pub fn add_capabilities/,/^    }/p'
git show v0.18.0:Cargo.toml | sed -n '/^\[dependencies\]/,/^\[/p'
```

- [ ] **Step 2: Confirm each line of the record below against what you just read**

Writing this plan meant checking every statement in Step 3's block against tag `v0.18.0`. Read the diff yourself and confirm each one; correct any that upstream has since changed, and if a correction lands, re-price Task 6 before starting it.

- [ ] **Step 3: Append the record**

Add to the end of `docs/notes/2026-08-22-fuser-upgrade-assessment.md`:

```markdown
## Addendum, pre-bump: the 0.18.0 diff read

Recorded before pinning `=0.18.0`, per the dependency-governance practice above.
Eight things in Part A did not survive contact with the tag. Two of them are
blockers rather than adjustments.

**Blockers.**

- `ForgetOne` is not nameable downstream. `src/lib.rs:34` imports it privately
  and `src/lib.rs:90` declares `mod forget_one;` private, so both
  `fuser::ForgetOne` and `fuser::forget_one::ForgetOne` are `error[E0603]`. The
  type appears in a public trait signature that no outside crate can write.
  Our `batch_forget` override is therefore deleted rather than migrated; the
  trait's default body loops the slice calling `forget`, which is what the
  override did.
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
- The nodeid-versus-`st_ino` conflation at `crates/lbfs-client/src/fuse.rs:29`
  is untouched by this release: `entry_with_ttls` still derives the nodeid from
  `attr.ino`. The README limitation about `ls -i` stands.

**Unchanged and relied upon.** `add_capabilities` still checks the ask against
the kernel's advertised bits and nothing else — there is no refusal list on this
tag — so the `FUSE_HANDLE_KILLPRIV_V2` ask survives the bump. `MAX_WRITE_SIZE`
is still 16 MiB and `BUFFER_SIZE` is still `MAX_WRITE_SIZE + 4096`, one per
event-loop thread. `InitFlags::FUSE_HANDLE_KILLPRIV_V2` (bit 28),
`WriteFlags::FUSE_WRITE_KILL_SUIDGID` (bit 2) and
`FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES` (bit 6) all exist and match the
values the two earlier plans declare by hand.
```

- [ ] **Step 4: Check the prose gate**

Run: `vale --output=line docs/notes/2026-08-22-fuser-upgrade-assessment.md`
Expected: no output.

- [ ] **Step 5: Commit**

```bash
git add docs/notes/2026-08-22-fuser-upgrade-assessment.md
git commit -m "docs: record the 0.18.0 diff read before the bump"
```

---

### Task 6: Pin `fuser` at `=0.18.0` and migrate the bridge

**Files:**
- Edit: `Cargo.toml:27-35`
- Edit: `crates/lbfs-client/src/fuse.rs` (imports, helpers, the whole `Filesystem` block, the test module)
- Edit: `crates/lbfs-client/src/main.rs:148-155`
- Edit: `tests/tests/loopback.rs:64`, `:341`, `:434`

**Interfaces:**
- Consumes: Tasks 1-5.
- Produces: `pub fn session_config(max_io_size: u32, allow_other: bool, auto_unmount: bool) -> fuser::Config` replacing `pub fn mount_options(...) -> Vec<MountOption>`; `fn open_flags(app_flags: OpenFlags) -> FopenFlags`; `fn errno(e: Errno) -> FuseErrno`; `struct Capability { bit: InitFlags, name: &'static str, required: bool }`; `fn requested(writeback: bool) -> InitFlags` in the test module.

One commit, and one behaviour change: none. Everything the new API makes possible waits for Tasks 7 through 9, so a reviewer reading this diff is checking a translation and nothing else.

- [ ] **Step 1: Run the tripwire on the pre-bump tree**

Run: `cargo test -p lbfs-tests --test loopback privileged_bits -- --ignored --test-threads=1`
Expected: PASS, both cases.

- [ ] **Step 2: Pin the crate**

In `Cargo.toml`, replace the comment block and pin from Task 3 with:

```toml
# Pinned exactly, never as a range. Releases after 0.18.0 come primarily from a
# coding agent under, in the maintainer's own words, "at least a cursory review
# from a human", and for a filesystem client a protocol bug is a data bug. Read
# the diff before moving this number; see the governance section and the
# pre-bump addendum in `docs/notes/2026-08-22-fuser-upgrade-assessment.md`.
#
# The `abi-7-*` features are gone as of this release — the ABI level is 7.40,
# fixed, with compatibility settled at run time against what the kernel offers.
# `libfuse` stays off, which selects the crate's pure-Rust mount: it runs
# `fusermount3` as a child process instead of linking `libfuse3.so`.
fuser = { version = "=0.18.0" }
```

- [ ] **Step 3: Read the wall of errors once, before editing**

Run: `cargo check -p lbfs-client 2>&1 | grep -c "^error"`
Expected: a three-digit number. Read the first twenty with `cargo check -p lbfs-client 2>&1 | head -80` so the shapes below are recognisable rather than surprising.

- [ ] **Step 4: Replace the import block**

In `crates/lbfs-client/src/fuse.rs`, replace lines 47-67 — the whole `use` block, `fuser::consts` included — with:

```rust
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// `fuser::Errno` and `lbfs_proto::Errno` are both in scope here and both are
// spelled `Errno`. The wire one keeps the bare name, because it is the one
// this module handles by the dozen; the kernel-facing one is aliased. Renaming
// the wire type instead would ripple into every `Result<_, Errno>` signature in
// the crate for the sake of one conversion function.
use fuser::{
    BsdFileFlags, Config, CopyFileRangeFlags, Errno as FuseErrno, FileHandle, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry,
    ReplyLseek, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, SessionACL, TimeOrNow,
    WriteFlags,
};
use lbfs_proto::ops::CopyFileRangeRequest;
use lbfs_proto::types::{
    DirEntryPlus, Entry, FileAttr, FileKind, NodeId, SetattrArgs, StatfsReply, TimeSet,
};
use lbfs_proto::Errno;

use crate::conn::Connection;
```

- [ ] **Step 5: Retype the two local constants and the capability list**

The two constants keep their raw values for this commit; Task 7 is where the question of trusting the crate's own names gets asked. Replace the declaration the kill-priv plan left:

```rust
/// `FUSE_HANDLE_KILLPRIV_V2`, bit 28 per `include/uapi/linux/fuse.h`.
///
/// Declared here rather than taken from the crate for one more commit, so this
/// bump stays a translation. `from_bits_retain` is the constructor that keeps a
/// bit the type does not name, which is exactly what a hand-declared flag is.
const FUSE_HANDLE_KILLPRIV_V2: InitFlags = InitFlags::from_bits_retain(1 << 28);
```

and the one the parallel-writes plan left:

```rust
/// `FOPEN_PARALLEL_DIRECT_WRITES`, bit 6 per `include/uapi/linux/fuse.h:393`.
///
/// Declared here rather than taken from the crate for one more commit, for the
/// same reason as `FUSE_HANDLE_KILLPRIV_V2` above.
const FOPEN_PARALLEL_DIRECT_WRITES: FopenFlags = FopenFlags::from_bits_retain(1 << 6);
```

Then retype the capability record and its five entries. Keep every doc comment on the entries exactly as it stands; only the field type and the five `bit:` values change:

```rust
/// One capability this client asks the kernel for, and what its absence costs.
struct Capability {
    bit: InitFlags,
    name: &'static str,
    /// Whether a kernel without it makes the mount wrong rather than merely
    /// slow.
    required: bool,
}
```

| entry | `bit:` was | `bit:` becomes |
|---|---|---|
| `FUSE_DO_READDIRPLUS` | `FUSE_DO_READDIRPLUS` | `InitFlags::FUSE_DO_READDIRPLUS` |
| `FUSE_READDIRPLUS_AUTO` | `FUSE_READDIRPLUS_AUTO` | `InitFlags::FUSE_READDIRPLUS_AUTO` |
| `FUSE_ASYNC_DIO` | `FUSE_ASYNC_DIO` | `InitFlags::FUSE_ASYNC_DIO` |
| `FUSE_HANDLE_KILLPRIV_V2` | `FUSE_HANDLE_KILLPRIV_V2` | unchanged — the local constant is an `InitFlags` now |
| `FUSE_WRITEBACK_CACHE` | `FUSE_WRITEBACK_CACHE` | `InitFlags::FUSE_WRITEBACK_CACHE` |

- [ ] **Step 6: Rewrite `open_flags`**

Replace the signature and body the parallel-writes plan left, keeping its doc comment word for word above it:

```rust
fn open_flags(app_flags: OpenFlags) -> FopenFlags {
    let mut flags = FopenFlags::FOPEN_KEEP_CACHE;
    if app_flags.0 & libc::O_DIRECT != 0 {
        flags |= FopenFlags::FOPEN_DIRECT_IO | FOPEN_PARALLEL_DIRECT_WRITES;
    }
    flags
}
```

`OpenFlags` is `pub struct OpenFlags(pub i32)`, so `.0` is the application's own flag word, unchanged in meaning from the `i32` the callback used to receive.

- [ ] **Step 7: Replace `mount_options` with `session_config`**

Replace `pub fn mount_options(...)` at line 426 and its doc comment entirely:

```rust
/// The session configuration this client mounts with.
///
/// `max_read` is the one option that is not decoration. The negotiated I/O
/// ceiling reaches the kernel's write path through `KernelConfig::set_max_write`,
/// but `fuser` exposes no equivalent for reads and the kernel's default is far
/// larger. Without this option the kernel would issue reads the multiplexer
/// refuses with `EINVAL` — an unreadable file for no visible reason — so the
/// number is fixed here, at mount time, from the same negotiation.
///
/// `default_permissions` is what makes the kernel enforce the server's
/// reported mode bits locally, which is why `ACCESS` is `ENOSYS` server-side
/// (spec §7).
///
/// Who may reach the mount is a `SessionACL` rather than a mount option from
/// this release on. `SessionACL::Owner` is FUSE's own default and keeps the
/// mount private to the user who made it; `SessionACL::All` is the old
/// `allow_other`. The third value, `RootAndOwner`, is not offered here — a
/// mount that root may enter and nobody else is a shape no lbfs deployment has
/// asked for, and every value added to this function is one more combination
/// the tests have to cover.
///
/// `Config` is `#[non_exhaustive]`, so it cannot be written as a struct
/// expression from this crate at all — not even with `..Default::default()`.
/// Start from the default and assign.
pub fn session_config(max_io_size: u32, allow_other: bool, auto_unmount: bool) -> Config {
    let mut mount_options = vec![
        MountOption::FSName("lbfs".to_string()),
        MountOption::DefaultPermissions,
        MountOption::CUSTOM(format!("max_read={max_io_size}")),
        // Said out loud rather than inherited. `fusermount3` forces both on an
        // unprivileged mount, so for the ordinary case these change nothing —
        // but a client run as root gets neither, and the attributes this bridge
        // reports are the server's verbatim, setuid bits and `rdev` included.
        // A compromised server could otherwise plant a setuid-root binary or a
        // device node in the export and have the client's kernel honor it. v1
        // trusts the server, and this is the cheapest way not to have to.
        MountOption::NoSuid,
        MountOption::NoDev,
    ];
    if auto_unmount {
        // Off by default, and a flag rather than a constant, because
        // `fusermount3` only honors it for a mount that also carries
        // `allow_other` — `fuser` widens the ACL implicitly — and only when
        // `user_allow_other` is set in `/etc/fuse.conf`. Widening a private
        // mount to every user on the machine is not a default, and on a host
        // without that line it is not a default that works.
        mount_options.push(MountOption::AutoUnmount);
    }

    let mut config = Config::default();
    config.mount_options = mount_options;
    config.acl = if allow_other {
        SessionACL::All
    } else {
        SessionACL::Owner
    };
    config
}
```

- [ ] **Step 8: Rewrite the error and reply helpers**

Replace `fn errno` and the six `reply_*` helpers at lines 492-549:

```rust
/// A `u16` errno as the kernel wants it. Every error out of the multiplexer
/// arrives this way, including the `EIO` a dead connection answers with, so
/// disconnection needs no handling of its own here (spec §7).
///
/// `Errno::from_i32` answers `EIO` for anything that is not a positive number,
/// which is the right reading of a zero arriving where an error belongs.
fn errno(e: Errno) -> FuseErrno {
    FuseErrno::from_i32(i32::from(e.0))
}

fn reply_entry(reply: ReplyEntry, ttl: Duration, r: Result<Entry, Errno>) {
    match r {
        Ok(e) => reply.entry(
            &ttl,
            &to_fuse_attr(e.node, &e.attr),
            Generation(e.generation),
        ),
        Err(e) => reply.error(errno(e)),
    }
}

fn reply_attr(reply: ReplyAttr, ttl: Duration, node: NodeId, r: Result<FileAttr, Errno>) {
    match r {
        Ok(a) => reply.attr(&ttl, &to_fuse_attr(node, &a)),
        Err(e) => reply.error(errno(e)),
    }
}

fn reply_unit(reply: ReplyEmpty, r: Result<(), Errno>) {
    match r {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(errno(e)),
    }
}

fn reply_data(reply: ReplyData, r: Result<Vec<u8>, Errno>) {
    match r {
        Ok(bytes) => reply.data(&bytes),
        Err(e) => reply.error(errno(e)),
    }
}

/// FUSE's two-phase xattr read, both halves.
///
/// `size == 0` asks only for the length; anything else asks for the value and
/// wants `ERANGE` when it does not fit. The server answers the second case with
/// `ERANGE` itself, straight from the syscall — the check here is what keeps
/// the contract true whatever the backend does.
fn reply_xattr(reply: ReplyXattr, size: u32, r: Result<(u32, Vec<u8>), Errno>) {
    match r {
        Ok((len, _)) if size == 0 => reply.size(len),
        Ok((len, _)) if len > size => reply.error(FuseErrno::ERANGE),
        Ok((_, value)) => reply.data(&value),
        Err(e) => reply.error(errno(e)),
    }
}

fn reply_statfs(reply: ReplyStatfs, r: Result<StatfsReply, Errno>) {
    match r {
        Ok(s) => reply.statfs(
            s.blocks, s.bfree, s.bavail, s.files, s.ffree, s.bsize, s.namelen, s.frsize,
        ),
        Err(e) => reply.error(errno(e)),
    }
}
```

- [ ] **Step 9: Wrap the node id in `to_fuse_attr`**

One line inside `to_fuse_attr` at line 147, doc comment unchanged:

```rust
        ino: INodeNo(node),
```

- [ ] **Step 10: Rewrite `init` and `destroy`**

Replace the `init` signature and its two error returns; every comment inside the body stays as written:

```rust
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
```

The two `return Err(libc::EPROTO);` lines at 566 and 605 become:

```rust
            return Err(io::Error::from_raw_os_error(libc::EPROTO));
```

`destroy(&mut self)` keeps its signature; both callbacks run once, outside the concurrent phase, which is why they alone still take `&mut self`.

- [ ] **Step 11: Delete the `batch_forget` override**

Remove the whole method at lines 639-643. The trait's default loops the slice and calls `forget`, which is what the override did per node, so the deletion changes nothing observable. Correction c in Design and Context explains why the migration is a deletion: `ForgetOne` names a private type and no outside crate can write the signature.

Then rewrite `forget` itself:

```rust
    /// No reply object, no way to wait: [`Connection::send_forget`] is
    /// synchronous and batches behind the scenes, so this must not spawn.
    ///
    /// `batch_forget` is deliberately not overridden. Its slice element type is
    /// private to `fuser`, so the signature cannot be written from here — and
    /// it does not need to be, because the trait's default body calls this
    /// method once per node, which is exactly what the old override did.
    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.conn.send_forget(ino.0, nlookup);
    }
```

- [ ] **Step 12: Sweep the callbacks that only change shape**

For each callback below, apply the same three edits and nothing else: `&mut self` becomes `&self`, `Request<'_>` becomes `Request`, and the newtyped parameters get unwrapped on the first line of the body so every existing body line survives untouched.

| callback | new parameter types | first line of the body |
|---|---|---|
| `lookup` | `parent: INodeNo` | `let parent = parent.0;` |
| `getattr` | `ino: INodeNo, fh: Option<FileHandle>` | `let (ino, fh) = (ino.0, fh.map(\|h\| h.0));` |
| `setattr` | `ino: INodeNo`, `fh: Option<FileHandle>`, `_flags: Option<BsdFileFlags>` | `let (ino, fh) = (ino.0, fh.map(\|h\| h.0));` |
| `readlink` | `ino: INodeNo` | `let ino = ino.0;` |
| `mknod` | `_parent: INodeNo` | none; the body is one `reply.error(FuseErrno::ENOSYS);` |
| `mkdir` | `parent: INodeNo` | `let parent = parent.0;` |
| `unlink` | `parent: INodeNo` | `let parent = parent.0;` |
| `rmdir` | `parent: INodeNo` | `let parent = parent.0;` |
| `symlink` | `parent: INodeNo` | `let parent = parent.0;` |
| `link` | `ino: INodeNo, newparent: INodeNo` | `let (ino, newparent) = (ino.0, newparent.0);` |
| `flush` | `ino: INodeNo, fh: FileHandle, _lock_owner: LockOwner` | `let (ino, fh) = (ino.0, fh.0);` |
| `release` | `ino: INodeNo, fh: FileHandle, _flags: OpenFlags, _lock_owner: Option<LockOwner>` | `let (ino, fh) = (ino.0, fh.0);` |
| `fsync` | `ino: INodeNo, fh: FileHandle` | `let (ino, fh) = (ino.0, fh.0);` |
| `opendir` | `ino: INodeNo, _flags: OpenFlags` | `let ino = ino.0;` |
| `releasedir` | `ino: INodeNo, fh: FileHandle, _flags: OpenFlags` | `let (ino, fh) = (ino.0, fh.0);` |
| `fsyncdir` | `ino: INodeNo, fh: FileHandle` | `let (ino, fh) = (ino.0, fh.0);` |
| `statfs` | `ino: INodeNo` | `let ino = ino.0;` |
| `setxattr` | `ino: INodeNo`; `flags: i32` unchanged | `let ino = ino.0;` |
| `getxattr` | `ino: INodeNo` | `let ino = ino.0;` |
| `listxattr` | `ino: INodeNo` | `let ino = ino.0;` |
| `removexattr` | `ino: INodeNo` | `let ino = ino.0;` |

`opendir`'s `reply.opened(dh, 0)` becomes `reply.opened(FileHandle(dh), FopenFlags::empty())` — a directory handle wants no flags, which is what the zero said.

`mknod`'s single statement becomes `reply.error(FuseErrno::ENOSYS);`.

- [ ] **Step 13: Rewrite the eleven callbacks that change more than shape**

```rust
    /// `flags` carries `RENAME_NOREPLACE` and `RENAME_EXCHANGE` straight
    /// through to the server's `renameat2`, which is what preserves the
    /// atomicity the caller asked for (spec §8). `RenameFlags` decodes with
    /// `from_bits_retain`, so a bit this crate does not name still reaches the
    /// syscall unchanged.
    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (conn, _) = self.ctx();
        let (parent, newparent, flags) = (parent.0, newparent.0, flags.bits());
        let name = name.as_bytes().to_vec();
        let newname = newname.as_bytes().to_vec();
        self.rt.spawn(async move {
            reply_unit(
                reply,
                conn.rename(parent, &name, newparent, &newname, flags).await,
            )
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let (conn, _) = self.ctx();
        let ino = ino.0;
        self.rt.spawn(async move {
            match conn.open(ino, flags.0 as u32).await {
                Ok(fh) => reply.opened(FileHandle(fh), open_flags(flags)),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let (conn, ttl) = self.ctx();
        let parent = parent.0;
        let name = name.as_bytes().to_vec();
        self.rt.spawn(async move {
            match conn.create(parent, &name, mode, flags as u32).await {
                Ok((e, fh)) => reply.created(
                    &ttl,
                    &to_fuse_attr(e.node, &e.attr),
                    Generation(e.generation),
                    FileHandle(fh),
                    open_flags(OpenFlags(flags)),
                ),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let (conn, _) = self.ctx();
        let (ino, fh) = (ino.0, fh.0);
        self.rt
            .spawn(async move { reply_data(reply, conn.read(ino, fh, offset, size).await) });
    }
```

`write` keeps whatever the kill-priv plan's Task 3 left in the body; only the signature and the offset guard change. The `write_flags` parameter arrives as a `WriteFlags` now, so the comparison against `consts::FUSE_WRITE_KILL_PRIV` becomes a `contains` against the bit the kill-priv plan already reads:

```rust
    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let (conn, _) = self.ctx();
        let (ino, fh) = (ino.0, fh.0);
        let kill_suidgid = write_flags.contains(WriteFlags::from_bits_retain(FUSE_WRITE_KILL_PRIV));
        // The slice borrows the session's single receive buffer, which is
        // reused the moment this callback returns. The copy is what lets the
        // write outlive the callback.
        let data = data.to_vec();
        self.rt.spawn(async move {
            match conn.write(ino, fh, offset, data, kill_suidgid).await {
                Ok(written) => reply.written(written),
                Err(e) => reply.error(errno(e)),
            }
        });
    }
```

Keep whatever name and doc comment the kill-priv plan gave the helper that computes `kill_suidgid`; if that plan put the comparison in a named function rather than inline, change only its parameter type from `u32` to `WriteFlags` and leave the call here. `FUSE_WRITE_KILL_PRIV` no longer exists in `fuser::consts`, so a local `const FUSE_WRITE_KILL_PRIV: u32 = 1 << 2;` covers the gap for this commit; Task 7 replaces the whole expression with `WriteFlags::FUSE_WRITE_KILL_SUIDGID`.

```rust
    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let (conn, _) = self.ctx();
        let (ino, fh) = (ino.0, fh.0);
        self.rt.spawn(async move {
            let mut cursor = offset;
            // Across pages, not within one: the kernel's buffer holds the whole
            // reply, so this is what says whether a refusal leaves it empty.
            let mut emitted = 0usize;
            loop {
                let page = match conn.readdir(ino, fh, cursor, READDIR_PAGE_BYTES).await {
                    Ok(page) => page,
                    Err(e) => return reply.error(errno(e)),
                };
                // An empty non-final page would leave the cursor where it is
                // and spin here forever. The server does not produce one; the
                // check is what keeps a server that did from hanging the mount
                // rather than merely truncating a listing.
                let done = page.end || page.entries.is_empty();
                for e in page.entries.iter() {
                    cursor = e.offset;
                    // The real inode from the server's `getdents64`. Plain
                    // `READDIR` instantiates nothing, so this number is only
                    // ever `d_ino`, a zero would have glibc drop the name, and
                    // a discarded entry owes no `FORGET` — the difference from
                    // `readdirplus`, where every entry carries a lookup count.
                    if reply.add(
                        INodeNo(e.ino),
                        e.offset,
                        kind_of_wire(e.kind),
                        OsStr::from_bytes(&e.name),
                    ) {
                        first_entry_overflow(emitted, &e.name);
                        return reply.ok();
                    }
                    emitted += 1;
                }
                if done {
                    return reply.ok();
                }
            }
        });
    }
```

The doc comment above `readdir` gains one sentence, because half of what it explains has stopped being true:

```rust
    /// The offset is an opaque cursor, not a byte count: it is the `d_off` the
    /// server's `getdents64` reported. From fuser 0.18.0 on it arrives and
    /// leaves as a `u64`, so the reinterpretation the earlier code performed on
    /// the way in and out is gone and a filesystem that packs a hash into the
    /// high bit round-trips because nothing touches it. Only zero has a meaning
    /// of its own — the start of the listing.
```

`readdirplus` takes the same treatment. Its signature matches `readdir`'s, its body opens with the same two unwrapping lines and `let mut cursor = offset;`, and the `reply.add` call inside the closure becomes:

```rust
                    |e, node| {
                        let attr = to_fuse_attr(node, &e.entry.attr);
                        reply.add(
                            INodeNo(node),
                            e.offset,
                            OsStr::from_bytes(&e.name),
                            &ttl,
                            &attr,
                            Generation(e.entry.generation),
                        )
                    },
```

```rust
    fn fallocate(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        let (conn, _) = self.ctx();
        let (ino, fh) = (ino.0, fh.0);
        self.rt.spawn(async move {
            reply_unit(
                reply,
                conn.fallocate(ino, fh, offset, length, mode as u32).await,
            )
        });
    }

    /// The one offset the kernel still hands over signed, because `SEEK_HOLE`
    /// and `SEEK_DATA` take a starting point rather than a length. The guard
    /// stays where its four neighbours lost theirs.
    fn lseek(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        let (conn, _) = self.ctx();
        let (ino, fh) = (ino.0, fh.0);
        self.rt.spawn(async move {
            let Ok(offset) = u64::try_from(offset) else {
                reply.error(FuseErrno::EINVAL);
                return;
            };
            match conn.lseek(ino, fh, offset, whence as u32).await {
                // The result is a file offset, so it fits an `i64` on any
                // filesystem the kernel can represent.
                Ok(off) => reply.offset(off as i64),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_file_range(
        &self,
        _req: &Request,
        ino_in: INodeNo,
        fh_in: FileHandle,
        offset_in: u64,
        ino_out: INodeNo,
        fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        // Reserved by the syscall; must be zero, and the kernel checks.
        _flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        let (conn, _) = self.ctx();
        self.rt.spawn(async move {
            let req = CopyFileRangeRequest {
                node_in: ino_in.0,
                fh_in: fh_in.0,
                off_in: offset_in,
                node_out: ino_out.0,
                fh_out: fh_out.0,
                off_out: offset_out,
                len,
            };
            match conn.copy_file_range(&req).await {
                // FUSE's reply field is 32 bits wide, so a copy larger than
                // that could not be reported whole; the caller sees a short
                // copy and comes back for the rest, which is what
                // `copy_file_range(2)` already allows.
                Ok(copied) => reply.written(copied.min(u32::MAX as u64) as u32),
                Err(e) => reply.error(errno(e)),
            }
        });
    }
```

- [ ] **Step 14: Repair the test module**

`fn requested` returns a flag set now:

```rust
    fn requested(writeback: bool) -> InitFlags {
        capabilities(writeback)
            .iter()
            .fold(InitFlags::empty(), |all, c| all | c.bit)
    }
```

The three capability tests move from masking to `contains`:

```rust
    #[test]
    fn atomic_o_trunc_is_never_requested() {
        for writeback in [true, false] {
            assert!(!requested(writeback).contains(InitFlags::FUSE_ATOMIC_O_TRUNC));
        }
    }

    #[test]
    fn readdirplus_is_always_requested_and_writeback_only_when_asked() {
        for writeback in [true, false] {
            let caps = requested(writeback);
            assert!(caps.contains(InitFlags::FUSE_DO_READDIRPLUS));
            assert!(caps.contains(InitFlags::FUSE_READDIRPLUS_AUTO));
        }
        assert!(requested(true).contains(InitFlags::FUSE_WRITEBACK_CACHE));
        assert!(!requested(false).contains(InitFlags::FUSE_WRITEBACK_CACHE));
    }

    #[test]
    fn async_dio_is_always_requested() {
        for writeback in [true, false] {
            assert!(requested(writeback).contains(InitFlags::FUSE_ASYNC_DIO));
        }
    }

    #[test]
    fn only_the_promised_capability_is_required() {
        let required: Vec<_> = capabilities(true)
            .into_iter()
            .filter(|c| c.required)
            .map(|c| c.bit)
            .collect();
        assert_eq!(required, vec![InitFlags::FUSE_WRITEBACK_CACHE]);
        assert!(capabilities(false).iter().all(|c| !c.required));
    }

    #[test]
    fn the_only_high_capability_asked_for_is_killpriv_v2() {
        let low = InitFlags::from_bits_retain((1u64 << 26) - 1);
        for writeback in [true, false] {
            assert_eq!(
                requested(writeback) & !low,
                FUSE_HANDLE_KILLPRIV_V2,
                "an unexpected capability above bit 25 (writeback={writeback})"
            );
        }
    }
```

The kill-priv plan's two tests keep their shape with the type changed:

```rust
    #[test]
    fn killpriv_v2_is_always_requested_at_bit_twenty_eight() {
        assert_eq!(FUSE_HANDLE_KILLPRIV_V2.bits(), 1 << 28);
        for writeback in [true, false] {
            assert!(requested(writeback).contains(FUSE_HANDLE_KILLPRIV_V2));
        }
    }

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

The parallel-writes plan's four `open_flags` tests take `OpenFlags` and compare flag sets. The two that need real edits:

```rust
    #[test]
    fn only_an_o_direct_open_gets_the_direct_io_reply() {
        let direct_pair = FopenFlags::FOPEN_DIRECT_IO | FOPEN_PARALLEL_DIRECT_WRITES;

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
            assert!(!reply.intersects(direct_pair), "flags {plain:#o}");
        }

        // `O_APPEND | O_DIRECT` belongs in the direct set even though every
        // append takes the exclusive lock anyway (`file.c:1412-1413`): the
        // reply describes the handle, and the kernel decides per write.
        for direct in [
            libc::O_RDONLY | libc::O_DIRECT,
            libc::O_WRONLY | libc::O_DIRECT,
            libc::O_RDWR | libc::O_DIRECT,
            libc::O_RDWR | libc::O_DIRECT | libc::O_APPEND,
            libc::O_WRONLY | libc::O_DIRECT | libc::O_SYNC,
        ] {
            let want = FopenFlags::FOPEN_KEEP_CACHE | direct_pair;
            assert_eq!(open_flags(OpenFlags(direct)), want, "flags {direct:#o}");
        }
    }

    /// Bit 6, per `include/uapi/linux/fuse.h:393`. The constant is still local
    /// for one more commit, so this test is what pins it; the collision checks
    /// matter because a wrong value would land on a flag with an entirely
    /// different meaning and nothing would report an error.
    #[test]
    fn the_parallel_write_bit_is_bit_six() {
        assert_eq!(FOPEN_PARALLEL_DIRECT_WRITES.bits(), 1 << 6);
        assert_eq!(FopenFlags::FOPEN_DIRECT_IO.bits(), 1 << 0);
        assert_eq!(FopenFlags::FOPEN_KEEP_CACHE.bits(), 1 << 1);
        for named in [
            FopenFlags::FOPEN_DIRECT_IO,
            FopenFlags::FOPEN_KEEP_CACHE,
            FopenFlags::FOPEN_NONSEEKABLE,
            FopenFlags::FOPEN_CACHE_DIR,
            FopenFlags::FOPEN_STREAM,
        ] {
            assert!(!FOPEN_PARALLEL_DIRECT_WRITES.intersects(named));
        }
    }
```

`the_parallel_write_bit_never_travels_alone` and `opens_keep_the_page_cache` change only in taking `OpenFlags(flags)` and testing with `contains` in place of a mask.

The three mount-option tests become configuration tests. `access_widening_options_are_opt_in` splits, as the assessment predicted, and `the_option_list_holds_no_duplicates_or_conflicts` keeps the duplicate half and loses the conflict half, because nobody can construct the pair it guarded against any more:

```rust
    /// `fuser` has no `set_max_read`, so the negotiated ceiling can only reach
    /// the kernel's read path as a mount option. Without it the kernel issues
    /// reads the multiplexer answers with `EINVAL`.
    #[test]
    fn the_mount_pins_max_read_to_the_negotiated_size() {
        let cfg = session_config(4096, false, false);
        assert!(cfg
            .mount_options
            .contains(&MountOption::CUSTOM("max_read=4096".to_string())));
        assert!(cfg
            .mount_options
            .contains(&MountOption::FSName("lbfs".to_string())));
        assert!(cfg.mount_options.contains(&MountOption::DefaultPermissions));
    }

    /// The server's attributes travel verbatim, setuid bits and `rdev`
    /// included. `fusermount3` refuses both to an unprivileged mount anyway;
    /// this is what covers a client run as root against a server it should not
    /// have trusted that far.
    #[test]
    fn the_mount_never_honours_setuid_bits_or_device_nodes() {
        for (allow_other, auto_unmount) in [(false, false), (true, true)] {
            let opts = session_config(1 << 20, allow_other, auto_unmount).mount_options;
            assert!(opts.contains(&MountOption::NoSuid));
            assert!(opts.contains(&MountOption::NoDev));
            assert!(!opts.contains(&MountOption::Suid));
            assert!(!opts.contains(&MountOption::Dev));
        }
    }

    /// Neither widens access by default. Reach is an ACL from this release on
    /// rather than a mount option, and `auto_unmount` makes `fuser` widen the
    /// ACL implicitly, so the two are still opt-in together.
    #[test]
    fn access_widening_is_opt_in() {
        let plain = session_config(1 << 20, false, false);
        assert_eq!(plain.acl, SessionACL::Owner);
        assert!(!plain.mount_options.contains(&MountOption::AutoUnmount));

        let wide = session_config(1 << 20, true, true);
        assert_eq!(wide.acl, SessionACL::All);
        assert!(wide.mount_options.contains(&MountOption::AutoUnmount));
    }

    /// Never `RootAndOwner`: a mount root may enter and nobody else is a shape
    /// no lbfs deployment asks for, and an ACL nothing sets is one nothing
    /// tests.
    #[test]
    fn the_root_and_owner_acl_is_never_chosen() {
        for (allow_other, auto_unmount) in [(false, false), (true, false), (true, true)] {
            let cfg = session_config(1 << 20, allow_other, auto_unmount);
            assert_ne!(cfg.acl, SessionACL::RootAndOwner);
        }
    }

    /// `fuser` rejects an option list holding the same option twice, and it
    /// rejects it by failing the mount.
    #[test]
    fn the_option_list_holds_no_duplicates() {
        for (allow_other, auto_unmount) in [(false, false), (true, false), (true, true)] {
            let opts = session_config(1 << 20, allow_other, auto_unmount).mount_options;
            let unique: std::collections::HashSet<_> = opts.iter().collect();
            assert_eq!(unique.len(), opts.len(), "{opts:?}");
        }
    }

    /// One event loop and one shared descriptor, until somebody measures
    /// otherwise on a guest with cores to spare. Each extra thread costs a
    /// resident 16 MiB receive buffer.
    #[test]
    fn the_session_runs_one_event_loop_by_default() {
        let cfg = session_config(1 << 20, false, false);
        assert_eq!(cfg.n_threads, None);
        assert!(!cfg.clone_fd);
    }
```

The three `FileAttr` tests compare against the newtype:

```rust
        assert_eq!(f.ino, INodeNo(7));
```

in `attr_conversion_preserves_fields`, and

```rust
        assert_eq!(to_fuse_attr(42, &a).ino, INodeNo(42));
```

in `attr_conversion_reports_the_node_id_not_the_servers_inode`. `attr_conversion_keeps_setuid_setgid_and_sticky` reads `perm` only and needs no edit.

- [ ] **Step 15: Repair the binary**

In `crates/lbfs-client/src/main.rs`, replace lines 148-155:

```rust
    let cfg = session_config(limits.max_io_size, cli.allow_other, cli.auto_unmount);
    let fs = LbfsFuse::new(conn, rt.handle().clone(), ttl, writeback);
    let session =
        fuser::spawn_mount(fs, &cli.mountpoint, &cfg).map_err(|source| StartupError::Mount {
            path: cli.mountpoint.display().to_string(),
            source,
        })?;
```

and update the import at the top of the file from `mount_options` to `session_config`. The comment at line 141 mentioning `spawn_mount2` becomes `spawn_mount`; the drain comment at lines 164-171 stays word for word, because the behaviour it describes does not change — `BackgroundSession` still has no `Drop` of its own and the `Mount` it holds still unmounts when dropped.

- [ ] **Step 16: Repair the loopback fixture**

In `tests/tests/loopback.rs`, change the import at line 64:

```rust
use lbfs_client::fuse::{session_config, LbfsFuse};
```

the mount at line 341:

```rust
        let session = fuser::spawn_mount(
            fs,
            &mnt,
            &session_config(limits.max_io_size, false, false),
        )
        .expect("the mount succeeds");
```

and the join inside `try_unmount` at line 434:

```rust
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(move || {
                session.umount_and_join()
            }));
            let _ = done.send(matches!(outcome, Ok(Ok(()))));
```

That last change is the one to read twice. `join()` still exists on 0.18.0 and still compiles here, and it would leave the session thread parked on `/dev/fuse` forever, because it no longer drops the `Mount` first. `umount_and_join` unmounts and then joins, which is what 0.15.1's `join` did. The doc comment above `try_unmount` mentions `BackgroundSession::join` unwrapping two results; replace that sentence:

```rust
    /// The same, reporting rather than asserting, so [`Loopback::drop`] can use
    /// it. A panic raised while unwinding aborts the whole test binary and
    /// takes every other case's diagnostics with it, and
    /// `BackgroundSession::umount_and_join` returns an `io::Result` that a
    /// failed unmount fills in.
```

Also update the two `spawn_mount2` mentions in the comments at lines 359 and 362.

- [ ] **Step 17: Run the gate**

Run: `cargo fmt --all && make check`
Expected: PASS. If clippy reports `clippy::field_reassign_with_default` on `session_config`, that lint fires on a `Default` followed by field assignment — the pattern `#[non_exhaustive]` forces here. Add `#[allow(clippy::field_reassign_with_default)]` to the function with a one-line comment naming E0639 as the reason, rather than reaching for a struct expression that cannot compile.

- [ ] **Step 18: Run both loopback suites, tripwire and drain included**

Run: `make test-loopback`
Expected: PASS, every case in both suites. Watch for `privileged_bits_die_on_write_*` and `writes_reach_the_export_on_unmount_*` by name. A timeout in the second pair means Step 16's join is wrong.

- [ ] **Step 19: Deploy and run the end-to-end suite**

Run: `make build-guest && make vm-deploy && make vm-test`
Expected: every PASS line.

- [ ] **Step 20: Commit**

```bash
git add Cargo.toml Cargo.lock crates/lbfs-client/src/fuse.rs \
  crates/lbfs-client/src/main.rs tests/tests/loopback.rs
git commit -m "build(deps)!: pin fuser at =0.18.0 and migrate the bridge

A translation with no behaviour change. The trait takes &self on the 33
callbacks that run concurrently; node ids and handles are newtypes,
unwrapped on the first line of each body so the bodies are untouched;
flag words are bitflags; Vec<MountOption> is Config; init returns
io::Result.

Three things the migration could not do mechanically. batch_forget is
deleted rather than migrated, because ForgetOne is private to the crate
and no outside impl can name it -- the trait default calls forget per
node, which is what the override did. Config is #[non_exhaustive], so it
is built from Default and assigned rather than written as a struct
expression. loopback's join becomes umount_and_join, because join alone
no longer unmounts and would park the session thread.

Four u64::try_from guards go; lseek keeps its offset signed and keeps
its guard."
```

---

### Task 7: Take the crate's names for the three flag bits we spell by hand

**Files:**
- Edit: `crates/lbfs-client/src/fuse.rs` (two constants deleted, one added and deleted again, three call sites, three tests)

**Interfaces:**
- Consumes: Task 6.
- Produces: `InitFlags::FUSE_HANDLE_KILLPRIV_V2`, `WriteFlags::FUSE_WRITE_KILL_SUIDGID` and `FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES` used directly; no locally declared FUSE constants left in the file.

A separate commit because it asks a separate question. The bits are the same either way; what changes is whose constant the client trusts. Trusting the crate's is right here — writing this plan meant checking all three values against `include/uapi/linux/fuse.h` — and a hand-declared constant that silently disagrees with the crate's own name is worse than either.

- [ ] **Step 1: Rewrite the three pin-tests first**

Replace `killpriv_v2_is_always_requested_at_bit_twenty_eight`, `the_parallel_write_bit_is_bit_six` and add one for the write flag:

```rust
    /// The values, checked against `include/uapi/linux/fuse.h` rather than
    /// taken on trust. The crate names all three now, so these tests stopped
    /// pinning constants of ours and started pinning the crate's — which is the
    /// thing worth checking at every bump, since a wrong bit here lands on a
    /// flag with an entirely different meaning and nothing reports an error.
    #[test]
    fn the_three_hand_checked_flag_bits_hold_their_values() {
        assert_eq!(InitFlags::FUSE_HANDLE_KILLPRIV_V2.bits(), 1 << 28);
        assert_eq!(WriteFlags::FUSE_WRITE_KILL_SUIDGID.bits(), 1 << 2);
        assert_eq!(FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES.bits(), 1 << 6);
        assert_eq!(FopenFlags::FOPEN_DIRECT_IO.bits(), 1 << 0);
        assert_eq!(FopenFlags::FOPEN_KEEP_CACHE.bits(), 1 << 1);
    }

    #[test]
    fn killpriv_v2_is_always_requested() {
        for writeback in [true, false] {
            assert!(requested(writeback).contains(InitFlags::FUSE_HANDLE_KILLPRIV_V2));
        }
    }
```

- [ ] **Step 2: Run them to verify they fail**

Step 1 replaces the two old tests rather than adding beside them, so nothing
duplicates a name.

Run: `cargo test -p lbfs-client --lib hand_checked_flag_bits killpriv_v2`
Expected: PASS on the two rewritten cases, because the crate's constants carry the right values already. The failure arrives in Step 3, the moment the local constants go: `cargo check -p lbfs-client` then reports `cannot find value 'FUSE_HANDLE_KILLPRIV_V2' in this scope` at `capabilities()`, at `killpriv_v2_is_optional` and at `the_only_high_capability_asked_for_is_killpriv_v2`, plus the same for `FOPEN_PARALLEL_DIRECT_WRITES` and `FUSE_WRITE_KILL_PRIV`. Step 4 answers every one of them.

- [ ] **Step 3: Delete the three local constants**

Remove `const FUSE_HANDLE_KILLPRIV_V2: InitFlags = ...`, `const FOPEN_PARALLEL_DIRECT_WRITES: FopenFlags = ...` and the `const FUSE_WRITE_KILL_PRIV: u32 = 1 << 2;` Task 6 Step 13 introduced. Their doc comments go with them; the reasoning they carried — why bit 28 works on a client declaring 7.40, and why bit 6 needs no negotiation — moves into the capability entry and the `open_flags` doc comment respectively, both of which already say most of it.

- [ ] **Step 4: Point the four use sites at the crate**

In `capabilities()`:

```rust
        Capability {
            bit: InitFlags::FUSE_HANDLE_KILLPRIV_V2,
            name: "FUSE_HANDLE_KILLPRIV_V2",
            required: false,
        },
```

In `open_flags`:

```rust
        flags |= FopenFlags::FOPEN_DIRECT_IO | FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES;
```

In `write`:

```rust
        let kill_suidgid = write_flags.contains(WriteFlags::FUSE_WRITE_KILL_SUIDGID);
```

In `killpriv_v2_is_optional` and `the_only_high_capability_asked_for_is_killpriv_v2`, replace the bare `FUSE_HANDLE_KILLPRIV_V2` with `InitFlags::FUSE_HANDLE_KILLPRIV_V2`, and in the four `open_flags` tests replace the bare `FOPEN_PARALLEL_DIRECT_WRITES` with `FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES`.

- [ ] **Step 5: Confirm no local constant survives**

Run: `grep -n "^const FUSE_\|^const FOPEN_" crates/lbfs-client/src/fuse.rs`
Expected: no output.

- [ ] **Step 6: Run the gate and both loopback suites**

Run: `cargo fmt --all && make check && make test-loopback`
Expected: PASS. The tripwire pair is the one to watch: `WriteFlags::FUSE_WRITE_KILL_SUIDGID` is now the name the strip depends on, and a wrong bit there would leak set-user-ID silently.

- [ ] **Step 7: Commit**

```bash
git add crates/lbfs-client/src/fuse.rs
git commit -m "refactor(client): take fuser's names for the three flag bits"
```

---

### Task 8: Split the entry lifetime from the attribute lifetime

**Files:**
- Edit: `crates/lbfs-client/src/fuse.rs` (`LbfsFuse`, `new`, `ctx`, `reply_entry`, four callbacks, tests)
- Edit: `crates/lbfs-client/src/main.rs` (a CLI flag and its plumbing)
- Edit: `tests/tests/loopback.rs` (`Opts`, the fixture)
- Edit: `docs/superpowers/specs/2026-08-20-lbfs-design.md` (§7 caching bullet)
- Edit: `README.md` (the client flag table)

**Interfaces:**
- Consumes: Task 6's `Filesystem` block.
- Produces: `LbfsFuse::new(conn, rt, attr_ttl, entry_ttl, writeback)`; `fn reply_entry(reply: ReplyEntry, attr_ttl: Duration, entry_ttl: Duration, r: Result<Entry, Errno>)`; `fn entry_timeout(entry: Option<f64>, attr: Duration) -> Result<Duration, StartupError>` in `main.rs`; `Cli::entry_timeout: Option<f64>`; `Opts.entry_ttl: Duration`.

Spec §7 has always written this as two knobs — "`entry_timeout`/`attr_timeout` default 1 s, CLI-tunable (0 disables)" — and the code collapsed them to one because 0.15.1's `ReplyEntry::entry` took a single `Duration`. `entry_with_ttls` takes both, attribute lifetime first. The default keeps the two equal, so a mount naming no new flag behaves exactly as it does today.

The split reaches `lookup`, `mkdir`, `symlink` and `link`. `ReplyCreate::created` and `ReplyDirectoryPlus::add` still send one lifetime as both, so a freshly created file and a name learned from a listing use the attribute lifetime for their dentry. That is the conservative direction — a shorter name lifetime costs round trips, never correctness — and it wants saying in the field's doc comment so nobody reads the flag as covering more than it does.

- [ ] **Step 1: Write the failing test**

The defaulting rule is the only branch worth a unit test, and it belongs in
`crates/lbfs-client/src/main.rs` beside `attr_timeout`, which it mirrors. The
field pair on `LbfsFuse` needs none: the type system carries it, and the
loopback case in Step 8 proves it reaches the kernel.

Add to the `mod tests` block in `crates/lbfs-client/src/main.rs`:

```rust
    /// Absent means "the same as the attribute lifetime", which is what every
    /// mount did before `entry_with_ttls` made the two separable. Present
    /// means what it says, including zero, which disables dentry caching on
    /// its own.
    #[test]
    fn the_entry_lifetime_falls_back_to_the_attribute_lifetime() {
        let attr = Duration::from_millis(500);
        assert_eq!(entry_timeout(None, attr).unwrap(), attr);
        assert_eq!(
            entry_timeout(Some(60.0), attr).unwrap(),
            Duration::from_secs(60)
        );
        assert_eq!(entry_timeout(Some(0.0), attr).unwrap(), Duration::ZERO);
        assert!(entry_timeout(Some(-1.0), attr).is_err());
        assert!(entry_timeout(Some(f64::NAN), attr).is_err());
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p lbfs-client --lib entry_lifetime`
Expected: FAIL — `cannot find function 'entry_timeout' in this scope`.

- [ ] **Step 3: Give the bridge two lifetimes**

Replace the struct, its constructor and `ctx`:

```rust
/// The `fuser::Filesystem` implementation: one connection, one runtime, two
/// cache lifetimes.
pub struct LbfsFuse {
    conn: Arc<Connection>,
    rt: tokio::runtime::Handle,
    /// How long the kernel may trust a cached `stat` (spec §7). Zero disables
    /// attribute caching.
    attr_ttl: Duration,
    /// How long the kernel may trust a cached name-to-node mapping (spec §7).
    /// Zero disables dentry caching.
    ///
    /// Separate from `attr_ttl` only where `ReplyEntry` carries the reply.
    /// `ReplyCreate::created` and `ReplyDirectoryPlus::add` still take one
    /// lifetime and send it as both, so a file this mount just created, and a
    /// name it learned from a `READDIRPLUS`, both cache their dentry for
    /// `attr_ttl`. Erring short costs round trips and never correctness, which
    /// is the direction to err in.
    entry_ttl: Duration,
    writeback: bool,
}

impl LbfsFuse {
    pub fn new(
        conn: Arc<Connection>,
        rt: tokio::runtime::Handle,
        attr_ttl: Duration,
        entry_ttl: Duration,
        writeback: bool,
    ) -> LbfsFuse {
        LbfsFuse {
            conn,
            rt,
            attr_ttl,
            entry_ttl,
            writeback,
        }
    }

    /// What every callback captures before it spawns.
    fn ctx(&self) -> (Arc<Connection>, Duration) {
        (Arc::clone(&self.conn), self.attr_ttl)
    }

    /// The same, for the four callbacks that answer with a `ReplyEntry` and can
    /// therefore give the two lifetimes different values.
    fn entry_ctx(&self) -> (Arc<Connection>, Duration, Duration) {
        (Arc::clone(&self.conn), self.attr_ttl, self.entry_ttl)
    }
}
```

- [ ] **Step 4: Widen `reply_entry` and its four callers**

```rust
fn reply_entry(
    reply: ReplyEntry,
    attr_ttl: Duration,
    entry_ttl: Duration,
    r: Result<Entry, Errno>,
) {
    match r {
        // Attribute lifetime first: that is the order `entry_with_ttls` takes,
        // and swapping them compiles cleanly and caches the wrong thing.
        Ok(e) => reply.entry_with_ttls(
            &attr_ttl,
            &entry_ttl,
            &to_fuse_attr(e.node, &e.attr),
            Generation(e.generation),
        ),
        Err(e) => reply.error(errno(e)),
    }
}
```

In `lookup`, `mkdir`, `symlink` and `link`, replace `let (conn, ttl) = self.ctx();` with `let (conn, attr_ttl, entry_ttl) = self.entry_ctx();` and the `reply_entry(reply, ttl, ...)` call with `reply_entry(reply, attr_ttl, entry_ttl, ...)`. `create` keeps `self.ctx()` and its single `ttl`, because `ReplyCreate::created` takes one.

The `tracing::info!` at the end of `init` gains the second number:

```rust
        tracing::info!(
            max_io,
            writeback = self.writeback,
            attr_ttl = ?self.attr_ttl,
            entry_ttl = ?self.entry_ttl,
            "mount initialized"
        );
```

- [ ] **Step 5: Add the CLI flag**

In `crates/lbfs-client/src/main.rs`, add after `attr_timeout`:

```rust
    /// How long the kernel may trust a cached name, in seconds.
    ///
    /// Defaults to `--attr-timeout`, which is what this client did before the
    /// two became separable. Raising it alone suits a workload that resolves
    /// the same paths repeatedly and reads their attributes rarely — a build
    /// tree is the case in point. Zero disables dentry caching. It reaches
    /// `LOOKUP`, `MKDIR`, `SYMLINK` and `LINK` replies; a file this mount
    /// created, and a name it learned from a directory listing, use
    /// `--attr-timeout` for both lifetimes because FUSE's reply for those
    /// carries only one.
    #[arg(long)]
    entry_timeout: Option<f64>,
```

and the helper beside `attr_timeout`:

```rust
/// The name lifetime, falling back to the attribute lifetime when the operator
/// named only one.
///
/// A fallback rather than a constant default, so `--attr-timeout 0` keeps
/// disabling both caches the way it always did, and a mount that names neither
/// flag behaves as every mount did before the two became separable.
fn entry_timeout(entry: Option<f64>, attr: Duration) -> Result<Duration, StartupError> {
    match entry {
        None => Ok(attr),
        Some(secs) => attr_timeout(secs),
    }
}
```

and in `run()`, after `let ttl = attr_timeout(cli.attr_timeout)?;`:

```rust
    let entry_ttl = entry_timeout(cli.entry_timeout, ttl)?;
```

then pass it: `LbfsFuse::new(conn, rt.handle().clone(), ttl, entry_ttl, writeback)`.

- [ ] **Step 6: Add the CLI test**

In the `mod tests` block of `main.rs`, extend `cli_parses_the_documented_invocation` with one line and add one case:

```rust
        assert_eq!(cli.entry_timeout, None);
```

```rust
    /// The flag parses, and its absence parses as absence rather than as a
    /// number somebody has to remember the meaning of.
    #[test]
    fn the_entry_timeout_flag_parses() {
        let cli = Cli::parse_from([
            "lbfs-client",
            "--attr-timeout",
            "0.5",
            "10.0.0.2:7000",
            "/srv/exports/a",
            "/mnt/lbfs",
        ]);
        assert_eq!(cli.entry_timeout, None);

        let split = Cli::parse_from([
            "lbfs-client",
            "--attr-timeout",
            "0.5",
            "--entry-timeout",
            "60",
            "10.0.0.2:7000",
            "/srv/exports/a",
            "/mnt/lbfs",
        ]);
        assert_eq!(split.attr_timeout, 0.5);
        assert_eq!(split.entry_timeout, Some(60.0));
    }
```

- [ ] **Step 7: Repair the loopback fixture**

In `tests/tests/loopback.rs`, add a field to `Opts` and its default:

```rust
    /// Name lifetime. Defaults to `ttl`, which is what the shipped client does
    /// when `--entry-timeout` is absent.
    entry_ttl: Duration,
```

```rust
            ttl: Duration::from_secs(1),
            entry_ttl: Duration::from_secs(1),
```

and pass it at the construction site near line 331:

```rust
        let fs = LbfsFuse::new(
            Arc::clone(&conn),
            client_rt.handle().clone(),
            opts.ttl,
            opts.entry_ttl,
            opts.writeback,
        );
```

Every existing `Opts { ttl: Duration::ZERO, ..Opts::default() }` in the file keeps a one-second entry lifetime after this change, which would defeat any case that sets `ttl` to zero to force a round trip. Find them with `grep -n "ttl:" tests/tests/loopback.rs` and set `entry_ttl: Duration::ZERO` beside each one.

- [ ] **Step 8: Add the loopback case**

```rust
/// A long name lifetime beside a short attribute lifetime, end to end.
///
/// The point of separating them is that a path can stay resolved while its
/// attributes go stale, so this asserts both halves: a `stat` past the
/// attribute lifetime sees a size the server changed behind the mount's back,
/// and the name itself never had to be looked up again for that to happen.
#[test]
#[ignore = "mounts a real filesystem; run with `make test-loopback`"]
fn a_long_name_lifetime_does_not_hold_a_stale_size() {
    let lb = Loopback::start(Opts {
        ttl: Duration::from_millis(50),
        entry_ttl: Duration::from_secs(3600),
        ..Opts::default()
    });
    lb.wait_ready();

    let seen = lb.mnt().join("grows");
    let real = lb.export().join("grows");
    std::fs::write(&seen, b"one").unwrap();
    assert_eq!(std::fs::metadata(&seen).unwrap().len(), 3);

    // Behind the mount's back, so only an expired attribute lifetime can
    // reveal it.
    std::fs::write(&real, b"four-plus").unwrap();
    std::thread::sleep(Duration::from_millis(200));

    assert_eq!(
        std::fs::metadata(&seen).unwrap().len(),
        9,
        "the attribute lifetime did not expire, or the entry lifetime pinned it"
    );
}
```

- [ ] **Step 9: Run the tests**

Run: `cargo test -p lbfs-client --lib entry_lifetime entry_timeout_flag && cargo test -p lbfs-tests --test loopback a_long_name_lifetime -- --ignored --test-threads=1`
Expected: PASS.

- [ ] **Step 10: Document the flag**

In `README.md`, add a row to the client flag table beside `--attr-timeout`:

```markdown
| `--entry-timeout SECS` | same as `--attr-timeout` | How long the kernel may trust a cached name. Raise it alone for a workload that resolves the same paths often and reads their attributes rarely. Reaches `LOOKUP`, `MKDIR`, `SYMLINK` and `LINK`; a freshly created file and a name from a listing use `--attr-timeout` for both, because FUSE's reply for those carries one lifetime. |
```

In `docs/superpowers/specs/2026-08-20-lbfs-design.md` §7, replace the first clause of the caching bullet:

```markdown
  `entry_timeout`/`attr_timeout` default 1 s, both CLI-tunable (0 disables),
  and `entry_timeout` defaults to whatever `attr_timeout` is. The split
  reaches `ReplyEntry` replies only — `CREATE` and `READDIRPLUS` carry one
  lifetime and send it as both;
```

and update the CLI line further down the same section:

```markdown
  `lbfs-client <server:port> <remote-path> <mountpoint> [--attr-timeout N]
  [--entry-timeout N] [--allow-other] [--auto-unmount] [--no-writeback]`.
```

- [ ] **Step 11: Run the whole gate**

Run: `make check && make test-loopback`
Expected: PASS.

- [ ] **Step 12: Commit**

```bash
git add crates/lbfs-client/src/fuse.rs crates/lbfs-client/src/main.rs \
  tests/tests/loopback.rs README.md docs/superpowers/specs/2026-08-20-lbfs-design.md
git commit -m "feat(client): --entry-timeout, separate from the attribute timeout"
```

---

### Task 9: Extra event-loop threads, off by default

**Files:**
- Edit: `crates/lbfs-client/src/fuse.rs` (`session_config` gains two parameters, plus tests)
- Edit: `crates/lbfs-client/src/main.rs` (two CLI flags, `event_loop_threads`, a `StartupError` variant, tests)
- Edit: `tests/tests/loopback.rs` (the one `session_config` call)
- Edit: `README.md` (two rows in the client flag table)

**Interfaces:**
- Consumes: Task 6's `session_config`.
- Produces: `pub fn session_config(max_io_size: u32, allow_other: bool, auto_unmount: bool, n_threads: Option<usize>, clone_fd: bool) -> Config`; `fn event_loop_threads(n: Option<usize>) -> Result<Option<usize>, StartupError>` in `main.rs`; `Cli::fuse_threads: Option<usize>` and `Cli::fuse_clone_fd: bool`.

Off by default and expected to stay off. Section 8 of Design and Context has the argument: the session thread peaks at 15.6% of a core across the whole bottleneck campaign, the guest has two vCPUs with tokio workers already on one, and each extra thread reserves a resident 16 MiB receive buffer that never shrinks to the negotiated `max_write`. The knob exists so a four-vCPU guest earns a measurement without another code change, and Task 10 records what it does today.

- [ ] **Step 1: Write the failing tests**

In `crates/lbfs-client/src/fuse.rs`, replace `the_session_runs_one_event_loop_by_default` and add its companion:

```rust
    /// One event loop and one shared descriptor unless somebody asks
    /// otherwise. Each extra thread reserves a resident 16 MiB receive buffer
    /// (`MAX_WRITE_SIZE + 4096`, one per thread, never shrunk to the negotiated
    /// `max_write`), which on a 1962 MB guest is 3% per four threads.
    #[test]
    fn the_session_runs_one_event_loop_by_default() {
        let cfg = session_config(1 << 20, false, false, None, false);
        assert_eq!(cfg.n_threads, None);
        assert!(!cfg.clone_fd);
    }

    /// And both travel through when asked. `clone_fd` is what gives each
    /// worker its own `/dev/fuse` descriptor through `FUSE_DEV_IOC_CLONE`;
    /// without it extra threads share one queue and one kernel-side read lock,
    /// which is most of what makes them worth having.
    #[test]
    fn the_thread_settings_reach_the_session() {
        let cfg = session_config(1 << 20, false, false, Some(4), true);
        assert_eq!(cfg.n_threads, Some(4));
        assert!(cfg.clone_fd);
    }
```

In `crates/lbfs-client/src/main.rs`, add beside `attr_timeout_accepts_zero_and_fractions_and_refuses_nonsense`:

```rust
    #[test]
    fn event_loop_threads_refuses_zero_and_absurd_counts() {
        assert_eq!(event_loop_threads(None).unwrap(), None);
        assert_eq!(event_loop_threads(Some(1)).unwrap(), Some(1));
        assert_eq!(event_loop_threads(Some(64)).unwrap(), Some(64));
        assert!(event_loop_threads(Some(0)).is_err());
        assert!(event_loop_threads(Some(65)).is_err());
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p lbfs-client --lib event_loop thread_settings`
Expected: FAIL — `this function takes 3 arguments but 5 arguments were supplied`, and `cannot find function 'event_loop_threads'`.

- [ ] **Step 3: Widen `session_config`**

Add two parameters and two assignments, and extend the doc comment:

```rust
/// `n_threads` and `clone_fd` are off unless a caller says otherwise, and the
/// measurements say to leave them that way for now: the session thread never
/// exceeds 15.6% of a core in any run in
/// `docs/benchmarks/2026-08-22-bottleneck-analysis.md`, and the guest has two
/// vCPUs with tokio workers already on one, so a second event loop competes
/// rather than adds. The cost of turning them on is memory rather than risk —
/// each thread holds its own receive buffer of `MAX_WRITE_SIZE + 4096`, which
/// is 16 MiB and does not shrink to the negotiated `max_write`.
pub fn session_config(
    max_io_size: u32,
    allow_other: bool,
    auto_unmount: bool,
    n_threads: Option<usize>,
    clone_fd: bool,
) -> Config {
```

with the two new lines beside the others:

```rust
    config.n_threads = n_threads;
    config.clone_fd = clone_fd;
    config
```

- [ ] **Step 4: Add the flags and the check**

In `crates/lbfs-client/src/main.rs`, add to `Cli` after `no_writeback`:

```rust
    /// Run this many fuser event-loop threads instead of one.
    ///
    /// Off by default, and expected to stay off on a two-vCPU guest: the
    /// session thread peaks at 15.6% of a core under the heaviest shape
    /// measured, and a second event loop competes with the tokio workers for
    /// the other core. Each thread holds a resident 16 MiB receive buffer that
    /// does not shrink to the negotiated I/O size, so four threads cost 64 MiB.
    /// Pair it with `--fuse-clone-fd` or most of the benefit stays behind a
    /// shared descriptor. Linux only, 1 to 64.
    #[arg(long)]
    fuse_threads: Option<usize>,

    /// Give each event-loop thread its own `/dev/fuse` descriptor.
    ///
    /// `FUSE_DEV_IOC_CLONE`, Linux 4.5 and up. Without it every thread reads
    /// one descriptor and one kernel queue, which is the serialisation extra
    /// threads exist to remove. Means nothing on its own — pass
    /// `--fuse-threads` too.
    #[arg(long)]
    fuse_clone_fd: bool,
```

and the checker beside `attr_timeout`:

```rust
/// One to sixty-four event loops, or none named at all.
///
/// Zero is the value worth catching here rather than downstream: `Session::run`
/// answers a zero with `io::Error::other("n_threads")`, which reaches the
/// operator as a mount failure with no explanation in it. The upper bound is
/// arbitrary and generous — sixty-four threads would reserve a gigabyte of
/// receive buffer, which is more than the guests have.
fn event_loop_threads(n: Option<usize>) -> Result<Option<usize>, StartupError> {
    match n {
        None => Ok(None),
        Some(n) if (1..=64).contains(&n) => Ok(Some(n)),
        Some(_) => Err(StartupError::FuseThreads),
    }
}
```

with the error variant:

```rust
    #[error("--fuse-threads must be between 1 and 64")]
    FuseThreads,
```

and the call in `run()`, replacing the `session_config` line:

```rust
    let n_threads = event_loop_threads(cli.fuse_threads)?;
    let cfg = session_config(
        limits.max_io_size,
        cli.allow_other,
        cli.auto_unmount,
        n_threads,
        cli.fuse_clone_fd,
    );
```

- [ ] **Step 5: Repair the loopback fixture**

One call site, at `tests/tests/loopback.rs:341`:

```rust
        let session = fuser::spawn_mount(
            fs,
            &mnt,
            &session_config(limits.max_io_size, false, false, None, false),
        )
        .expect("the mount succeeds");
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p lbfs-client --lib event_loop thread_settings one_event_loop`
Expected: PASS, all three.

- [ ] **Step 7: Document the flags**

Add two rows to the client flag table in `README.md`:

```markdown
| `--fuse-threads N` | off (one) | Run N fuser event-loop threads. Off by default and expected to stay off on a two-vCPU guest: the session thread peaks at 15.6% of a core under the heaviest measured shape. Each thread reserves a resident 16 MiB receive buffer, so four cost 64 MiB. Pair with `--fuse-clone-fd`. Linux only, 1 to 64. |
| `--fuse-clone-fd` | off | Give each event-loop thread its own `/dev/fuse` descriptor (`FUSE_DEV_IOC_CLONE`, Linux 4.5+). Without it the threads share one queue. Means nothing without `--fuse-threads`. |
```

- [ ] **Step 8: Run the whole gate**

Run: `make check && make test-loopback`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/lbfs-client/src/fuse.rs crates/lbfs-client/src/main.rs \
  tests/tests/loopback.rs README.md
git commit -m "feat(client): --fuse-threads and --fuse-clone-fd, off by default"
```

---

### Task 10: Measure on the VM pair and record the result

**Files:**
- Edit: `docs/benchmarks/2026-08-22-bottleneck-analysis.md`

**Interfaces:**
- Consumes: every task above.
- Produces: the acceptance evidence. No automated test — this one needs two guests and a quiet machine.

Two questions, and the second one is the reason the knob shipped. First: did the upgrade move anything? The expected answer is no, and a plan whose whole argument is "no measured change" owes a measurement. Second: what do extra event loops do on today's guest? The expected answer is also no change, and recording that is what stops somebody reopening the question from memory.

- [ ] **Step 1: Build and deploy**

Run: `make build-guest && make vm-deploy`
Expected: `deployed.` with `lbfs-server` active. If the pair is down, `make vm-up` first.

- [ ] **Step 2: Confirm the mount still negotiates what it used to**

Mount from the client guest and read the client's own log:

```bash
lbfs-client 192.168.77.10:9423 /srv/exports/data /mnt/lbfs 2>&1 | \
  grep -E "mount initialized|unsupported by this kernel"
```

Expected: one `mount initialized` line carrying `max_io=1048576`, `writeback=true`, `attr_ttl=1s` and `entry_ttl=1s`, and no `unsupported by this kernel` line at all. A refusal naming `FUSE_HANDLE_KILLPRIV_V2` means the bump lost the capability and the write numbers below will regress by roughly 90 µs.

- [ ] **Step 3: Measure the five standard shapes with the knob off**

Run the drained single-job driver used for the tables in the benchmark document — each job `direct=1`, each preceded by a drain (`sync`, then poll `/proc/meminfo` until `Dirty + Writeback` falls under 8 MB).

Expected, against the figures already in the document:

| job | on 0.15.1 | expected on 0.18.0 |
|---|---|---|
| seq write 1M psync | 361.4 MB/s, 2757 µs | inside run-to-run spread |
| seq read 1M psync | 1580.2 MB/s, 632 µs | inside run-to-run spread |
| randread 4k psync QD1 | 8322 IOPS, 119.3 µs | inside run-to-run spread |
| randwrite 4k psync QD1 | 3365 IOPS, 296 µs | whatever the kill-priv plan left, inside its own spread |
| randread 4k libaio QD16 | 40290 IOPS, 393 µs | inside run-to-run spread |

Run-to-run spread on this pair is about 20% on the write shapes and tighter on the reads; the document's own Phase 2/Phase 3 columns show the range. Anything outside that range counts as a regression and stops the task.

- [ ] **Step 4: Run the A/B on event-loop threads**

Same five shapes, same drain, three client builds of the same binary differing only in flags:

```bash
# control, already measured in Step 3
lbfs-client 192.168.77.10:9423 /srv/exports/data /mnt/lbfs
# two loops, private descriptors
lbfs-client --fuse-threads 2 --fuse-clone-fd 192.168.77.10:9423 /srv/exports/data /mnt/lbfs
# four loops, private descriptors
lbfs-client --fuse-threads 4 --fuse-clone-fd 192.168.77.10:9423 /srv/exports/data /mnt/lbfs
```

Expected: no change beyond spread on any of the five, on either setting. The guest has two vCPUs and the tokio workers already occupy one of them, so a second event loop competes for the core rather than finding an idle one, and the session thread it would relieve peaks at 15.6% of one core.

Record the resident set with each setting, since that is the cost the numbers do not show:

```bash
ps -o rss= -p "$(pgrep -x lbfs-client)"
```

Expected: roughly 16 MiB more per extra thread. Note the reading whatever it says; if four threads do not cost about 48 MiB above the control, the crate allocates the buffer lazily and the memory argument in the README wants softening.

- [ ] **Step 5: Confirm the threads exist**

```bash
ps -L -o comm= -p "$(pgrep -x lbfs-client)" | sort | uniq -c
```

Expected: with `--fuse-threads 4`, four threads named `fuser-0` through `fuser-3`; with the flag absent, one background session thread and no `fuser-N` names. A missing set means the flag never reached `Config` and Step 4 measured the control twice.

- [ ] **Step 6: Record it**

Append to `docs/benchmarks/2026-08-22-bottleneck-analysis.md`:

```text
## The fuser upgrade, measured

0.15.1 → 0.16.0 (ABI 7.40, pure-Rust mount) → 0.18.0. No shape moved. That was
the prediction and this is the check: the crate's own dispatch thread never
exceeded 15.6% of a core in the Phase 6 attribution, so there was nothing for
a newer session loop to relieve. The value of the two steps is the API, the
dropped libfuse3 link and the road to the release that carries the kill-priv
forwarding fix.

[the five-shape table from Step 3]

### Extra event loops, measured rather than assumed

`Config { n_threads, clone_fd }` arrived with 0.17.0 and is reachable from
0.18.0. The client exposes it as `--fuse-threads` and `--fuse-clone-fd`, off by
default. Same five shapes, same drain, one, two and four event loops with
private descriptors:

[the A/B table from Step 4]

[the resident-set readings from Step 4]

The reading matches the prediction: this guest has two vCPUs, one of them
already carrying tokio workers, so a second reader of /dev/fuse competes for a
core rather than finding an idle one. Each thread holds a 16 MiB receive buffer
that never shrinks to the negotiated max_write. The knob is worth keeping for a
guest with four or more vCPUs running many files concurrently — the shape that
scaled 2.66× in the Phase 4 ladder — and worth leaving off everywhere else.
```

- [ ] **Step 7: Check the prose gate**

Run: `vale --output=line docs/benchmarks/2026-08-22-bottleneck-analysis.md`
Expected: no output.

- [ ] **Step 8: Commit**

```bash
git add docs/benchmarks/2026-08-22-bottleneck-analysis.md
git commit -m "docs(bench): the fuser upgrade moves nothing, and neither do extra event loops"
```

---

### Task 11: Record the pin and the practice behind it

**Files:**
- Edit: `docs/superpowers/specs/2026-08-20-lbfs-design.md` (§9, one paragraph)
- Edit: `README.md` (the Build section, one paragraph)

**Interfaces:**
- Consumes: every task above.
- Produces: the reason the version has an `=` in front of it, written where the next person to run `cargo update` will meet it.

An exact pin with no explanation beside it invites somebody to relax it. One short paragraph in each of the two documents an operator reads, both pointing at the assessment note for the argument.

- [ ] **Step 1: Amend the spec**

Add to `docs/superpowers/specs/2026-08-20-lbfs-design.md` §9, after the deployment paragraph Task 4 rewrote:

```markdown
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
```

- [ ] **Step 2: Amend the README**

Add to `README.md` at the end of the Build section, before "Why a container and not a musl target":

```markdown
The `fuser` dependency is pinned to an exact version rather than a range, and
`cargo update` must not move it on its own. Releases after 0.18.0 come
primarily from a coding agent under a review the maintainer describes as
cursory, and the project no longer takes pull requests, so a patch nobody here
has read could otherwise reach a guest binary through routine housekeeping. The
practice for moving it: read the release diff first, land the crate bump and
any ABI change as separate commits, and run `make test-loopback` either side of
the move — the set-user-ID cases in it are what catch a release that stops
forwarding a kill signal. The reasoning is in
`docs/notes/2026-08-22-fuser-upgrade-assessment.md`.
```

- [ ] **Step 3: Update the pre-epoch limitation**

In `README.md`, find the known-limitations paragraph on pre-1970 timestamps
(near line 211) and append one sentence:

```markdown
The 0.18.0 pin repairs the outbound half of this — replies now carry a
fractional pre-1970 time intact — while the inbound half (`utimensat` through
`system_time_from_time`) still waits on a later fuser release.
```

- [ ] **Step 4: Check the prose gate**

Run: `vale --output=line README.md docs/superpowers/specs/2026-08-20-lbfs-design.md`
Expected: no output.

- [ ] **Step 5: Run the whole gate one last time**

Run: `make check && make test-loopback && make vm-test`
Expected: PASS throughout.

- [ ] **Step 6: Commit**

```bash
git add README.md docs/superpowers/specs/2026-08-20-lbfs-design.md
git commit -m "docs: why the fuser pin is exact, and what moving it costs"
```

---

## Acceptance Criteria

1. `make check` passes after every task: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
2. `make test-loopback` passes after every task, with no case lost along the way. This plan adds three to `tests/tests/loopback.rs` — the drain pair in Task 1 and the split-lifetime case in Task 8 — on top of whatever the two earlier plans left.
3. `privileged_bits_die_on_write_with_the_writeback_cache` and `privileged_bits_die_on_write_without_the_writeback_cache` pass immediately before and immediately after each of the three `Cargo.toml` version edits — Tasks 2, 3 and 6.
4. `writes_reach_the_export_on_unmount_with_the_writeback_cache` and its no-writeback twin pass at every step, and neither ever reaches `Loopback::unmount`'s timeout.
5. `make vm-test` passes after Task 4 and again after Task 6, including the fio `crc32c` verify job, the throughput floor, the build workload and the disconnect drill.
6. `ldd target/guest/release/lbfs-client` names no FUSE library, and `make build-guest` installs no packages into the container.
7. `Cargo.toml` reads `fuser = { version = "=0.18.0" }` — an exact pin, no feature list, no caret.
8. The crate bump and the ABI-declaration bump are two commits, in that order, each independently revertible.
9. `grep -n "^const FUSE_\|^const FOPEN_" crates/lbfs-client/src/fuse.rs` returns nothing after Task 7.
10. On the five standard fio shapes — seq write 1M psync, seq read 1M psync, randread 4k psync QD1, randwrite 4k psync QD1, randread 4k libaio QD16 — every number lands inside the pair's run-to-run spread, about 20% on this hardware.
11. `--fuse-threads 4 --fuse-clone-fd` shows four threads named `fuser-0` through `fuser-3`, costs roughly 16 MiB of resident memory each, and moves no shape beyond spread. The reading is in `docs/benchmarks/2026-08-22-bottleneck-analysis.md` whatever it says.
12. `--entry-timeout` absent behaves exactly as the tree did before Task 8, and `--entry-timeout 3600 --attr-timeout 0.05` still reports a size the server changed behind the mount's back.
13. `git diff` across the whole plan touches no file under `crates/lbfs-proto/` or `crates/lbfs-server/`, and the protocol version is still `2`.

## Open Risks

- **The 0.18.0 sweep is one large commit, and no smaller one would be truthful.** Changing the pin breaks every callback at once, so no intermediate state compiles. Tasks 7 through 9 exist to keep the judgement calls out of it, which leaves a diff that is large but purely mechanical. A reviewer should read Step 12's table and spot-check three callbacks rather than read all 33.
- **`ForgetOne` may become public in a later release, at which point the deleted override is worth revisiting.** Today the trait default calls `forget` once per node and our `send_forget` batches behind the scenes, so the loop costs nothing. If upstream exports the type and adds anything to it that the per-node path loses, this is the place to look.
- **The tripwire is a guarantee check by default, not a forwarding check.** On this deployment the server runs unprivileged, so its own kernel strips set-user-ID inside `write(2)` whatever the client forwards, and the assertion would hold even if fuser dropped every kill signal. The kill-priv plan's own risk list says so and offers the five-line `Explicit` override that closes it. Running `make test-loopback` once under `sudo`, by hand, is what exercises the forwarding path; the suite cannot demand root.
- **The pre-epoch reply fix arrives, and its inbound half does not.** 0.18.0's `time_from_system_time` stops mangling a fractional pre-1970 time on the way to the kernel, which is half of the limitation at `README.md:211`. `system_time_from_time` — the inbound half, reached by `utimensat` — waits for the next release. Leaving the README paragraph as it stands would overstate the problem and rewriting it would understate it; the right move is a sentence noting that this release repairs the outbound half, and Task 11 Step 3 now budgets exactly that sentence (added after the code-review adjudication flagged the gap).
- **`--fuse-threads` is a visible flag rather than a hidden one.** The assessment recommends hiding it. Visible loses nothing today and documents itself, yet it widens the support surface for a knob nobody should turn on this hardware. If it starts appearing in bug reports, `#[arg(long, hide = true)]` costs one line.
- **A four-vCPU guest is the measurement this plan does not take.** Every number behind the "leave the threads off" recommendation comes from two vCPUs with tokio workers already on one of them. Task 10 records the two-vCPU reading, so the next person reopens the question with evidence rather than rearguing it.
- **Reverting past Task 6 means reverting Tasks 7 through 9 with it.** Each of the three rests on 0.18.0 types. That is the ordinary shape of a dependency upgrade, and the two-commit split in step 1 is where the cheap revert lives; past the 0.18.0 pin, the branch is the unit.

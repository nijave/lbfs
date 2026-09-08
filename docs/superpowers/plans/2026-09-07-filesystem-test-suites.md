# Third-Party Filesystem Test Suites

**Status:** Placeholder, written 2026-09-07. This document records the research
and the decisions a real plan needs; the task-by-task plan stays deliberately
unwritten. A later session takes this through the normal process — brainstorm
the open decisions below, then write the implementation plan — rather than
expanding this file in place.

## Why

Every test in this repository is home-grown: unit suites, the protocol
integration layer, the loopback mounts, the VM drills. Two classes of defect
slip past home-grown tests by construction — POSIX conformance gaps (errno
choices, sticky-bit and SUID/SGID semantics, ctime propagation, rename edge
cases nobody thought to pin) and data corruption that only adversarial
randomized I/O finds. Third-party suites exist for both, and the projects most
like lbfs (JuiceFS, SeaweedFS, agentfs) run the same three.

## The three suites, ranked by value per setup cost

1. **fsx** (`ltp/fsx.c` in the xfstests tree, a single C file). Randomized
   read/write/truncate/mmap against one file with a shadow model; any wrong
   byte surfaces with the operation history that produced it. Highest value
   here because lbfs's riskiest surface matches what fsx pounds: the writeback
   cache, `FOPEN_KEEP_CACHE` coherence, the direct-I/O path, and
   reconnect-with-retained-handles. Needs no root. Natural homes: a
   loopback-level `make` target, a VM soak, and a run across a `sever()`.
   The sibling `fsstress` does the same for metadata storms — the node table,
   handle tables and FORGET paths.

2. **pjdfstest** (github.com/pjd/pjdfstest). ~8,800 TAP tests of POSIX syscall
   semantics under `prove(1)`. JuiceFS advertises passing every one;
   SeaweedFS added the suite to CI in April 2026 (PR #9013) and immediately
   fixed sticky-bit enforcement, SUID/SGID clearing on write, and ctime
   consistency — the class of gap a passthrough filesystem accumulates in
   silence. Wants root and a second uid for the ownership tests, so the VM
   client guest is the natural home. The deliverable is the suite **plus a
   curated `known_failures.txt`** documenting which failures the single-user
   trust model earns on purpose — SeaweedFS's runner shows the shape. A Rust
   rewrite exists (musikid/pjdfstest, GSoC 2022), but the C suite is the
   reference the other projects' numbers cite.

3. **xfstests `generic/quick`** (git.kernel.org xfstests-dev). The broadest
   net — bundles fsx and fsstress runs, xattr limits, O_DIRECT semantics,
   mmap coherence — and carries first-class FUSE support (`FSTYP=fuse`,
   `FUSE_SUBTYP`, from Miklos Szeredi's patch; agentfs documents running it
   this way). Also the heaviest integration: the harness mounts via
   `mount -t fuse.lbfs`, so lbfs needs a `mount.fuse.lbfs` helper shim that
   translates mount options into `lbfs-client` flags, plus TEST/SCRATCH
   configuration on the guest. Take it third, after the first two earn their
   keep.

## Decisions the later session must make before writing tasks

- **The skip list is the trust model, written down.** lbfs runs single-user
  by design (the kill-priv work), so a slice of pjdfstest's chown/setuid
  coverage fails on purpose. Decide per failure: fix, or record in
  `known_failures.txt` with the reason.
- **Locks.** The client implements no lock operations, so the kernel falls
  back to client-local `flock`/`fcntl` semantics — suites pass while the
  guarantee stays single-client. Decide whether that earns a spec sentence,
  an implementation, or both.
- **Known deliberate trips.** `mmap(MAP_SHARED)` on an `O_DIRECT` descriptor
  answers `ENODEV` (spec §7 records the cost); xfstests will hit it. Same for
  atime behavior under the current mount options.
- **Where each runs.** Make targets against loopback for the unprivileged
  pieces; `vm/test.sh` steps for the root-needing pieces. CI stays out, per
  the 2026-09-07 decision to leave the loopback/VM suites off CI.
- **Pinning.** Pin suite revisions (SeaweedFS pins a pjdfstest commit via env
  vars) so a drift upstream never reads as an lbfs regression.

## Prior art worth reading first

- JuiceFS POSIX-compatibility page: pjdfstest (all 8,789) plus a curated LTP
  run, with their deletion list for FUSE-inapplicable cases.
- SeaweedFS PR #9013: the `run.sh` harness shape — build pinned suite, copy
  into the mount, `prove -rv`, `known_failures.txt` exclusion.
- agentfs `TESTING.md`: both pjdfstest and xfstests-over-FUSE walkthroughs,
  including the `local.config` for `FSTYP=fuse`.

# The FUSE transport tax, priced for io_uring, 2026-09-07

Two-VM pair: server 192.168.77.10 (`lbfs-server`, export `/srv/exports/data`),
client 192.168.77.11. Ubuntu 26.04, 2 vCPU and 1962 MB each; the client runs
kernel 7.0.0-28-generic, the server 7.0.0-31-generic — an image update since
the 2026-09-05 campaign, which ran both at -28. fio 3.41 and bpftrace 0.25.0 on
the client. Both binaries built from `main` at `05b083d`. No code change
anywhere: the experiment attaches four kprobes.

The question comes from Task 1 of
`docs/superpowers/plans/2026-08-28-fuse-over-io-uring.md`. The mount answers a
4 KiB operation some 28-35 µs slower than the bare RPC layer underneath it, and
`FUSE_OVER_IO_URING` could recover one slice of that gap: the
`read(2)`/`write(2)` pair on `/dev/fuse` and the scheduling around it. The rest
of the gap — fuser's dispatch, the tokio bridge, the reply encode — rides over
whichever transport the mount uses. The plan's bar: proceed only if the
transport slice runs above 25 µs on a ~135 µs operation.

It measures about 14 µs, and that figure over-counts. The plan stops here.

## Headline

Timing the transport boundary directly, a 4 KiB random read at QD1 spends a
median 4.5 µs between the moment the kernel queues the request and the moment
the daemon holds it in userspace, and 10.2 µs between the daemon's reply
entering the kernel and the waiting application thread resuming: 14.2 µs of
transport on a 123.7 µs operation. The 4 KiB write reads the same — 5.3 µs
out, 8.3 µs back, 13.5 µs of 160.9. Four kprobes inflate the traced runs by
about 5 µs, the daemon's wakeup and the application's wakeup both survive a
move to a ring, and the payload copy survives it too, so 14 µs is a ceiling
rather than an estimate. The bar was 25 µs. The syscall pair is not where this
filesystem's small-request latency lives.

## Method

Five interleaved rounds. Inside a round, each shape runs its mount job, then
its raw-RPC job, then — for the QD1 shapes — a second mount job with the
kprobes attached, so probe cost stays out of the anchor numbers. The server
drains before every job (`sync`, then poll `/proc/meminfo` until
`Dirty + Writeback` falls under 8 MB) and the client drops its page cache
before every job. One file — `rpcbench.dat`, 512 MiB, directly under the
export root — serves both columns: fio reaches it through `/mnt/lbfs` with
`direct=1`, `lbfs-bench` reaches the same bytes at 192.168.77.10:9423 with no
FUSE anywhere. 15 s per job, `norandommap`, `randrepeat=0`, mean latency per
operation from fio's `lat_ns` and from `lbfs-bench`'s own report. The mount
stayed up throughout and sat idle during the RPC jobs.

The attribution probes: this kernel inlines `fuse_simple_request` — the
client's `/proc/kallsyms` lacks it, as the plan anticipated — so the
probes sit on `request_wait_answer` and on the `/dev/fuse` boundary functions,
all four present as kallsyms entries:

| mark | probe | meaning |
|---|---|---|
| t0 | `kprobe:request_wait_answer` | request queued, application thread blocks |
| t1 | `kretprobe:fuse_dev_do_read` | daemon holds the request in userspace |
| t2 | `kprobe:fuse_dev_do_write` | daemon's reply enters the kernel |
| t3 | `kretprobe:request_wait_answer` | application thread woken |

**submit** = t1−t0: the request's trip out — daemon wakeup, `read(2)` entry,
header and payload copy-out. **service** = t2−t1: everything userspace does —
fuser decode, bridge dispatch, `conn.call`, the wire round trip, reply encode,
`write(2)` entry. **complete** = t3−t2: the reply's trip back — copy-in,
`fuse_request_end`, waking the application thread. The transport slice a ring
could attack is submit + complete; service crosses no transport.

Global timestamps pair correctly at queue depth 1 only, which is the shape the
bar names, so the traced runs cover the two QD1 shapes. A guard drops any
sample whose four marks are not strictly ordered: at most 39 of ~115k samples
per run. The entry probe filters on the opcode through the kernel's BTF, and
the opcode counters confirm the runs are pure — 115,555 READs against four
stray operations in a representative run.

## The two columns, five rounds

Mean latency per operation, in round order, median bolded last:

| shape | mount (µs) | raw RPC (µs) |
|---|---|---|
| randread 4k qd1 | 125.8 123.7 121.3 130.4 122.7 → **123.7** | 93.5 96.3 94.0 92.3 94.8 → **94.0** |
| randwrite 4k qd1 | 165.7 160.9 159.7 165.8 160.0 → **160.9** | 126.9 131.7 124.1 134.5 125.5 → **126.9** |
| randread 4k qd16 | 385.1 397.2 384.1 368.5 377.8 → **384.1** | 291.1 295.8 281.0 279.6 284.0 → **284.0** |

The within-round difference, the whole FUSE-plus-bridge price:

| shape | mount − RPC, five rounds (µs) | median | 2026-09-05 median |
|---|---|---|---|
| randread 4k qd1 | 32.3 27.4 27.3 38.1 27.9 | **27.9** | 31.6 |
| randwrite 4k qd1 | 38.8 29.2 35.6 31.3 34.5 | **34.5** | 36.6 |
| randread 4k qd16 | 94.0 101.4 103.1 88.9 93.8 | **94.0** | 86.1 |

Same picture as Phase 10 of the bottleneck analysis two days earlier, across a
server kernel update and a fresh build: the gap this plan proposed to attack
holds at roughly 28-35 µs at QD1.

## The split

Traced QD1 runs, one figure per round, medians of five:

| segment, randread 4k | five rounds (µs) | median |
|---|---|---|
| submit (t0→t1) | 4.7 4.3 4.5 3.8 4.9 | **4.5** |
| service (t1→t2) | 108.4 106.1 107.8 108.9 111.4 | **108.4** |
| complete (t2→t3) | 9.5 10.5 10.2 10.3 8.7 | **10.2** |
| **transport (submit + complete)** | 14.2 14.8 14.7 14.1 13.6 | **14.2** |
| total (t0→t3) | 122.5 120.9 122.5 123.0 125.1 | **122.5** |

| segment, randwrite 4k | five rounds (µs) | median |
|---|---|---|
| submit (t0→t1) | 5.3 5.9 4.9 5.3 5.5 | **5.3** |
| service (t1→t2) | 144.3 160.6 144.8 143.5 144.2 | **144.3** |
| complete (t2→t3) | 8.5 8.8 8.3 8.2 7.9 | **8.3** |
| **transport (submit + complete)** | 13.8 14.7 13.2 13.5 13.4 | **13.5** |
| total (t0→t3) | 158.0 175.3 158.0 156.9 157.6 | **158.0** |

The distributions are tight, not skew-rescued: in round 3's read run, 98% of
submit samples land in [2, 8) µs and 97% of complete samples in [4, 16) µs,
and the samples above 32 µs number 43 and 36 out of 115,525.

Three cross-checks say the split is real:

- **The segments add up to the operation.** Traced total (122.5 µs read) sits
  6.6 µs under the same run's fio mean (129.1 µs); that residue is the
  syscall entry, `fuse_direct_io` setup and syscall exit outside the probe
  window — path a ring transport keeps.
- **Transport plus bridge reproduces the gap.** service − raw RPC = 14.4 µs of
  userspace bridge cost on the read; 14.2 (transport) + 14.4 (bridge) = 28.6
  against the 27.9 µs anchor gap. The write: 13.5 + 17.4 = 30.9 against 34.5.
- **Tracing overhead is visible and bounded.** The traced runs' own fio means
  run 129.1 against the untraced 123.7 (read) and 165.8 against 160.9 (write)
  — about 5 µs of probe cost, part of which lands inside the segments and
  inflates them.

## Why 14 µs still over-counts the prize

The ring removes the `read(2)`/`write(2)` path through `/dev/fuse`: syscall
entry and exit, argument handling, the request-queue walk. It does not remove
the two wakeups inside submit and complete — at QD1 an idle daemon sleeps on
the ring's completion queue exactly as it sleeps in `read(2)` today, and the
application thread still gets woken at t3 — and it does not remove the payload
copy, because bulk data does not ride the ring's 256-byte header (plan §1).
Subtract what survives and the recoverable share shrinks from 14 µs toward the
single-digit cost of the syscall pair itself.

## QD16, for completeness

The 94 µs gap at QD16 is depth multiplied by per-operation pipeline cost, not
a per-request 94 µs: at the medians the mount moves an operation every
24.0 µs (41,604 IOPS) against the RPC layer's 20.1 µs (49,815 IOPS), a
throughput cost of 3.3-4.2 µs per operation across the rounds. Batching
completions per wakeup is the ring's honest QD16 case, and ~3.6 µs per
operation is the entire budget it would batch against — on a two-vCPU guest
where the `--fuse-threads` ladder already showed a second `/dev/fuse` reader
buys nothing.

## The ruling

The bar said proceed above 25 µs of syscall-and-scheduling on a ~135 µs
operation. Measured: 14.2 µs on the read, 13.5 µs on the write, as a ceiling
that includes probe overhead and two wakeups the ring keeps. The
FUSE-over-io_uring plan stops at Task 1, and the fork never starts. This is
the third performance campaign in this repository to end at its own gate, after
the big-requests experiment and the `--fuse-threads` ladder.

Two things would reopen the question, and both change the machine rather than
the argument: a many-core guest, where the upstream feature's per-CPU queues
and context-switch avoidance have room to work, or a kernel path that lets the
payload live in an io_uring registered buffer, which would attack the copy
instead of the syscall — spec §11 keeps the survey item with that framing.

## Restore state

- Client guest: `/mnt/lbfs` unmounted, no `lbfs-client` process, the bpftrace
  program, fio JSON and deploy leftovers removed from `/tmp`.
- Server guest: `rpcbench.dat` deleted, the export empty, `lbfs-server` active
  with zero restarts.
- Binaries: both guests keep `main` at `05b083d` — current `main` differs from
  it by documentation only.

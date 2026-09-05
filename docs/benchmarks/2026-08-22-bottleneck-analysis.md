# Bottleneck analysis, 2026-08-22

Campaign on the two-VM pair: server 192.168.77.10 (`lbfs-server` exporting
`/srv/exports/data`), client 192.168.77.11. Ubuntu 26.04, kernel
7.0.0-28-generic, 2 vCPU and 1962 MB each. Code on branch
`perf/async-dio-bench` (commits `9fea609`, `893f56f`).

Three questions drove the day: what `FUSE_ASYNC_DIO` buys, where the time
goes, and how much of the cost belongs to FUSE, to lbfs, and to the wire.

> **Correction, 2026-08-22, after deeper kernel analysis.** The extra round
> trip behind every write is a `GETXATTR` of `security.capability`, not a
> `GETATTR`, and the `open`/`fstat`/`close` triple counted below comes from the
> server's xattr handler rather than from its `GETATTR` handler. Every number
> and every experiment here still holds; only the name of the mechanism was
> wrong, and the text now carries the corrected one.
> `docs/superpowers/plans/2026-08-22-per-write-getattr-elimination.md` walks
> the Linux v7.0 sources behind the correction.

## Headline

Asking the kernel for `FUSE_ASYNC_DIO` lifts every queued direct-I/O shape.
Random 4 KiB reads at queue depth 16 climb from 8.1k to 40.3k IOPS — a
factor of five — because without the flag the kernel holds `i_rwsem` across
each direct-I/O request and lets one operation per inode run at a time. The
negotiated window of 128 had nothing to pipeline; fio's depth 16 reached the
bridge as depth 1.

Writes do not move, and the reason is separate: every `WRITE` through the
mount drags a `GETXATTR` behind it, so a write costs two round trips where a
read costs one. The kernel probes `security.capability` before each write to
decide whether the file must lose its set-user-ID bit, and lbfs's honest
`ENODATA` never persuades it to stop. Turning the writeback cache off does not
change this, and neither does a 30-second attribute timeout, because no
attribute cache covers an xattr probe.

## Method note: writeback backlog contaminates back-to-back jobs

The first pass through the eight-job suite produced write numbers that swung
by a factor of four between runs. The cause sits on the server: lbfs strips
`O_DIRECT` server-side by design, so client writes land in the server's page
cache, and a job that dirties 512 MB in a 1962 MB guest leaves the next job
running against `balance_dirty_pages`. During one such run the server showed
80-90% iowait with no read traffic at all.

Every A/B table below comes from a driver that drains the server's
dirty pages (`sync`, then poll `/proc/meminfo` until `Dirty + Writeback` falls
under 8 MB) before each single job. The raw first-pass numbers appear in the
anomalies section.

## Phase 2 vs Phase 3: the ASYNC_DIO A/B

Both columns come from the same warm files, one job at a time, with the
server drained between jobs. The old client is `e6ebfaf`'s `fuse.rs` rebuilt
and redeployed for the control, so the only difference between the columns is
the capability bit.

| job (direct=1, 512M, 15 s) | old, no ASYNC_DIO | new, ASYNC_DIO | 2026-08-21 baseline |
|---|---|---|---|
| seq write 1M psync | 376.5 MB/s, 2647 µs | 361.4 MB/s, 2757 µs | 1113 MB/s, 936 µs |
| seq read 1M psync | 1731.2 MB/s, 577 µs | 1580.2 MB/s, 632 µs | 1626 MB/s, 644 µs |
| randread 4k psync QD1 | 8305 IOPS, 119.4 µs | 8322 IOPS, 119.3 µs | 8263 IOPS, 120 µs |
| randwrite 4k psync QD1 | 3481 IOPS, 285.9 µs | 3365 IOPS, 296 µs | 3463 IOPS, 287 µs |
| **randread 4k libaio QD16** | **8109 IOPS, 1851 µs** | **40290 IOPS, 393 µs** | 7217 IOPS, 2079 µs |

The queued shapes, measured back to back on one pair of mounts for a tighter
comparison:

| job | old | new | change |
|---|---|---|---|
| randread 4k libaio QD16 | 8109 IOPS, 1851 µs | 40290 IOPS, 393 µs | +397% |
| randwrite 4k libaio QD16 | 3481 IOPS, 4309 µs | 4963 IOPS, 3023 µs | +43% |
| seq read 1M libaio QD8 | 1333 MB/s, 5249 µs | 1790 MB/s, 4440 µs | +34% |
| seq write 1M libaio QD8 | 813 MB/s, 8605 µs | 874 MB/s, 8009 µs | +7.5% |

The old client's QD16 latency tells the story on its own: 1851 µs is
15.5 × the 119 µs of a single 4 KiB read. Sixteen requests, one at a time.
With the flag the same job runs at 393 µs mean, roughly 3.3 × the QD1
latency across 16 outstanding requests.

Nothing at QD1 changes, which is the right shape for a flag that only
governs how many direct-I/O requests may overlap.

The client log carries no "unsupported by this kernel" line for any
capability, so the kernel granted `FUSE_ASYNC_DIO` along with
`FUSE_DO_READDIRPLUS`, `FUSE_READDIRPLUS_AUTO` and `FUSE_WRITEBACK_CACHE`.
The behaviour change confirms the grant far better than the absent warning
does.

## Phase 4: the isolation ladder

All four columns address the same two files under `/srv/exports/data/bench`,
all with `direct=1`, all drained between jobs. The first three rungs run on
the server guest.

| job | native | bindfs | lbfs loopback | lbfs network |
|---|---|---|---|---|
| seq write 1M psync | 3608 MB/s, 270 µs | 2859 MB/s, 345 µs | 522 MB/s, 1909 µs | 361 MB/s, 2757 µs |
| seq read 1M psync | 5773 MB/s, 173 µs | 4926 MB/s, 203 µs | 2558 MB/s, 390 µs | 1580 MB/s, 632 µs |
| randread 4k psync | 26953 IOPS, 36.5 µs | 35719 IOPS, 27.7 µs | 12728 IOPS, 77.5 µs | 8322 IOPS, 119.3 µs |
| randwrite 4k psync | 23273 IOPS, 42.5 µs | 44660 IOPS, 22.0 µs | 4907 IOPS, 202 µs | 3365 IOPS, 296 µs |
| randread 4k QD16 | 175345 IOPS, 86.5 µs | 62808 IOPS, 239 µs | 34007 IOPS, 465 µs | 40290 IOPS, 393 µs |

Two caveats keep the bindfs column from reading as "the cost of bare FUSE".

First, bindfs beats native on both 4 KiB jobs. The reason is the data path,
not the FUSE layer: I checked `/proc/<pid>/fdinfo` for the backing
descriptor while each mount took writes, and both bindfs (`0100002`) and
lbfs (`02100002`) hold the file open without `O_DIRECT`. A 4 KiB operation
through either FUSE mount lands in the server's page cache, while
native `direct=1` goes to the virtual disk. bindfs measures RAM; native
measures virtio.

Second, the loopback rung runs fio, `lbfs-client` and `lbfs-server` on two
vCPUs. Its 1 MiB numbers carry that contention, which is why loopback
sometimes reads slower than the network mount on the streaming shapes.

Read the ladder as latency deltas per 4 KiB operation instead:

| step | randread 4k | randwrite 4k |
|---|---|---|
| bindfs (C libfuse over the page cache) | 27.7 µs | 22.0 µs |
| lbfs loopback | 77.5 µs | 202 µs |
| lbfs network | 119.3 µs | 296 µs |

lbfs software plus one loopback socket adds ~50 µs to a read and ~180 µs to
a write over a C passthrough. The wire adds a further ~42 µs to a read and
~94 µs to a write, against a hot ping RTT of 218 µs round trip on this pair
today (0.218 ms average over five ICMP packets, versus the 31 µs of
yesterday's hot measurement).

## Phase 5: raw RPC, no FUSE anywhere

`lbfs-bench` drives the same `Connection` the bridge drives, from a plain
binary, so the gap between these rows and the matching mount rows prices the
kernel's FUSE layer for this stack.

| shape | loopback (server guest) | network (client guest) |
|---|---|---|
| read 1M qd1 seq | 2151 MB/s, 446 µs | 2272 MB/s, 419 µs |
| read 1M qd8 seq | 2101 MB/s, 3641 µs | 2431 MB/s, 3241 µs |
| write 1M qd1 seq | 1885 MB/s, 504 µs | 1391 MB/s, 698 µs |
| write 1M qd8 seq | 2167 MB/s, 3159 µs | 1709 MB/s, 4510 µs |
| read 4k qd1 rand | 12981 IOPS, 63.1 µs | 9086 IOPS, 92.2 µs |
| write 4k qd1 rand | 9490 IOPS, 88.9 µs | 6148 IOPS, 146.4 µs |
| read 4k qd16 rand | 47347 IOPS, 307 µs | 48766 IOPS, 291 µs |

Subtracting the network RPC column from the network mount column gives what
the kernel's FUSE layer costs this stack per operation:

| shape | mount | raw RPC | FUSE cost |
|---|---|---|---|
| randread 4k qd1 | 119.3 µs | 92.2 µs | +27 µs (+29%) |
| randwrite 4k qd1 | 296 µs | 146.4 µs | +150 µs (+102%) |
| randread 4k qd16 | 393 µs | 291 µs | +102 µs (+35%) |
| seq read 1M qd1 | 632 µs | 419 µs | +213 µs (+51%) |
| seq write 1M qd1 | 1142 µs* | 698 µs | +444 µs (+64%) |

\* the least throttled 1 MiB write measurement of the day, 869 MB/s during
the CPU capture. See the anomalies section for the range this job covers.

The RPC layer pipelines well once FUSE stops serialising: 4 KiB random reads
go from 9086 IOPS at qd1 to 48766 at qd16, a 5.4 × gain on 16 × the depth.
Streaming does not pipeline — 1 MiB reads sit at ~2.3 GB/s whether one or
eight ride in flight, so something other than concurrency bounds that shape.

## The per-write `GETXATTR`

`strace -c -f` against `lbfs-server` for 10-12 s windows, one workload per
window:

| workload | reply frames (`writev`) | `open`/`fstat`/`close` triples |
|---|---|---|
| randread 4k psync | 31362 | 0 |
| randwrite 4k psync | 12872 | 6430 |
| seq write 1M psync | 9832 | 4911 |

The triple belongs to the server's xattr handler
(`crates/lbfs-server/src/fs/local/mod.rs:425-442`): it `fstat`s the node's
`O_PATH` descriptor, reopens the node through `/proc` because `fgetxattr`
refuses an `O_PATH` descriptor, and closes the reopened one afterwards. The
`GETATTR` handler does none of that — it runs one `statx` on the descriptor it
already holds, through the io_uring ring, where `strace` cannot see it. One
triple per write, and twice as many reply frames as write operations, in both
the 4 KiB and the 1 MiB case. Reads show neither, because the read path never
calls `file_remove_privs`.

A write through the mount thus costs a `WRITE` round trip plus a `GETXATTR`
round trip. Before every write to an inode without `S_NOSEC`, the kernel asks
the filesystem for `security.capability`; lbfs answers `ENODATA`, and only
`ENOSYS` makes FUSE stop asking, so the probe repeats for the life of the
mount. The arithmetic lands almost exactly: 146 µs (RPC write) + 92 µs
(a metadata round trip, priced at the RPC read) + ~58 µs of kernel write-path
work = 296 µs, the measured mount latency.

The 1 MiB case also shows that the kernel sends one `WRITE` frame per 1 MiB
application write. Nothing fragments the transfer.

## The exclusive inode lock

Four threads against one file, versus four threads against four files:

| shape | IOPS | mean latency |
|---|---|---|
| randwrite 4k psync, 1 thread, 1 file | 3325 | 300 µs |
| randwrite 4k psync, 4 threads, 1 file | 3275 | 1220 µs |
| randwrite 4k psync, 4 threads, 4 files | 8840 | 451 µs |

Four writers on one file deliver 0.98 × the throughput of one writer and
exactly 4 × the latency. That is a per-inode exclusive lock, serialising
writers whatever the queue depth. Four writers on four files scale 2.66 ×,
which is what two vCPUs and the round-trip cost allow.

## Phase 6: CPU attribution

Three 15 s jobs through the network mount, `pidstat -t -u 1` on both sides,
`mpstat -P ALL` for the box view. Percentages are of one core; each guest has
two.

| run | client `lbfs-client` | busiest client thread | server `lbfs-server` | busiest server thread |
|---|---|---|---|---|
| seq read 1M, 1544 MB/s | 55.7% | tokio worker 27.9% | 53.0% | tokio worker 19.7% |
| seq write 1M, 869 MB/s | 41.6% | fuse session 15.6% | 47.3% | `iou-wrk` 17.6% |
| randwrite 4k, 3463 IOPS | 30.6% | tokio worker 12.8% | 46.1% | tokio worker 13.7% |

Box totals:

| run | client idle | client sys / softirq | server idle | server sys / iowait |
|---|---|---|---|---|
| seq read 1M | 61.8% | 21.9% / 13.0% | 38.9% | 24.5% / 30.7% |
| seq write 1M | 73.6% | 19.1% / 3.6% | 31.5% | 22.8% / 33.7% |
| randwrite 4k | 77.9% | 13.4% / 4.6% | 34.5% | 17.7% / 38.0% |

No thread saturates on either side. The heaviest single thread all day is a
client tokio worker at 27.9% of a core during streaming reads, and the two
workers split the load evenly, so the bridge's task-per-callback design does
spread work. The fuser session thread stays under 16% even when it carries
1 MiB request bodies off `/dev/fuse`. The server spends far more time waiting
than computing: 30-38% iowait against 18-25% system time, and during the
4 KiB write run CPU 0 sat at 76% iowait while CPU 1 idled at 69%. That wait
belongs to the virtual disk absorbing page-cache writeback, not to lbfs.

Total per side, summing user and system across all threads: the client burns
roughly 0.3-0.56 of a core, the server roughly 0.46-0.53. Neither guest runs
out of CPU at any point in this campaign.

## Phase 7: the writeback hypothesis, refuted

Three mount configurations, randwrite 4k psync QD1, direct=1, drained
before each:

| mount | IOPS | mean | p99 |
|---|---|---|---|
| default (writeback on, ttl 1 s) | 3469 | 287 µs | 391 µs |
| `--no-writeback` | 3490 | 285 µs | 387 µs |
| writeback on, `--attr-timeout 30` | 3466 | 288 µs | 408 µs |

Sequential 1 MiB writes agree: 874 MB/s without writeback against 816 MB/s
with it, inside the run-to-run spread of that job. The writeback cache is not
the reason a write costs 2.4 × a read. The extra round trip per write survives
both changes — the `--no-writeback` window still shows 10149 triples against
20303 reply frames, and the 30-second attribute timeout still shows 9766
against 19535. Neither knob touches it, because the kernel repeats an xattr
probe rather than an attribute refresh: `--no-writeback` skips the pre-write
refresh outright, and the attribute timeout governs a cache the probe never
consults.

## Bottleneck attribution

The answer differs by workload shape, and no single layer owns them all.

**Queued random reads were the FUSE layer's fault, and the flag fixes them.**
Before today the kernel serialised direct I/O per inode, so a 16-deep job ran
one request at a time and the 128-request window sat idle. `FUSE_ASYNC_DIO`
turns 8.1k IOPS into 40.3k, and the raw-RPC ceiling of 48.8k IOPS at the same
depth says the protocol has roughly 20% left to give.

**Small random reads are round-trip bound, and lbfs owns most of that round
trip.** A 4 KiB read costs 119 µs through the mount, of which 27 µs belongs
to FUSE, 42 µs to the wire, and 50 µs to lbfs's own software above what a C
passthrough spends. The wire itself is not the problem — iperf3 measured
33-37 Gbit/s yesterday and the ping RTT is a fifth of a millisecond.

**Small random writes pay for a second round trip, and that is the whole
gap.** Reads take one RPC, writes take two, because the kernel probes
`security.capability` before each write and lbfs's `ENODATA` never stops it.
The kernel's pre-write size and mode refresh is not the culprit: the writeback
cache answers it locally, so it costs roughly one `GETATTR` per attribute
timeout rather than one per write. Removing the probe — `FUSE_HANDLE_KILLPRIV_V2`
lets the kernel latch `S_NOSEC` and stop asking, in exchange for the server
clearing the privileged mode bits itself — would be worth about 90 µs on a
296 µs operation.

**Concurrent writers to one file gain nothing at all.** The per-inode
exclusive lock in the kernel's write path pins throughput at the single-writer
rate no matter how many threads push. Only separate files scale. (Phase 9
below lifts this for opens that ask for `O_DIRECT`: answering them with
`FOPEN_DIRECT_IO | FOPEN_PARALLEL_DIRECT_WRITES` moves those writes to the
shared lock, and four writers on one file then reach 89 % of four writers on
four files. Every other open still measures exactly what this paragraph
says.)

**Per-megabyte software cost bounds streaming reads and writes, rather than
concurrency or the network.** Raw RPC tops out near 2.3 GB/s read and
1.7 GB/s write on this pair, and adding queue depth moves neither. FUSE adds
213 µs per megabyte read and ~444 µs per megabyte written on top. Against a
36 Gbit/s link that price buys copies and syscalls — the client copies each
megabyte from `/dev/fuse` into a buffer and again into the socket, and the
server does the mirror image.

**Server page-cache writeback caps sustained writes below every rate above.**
Because the server strips `O_DIRECT`, sustained write workloads eventually
run at whatever rate the server's kernel flushes dirty pages to the virtual
disk. On a 1962 MB guest that limit arrives after a few hundred megabytes,
and it separates the 1002 MB/s a fresh file accepts from the 361 MB/s a
saturated cache allows.

## Anomalies

**The 1 MiB sequential write job swings by 4 ×.** Measurements today, all
through the network mount: 243, 361, 376, 816, 869, 874, 920 and 1002 MB/s.
The high end comes from a fresh file with a cool server cache; the low end
comes from a run that follows a heavy write job. Yesterday's 1113 MB/s sits
at the top of that range and describes a burst, not a sustained rate. Any
future write comparison needs the drain step, and probably a smaller file or
a bigger server guest.

**The first pass through the eight-job suite is unusable for writes.** With
no drain between jobs the new client reported 384 MB/s sequential write, 806
IOPS random write and 827 IOPS at QD16 with a 19 ms mean. Rerunning the old
client in the same state gave 244 MB/s and 787 IOPS, which is how I ruled the
capability out as the cause. Both sets of numbers describe the server's
writeback queue, not the client.

**bindfs outruns native on 4 KiB work** by 1.3-1.9 ×, as described in the
ladder section. The FUSE mounts write to the page cache; native `direct=1`
writes to the disk.

**The loopback rung shares two vCPUs among three processes,** so its
streaming numbers understate what lbfs software alone would do.

**Random reads at QD16 beat the ladder's loopback rung over the network**
(40290 against 34007), for the same reason.

**`strace -c` roughly halves throughput** while attached — 8350 IOPS becomes
4227 on random reads. The first Phase 7 pass ran with strace attached and
produced 1750 and 1679 IOPS; the clean rerun in the table above shows the
truth. Syscall counts from those windows stay valid because they are ratios.

**The server can no longer cache the whole working set.** Two 512 MB files
plus the bench file exceed the ~1.5 GB of page cache a 1962 MB guest offers,
so part of every read comes off the virtual disk. Server iowait during the
streaming read run confirms it at 30.7%.

## Phase 8: the per-write probe, removed

Measured after the change landed on branch `perf/kill-priv` (`587bc00`): same
pair, same drained single-job driver, same two 512 MiB files, fio 3.41.

The extra round trip per write was a `GETXATTR` of `security.capability`, not
a `GETATTR`. The kernel issues it from `file_remove_privs`
(`fs/inode.c:2317-2341`) before every write on an inode that lacks `S_NOSEC`,
and a FUSE superblock only gains `SB_NOSEC` when the server negotiates
`FUSE_HANDLE_KILLPRIV_V2` (`fs/fuse/inode.c:1411-1414`). lbfs answers the probe
with ENODATA, which the kernel never latches, so it repeated forever. The
server's `xattr_fd` reopens the node through `/proc` to run `fgetxattr`, and
that reopen is the open/fstat/close triple the earlier window counted.

Asking for the flag removes the probe; honouring the promise it encodes moves
the set-user-ID strip onto the server. Three runs per shape, median first and
the range across the three beside it:

| job (direct=1, 512M, 15 s) | before | after | range across three runs |
|---|---|---|---|
| randwrite 4k psync QD1 | 3365 IOPS, 296 µs | 6576 IOPS, 151.0 µs | 6467-6769 IOPS, 147.0-153.7 µs |
| randread 4k psync QD1 | 8322 IOPS, 119.3 µs | 8327 IOPS, 119.1 µs | 8239-8448 IOPS, 117.6-120.6 µs |
| randread 4k libaio QD16 | 40290 IOPS, 393 µs | 43507 IOPS, 367.1 µs | 43472-44199 IOPS, 361.3-367.5 µs |
| seq read 1M psync | 1580 MB/s, 632 µs | 1814 MB/s, 550.2 µs | 1724-1843 MB/s, 541.8-579.1 µs |

The write shape halves: 296 µs becomes 151 µs and 3365 IOPS becomes 6576. Its
p99 clat follows, 216-239 µs across the three runs against the 391 µs Phase 7
recorded for the same shape. The plan behind the change predicted ~4800 IOPS at
~205 µs for this row, and priced the removed round trip at 90 µs; the run came
in at 151 µs, a 145 µs saving.

Two readings fit that overshoot, and this campaign cannot separate them. Either
the reopen and the `spawn_blocking` hop cost more than the 92 µs a bare
metadata round trip prices — the probe path does strictly more work than a
`GETATTR` — or the 146.4 µs the raw RPC layer charged for a 4 KiB write that
day ran pessimistic. The nearness of the new 151 µs to that 146.4 µs reads as a
warning rather than as confirmation: at face value it leaves the kernel's FUSE
layer costing a write about 5 µs where it costs a read 27 µs, and it collapses
both the +150 µs row in the FUSE-cost table above and the ~58 µs of kernel
write-path work that table's arithmetic assumed. A fresh `lbfs-bench` write
pass beside a fresh mount pass would tell the two readings apart. Until then
the 145 µs saving stands as a measurement and its decomposition stays open.

The three read shapes hold. Random 4 KiB reads at QD1 land within 0.2% of the
earlier figure, and that is the control which makes the write result a change
in the write path rather than a faster pair today. The queued read and the
streaming read come in 8% and 15% ahead, both inside the run-to-run spread this
pair carries: the 1 MiB sequential read alone has landed anywhere from
1519.7 MB/s in the 4 MiB request campaign (`2026-08-22-big-requests.md`, job 2)
to 1843 MB/s today, a band of about 20%.

Syscall counts from `strace -c -f` on `lbfs-server`, one 12 s randwrite window
per row:

| window | reply frames (`writev`) | `open` | `fstat` | `close` |
|---|---|---|---|---|
| before | 12872 | 6430 | 6430 | 6430 |
| after, 1 s attribute timeout | 31589 | 12 | 0 | 12 |
| after, 30 s attribute timeout | 31634 | 0 | 0 | 0 |

Read the rows as ratios, never as throughput. Each row pays its own tracer tax,
and that tax scales with the traced syscalls per write: five before, roughly
one now. The before row works out to 536 writes/s — 6430 reopens over 12 s,
12872 frames, two frames per write — against the 3365 IOPS that shape reaches
clean. The middle row's job averaged 5097 IOPS over 30 s with the tracer
attached for 12 of them, which puts the traced stretch near 2900 writes/s
against 6576 clean, the halving the anomalies section describes, and close to
the 2632 frames/s that 31589 over 12 s works out to. One frame per write, then,
where the before row shows two; the 2.45 × frame count in the after row
measures a lighter tracer on a shorter path, not a throughput jump of that size.

The triple became a pair as well: the node table now carries the file type, so
`xattr_fd` skips the `fstat` it used to run. Twelve reopens across twelve
seconds is one per second, and that rate is the attribute timeout —
`fuse_change_attributes_common` clears `S_NOSEC` on every attribute reply, so
each expiry buys the kernel one more probe. A mount with `--attr-timeout 30`
shows none at all over the same window, which places the residue on that path
and nowhere else. A workload that interleaves `stat` with writes pays more of
these probes than fio's pure write loop does.

The vanished probe is what shows the kernel granted the capability. A grant
leaves no line in the client log — only a refusal logs — so the absent
"unsupported by this kernel" line rules a refusal out rather than showing a
grant, and a kernel that withheld the flag would go on asking once per write.
The server's `policy=Kernel` line at attach names the strip branch the server
chose, right for a unit running as `ubuntu` without `CAP_FSETID`, and says
nothing about the kernel's answer to the client. Through the mount, a file with
mode `4755` drops to `755` on a 4 KiB write and again on a truncate, while a
file with mode `2664` keeps its set-group-ID bit across the same two
operations — the VFS rule, now on the server's side of the wire.

## Environment notes and follow-ups

* Kernel 7.0.0-28-generic on both guests. `fs.fuse.max_pages_limit = 256`,
  which is 1 MiB — the same ceiling the handshake negotiates. Raising both
  together is a follow-up worth trying, since fuser's `set_max_write` accepts
  up to 16 MiB and the streaming shapes are per-operation-cost bound.
* The kernel caps `set_max_readahead` at the bdi's `read_ahead_kb`, which
  reads 128 on the client's FUSE mount. lbfs asks for 1 MiB of readahead and
  gets 128 KiB. Buffered sequential reads thus fetch in 128 KiB steps
  whatever the mount asks for; today's `direct=1` jobs bypass readahead
  entirely, so no number here reflects that cap.
* `lbfs-bench` addresses one file directly under the export root. `LOOKUP`
  refuses a name containing a slash, so a path like `bench/rand.dat` returns
  `EINVAL`.
* The raw RPC layer carries a write/read asymmetry that no current plan
  targets: a 4k write costs 146.4 µs against the read's 92.2 µs over the
  network, and 88.9 against 63.1 over loopback, so roughly 26-54 µs sits in
  the server's write handling before FUSE enters the picture. The NFS
  comparison sharpens the point — kernel NFS answers a complete 4k write in
  ~104-111 µs, less than lbfs's bare RPC write. Spec §11 carries the
  follow-up analysis items (kernel-module client feasibility, server-side
  kernel integration survey).

## Cleanup performed

* Client guest: `/mnt/lbfs` unmounted, no `lbfs-client` process, temporary
  files removed.
* Server guest: `/mnt/lbfs-loop` and `/mnt/bindfs` unmounted and their
  mountpoints removed, no `lbfs-client` or `bindfs` process, `bench/` and the
  bench file gone from the export, temporary files removed.
* `lbfs-server` stays active on the server guest running the new binary
  (`c8e301160b4387929ba2c1c9938ef010`, matching `target/guest/release`).
* Both domains still running.
* `sysstat` remains on both guests and `bindfs` plus `strace` remain on the
  server guest.

The Phase 8 run left the pair the same way:

* Client guest: `/mnt/lbfs` unmounted, no `lbfs-client` process, the bench tree
  and the run's temporary files removed.
* Server guest: the export empty again, `lbfs-server` active with no restarts,
  no tracer attached, the drain script removed.
* `lbfs-server` now runs the `perf/kill-priv` build,
  `fb5066ce618ac0855c1d0fd3eca04a5d`, which supersedes the md5 above.
* Both domains still running.

## The fuser upgrade, measured (2026-08-27)

0.15.1 → 0.16.0 (ABI 7.40, pure-Rust mount) → 0.18.0. No shape moved. That was
the prediction and this is the check: the crate's own dispatch thread never
exceeded 15.6% of a core in the Phase 6 attribution, so there was nothing for
a newer session loop to relieve. The value of the two steps is the API, the
dropped libfuse3 link and the road to the release that carries the kill-priv
forwarding fix.

The Phase 2/3 tables above are not the baseline for this A/B: the kill-priv
work landed between them and this measurement, and it moved the write shapes
on its own (randwrite 4k sat at 296 µs mean with the per-write
`security.capability` probe and answers 165-172 µs without it). Both columns
below come instead from the same day, the same guest pair and the same
drained single-job driver — `main` at `341f6d3` (fuser 0.16.0) against the
upgrade branch at `311f5a0` (fuser 0.18.0), direct=1, 512M, 15 s per job.

| job | 0.16.0 (`341f6d3`) | 0.18.0 (`311f5a0`) | change |
|---|---|---|---|
| seq write 1M psync | 1304.1 MB/s, 803 µs | 1249.6 MB/s, 838 µs | −4.2%, inside spread |
| seq read 1M psync | 1843.6 MB/s, 568 µs | 1734.8 MB/s, 604 µs | −5.9%, inside spread |
| randread 4k psync QD1 | 8538 IOPS, 116.4 µs | 8513 IOPS, 116.7 µs | −0.3% |
| randwrite 4k psync QD1 | 5776 IOPS, 172 µs | 6004 IOPS, 166 µs | +3.9%, inside spread |
| randread 4k libaio QD16 | 43166 IOPS, 370 µs | 43251 IOPS, 369 µs | +0.2% |

The 0.18.0 mount negotiated what it used to: `max_io=1048576`,
`writeback=true`, both lifetimes at 1 s, and no `unsupported by this kernel`
line — the `FUSE_HANDLE_KILLPRIV_V2` ask survived the bump, which the
randwrite latency confirms from the outside.

### Extra event loops, measured rather than assumed

`Config { n_threads, clone_fd }` arrived with 0.17.0 and is reachable from
0.18.0. The client exposes it as `--fuse-threads` and `--fuse-clone-fd`, off by
default. Same five shapes, same drain, one, two and four event loops with
private descriptors, all on the 0.18.0 build:

| job | one loop | `--fuse-threads 2` | `--fuse-threads 4` |
|---|---|---|---|
| seq write 1M psync | 1249.6 MB/s, 838 µs | 1130.9 MB/s, 926 µs | 1023.3 MB/s, 1023 µs |
| seq read 1M psync | 1734.8 MB/s, 604 µs | 1611.6 MB/s, 650 µs | 1833.8 MB/s, 571 µs |
| randread 4k psync QD1 | 8513 IOPS, 117 µs | 7226 IOPS, 138 µs | 8265 IOPS, 120 µs |
| randwrite 4k psync QD1 | 6004 IOPS, 166 µs | 6139 IOPS, 162 µs | 6033 IOPS, 165 µs |
| randread 4k libaio QD16 | 43251 IOPS, 369 µs | 39952 IOPS, 400 µs | 40018 IOPS, 399 µs |

No shape improves on either setting. The sequential-write column drifts down
the table, but that is the shape this document already flags as swinging with
server page-cache pressure, and the drift sits inside its spread.

`ps -L` confirms the threads exist — `fuser-0` through `fuser-3` with
`--fuse-threads 4` — and also that 0.18.0 names its single default loop
`fuser-0`, so "no fuser-N threads" is not the signature of a missing flag; the
count is.

**The 16 MiB-per-thread cost is virtual, not resident.** Fresh mounts read
5188 kB RSS with one loop, 5516 kB with two and 5788 kB with four — roughly
150-300 kB per extra thread, nowhere near `BUFFER_SIZE`. After a sequential
write and a QD16 random read: 7488 kB with one loop against 13680 kB with
four, about 2 MB resident per extra thread. The allocation exists per thread,
but its pages fault in only as far as requests touch them, and a 1 MiB
negotiated `max_write` touches a sixteenth of each buffer. The prediction of
16 MiB resident per thread came from reading the allocation; the measurement
says an operator on this configuration pays an eighth of that.

The reading matches the prediction otherwise: this guest has two vCPUs, one of
them already carrying tokio workers, so a second reader of /dev/fuse competes
for a core rather than finding an idle one. The knob stays worth keeping for a
guest with four or more vCPUs running many files concurrently — the shape that
scaled 2.66× in the Phase 4 ladder — and worth leaving off everywhere else.

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

Measured on the two-guest pair, both builds carrying the window-permit fix
that precedes this change. Three rounds, alternating builds round by round
rather than running one pass after the other, with a server-side drain before
every job; the figures are medians of three. The interleaving is not
ceremony — a straight before-then-after pass had the sequential write at
1065 MB/s in the first pass and 66 MB/s in the second, and re-measuring the
first build reproduced the second number. That row tracks the host's page
cache, not the mount, and only alternating separates the two.

| probe | job | control | with the change | |
|---|---|---|---|---|
| A | randwrite 4k psync QD1, 4 threads, **1 file** | 6938 IOPS, 576 µs | **15910 IOPS, 250 µs** | **2.29 ×** |
| A control | randwrite 4k psync QD1, 1 thread, 1 file | 7064 IOPS, 141 µs | 7357 IOPS, 135 µs | 1.04 × |
| A control | randwrite 4k psync QD1, 4 threads, 4 files | 18030 IOPS, 220 µs | 17909 IOPS, 222 µs | 0.99 × |
| B | randwrite 4k libaio QD16 | 22795 IOPS, 697 µs | 22673 IOPS, 701 µs | 0.99 × |
| C | randread 4k psync QD1 | 8445 IOPS, 118 µs | 8417 IOPS, 118 µs | 1.00 × |
| C | randread 4k libaio QD16 | 42046 IOPS, 376 µs | 42959 IOPS, 369 µs | 1.02 × |
| C | seq read 1M psync | 1512 MB/s | 1520 MB/s | 1.01 × |
| C | seq write 1M psync | 85 MB/s | 85 MB/s | 1.00 × |
| D | randwrite 4k psync QD1, `direct=0` | 34572 IOPS, 26 µs | 35575 IOPS, 26 µs | 1.03 × |
| D | second `dd` read of a warm file | 8.1 GB/s | 12 GB/s | page cache intact |

**Four writers on one file now reach 89 % of four writers on four files**
(15910 against 17909), where they reached 38 % before. That ratio is the
result: the per-inode lock, not the transport, was what separated the two
shapes, and the shared lock closes nearly the whole gap. Single-writer and
four-file throughput both stay put, which is what says the win came from
concurrency rather than from a cheaper write.

**The kernel took the flags, on two independent observations.**
`mmap(MAP_SHARED)` on an `O_DIRECT` descriptor returns `ENODEV` where it
mapped before, while an ordinary descriptor still maps — the kernel's own
signal that it stored `FOPEN_DIRECT_IO` (`file.c:2393-2399`). And counting
entries into the two write paths during probe B: 171035 calls to
`fuse_cache_write_iter` and none to `fuse_direct_write_iter` on the control,
289741 the other way round with the change, with no cross-over in either
direction. This kernel inlines `fuse_dio_wr_exclusive_lock` itself
(`grep -c` of `/proc/kallsyms` returns 0), so the lock branch is not directly
observable; probe A against its four-file control is the measurement that
stands in for it.

**Probe B does not move, and the reason is that queue depth never needed the
lock.** The plan expected 4 KiB writes at QD16 to gain what probe A gained,
and predicted the FUSE event loop as the likely obstacle if they did not.
They did not, and the event loop is not it: `--fuse-threads 4` measured 36572
and 36388 IOPS against 37776 and 37237 for the single default loop. The
explanation is upstream of both. A libaio write is not a synchronous iocb, so
`fuse_direct_IO` already returned `-EIOCBQUEUED` before this change
(`file.c:2892-2894`), so the exclusive lock lasted only long enough to queue
the request rather than spanning the round trip. The lock costs what it costs
when a `pwrite` waits inside it, which is the psync shape probe A measures.
Probe B is bimodal on this pair — 22-23k IOPS in the campaign, 37-38k in the
later runs, on both builds alike — and tracks server-side cache state.

**Buffered opens keep every bit of today's behaviour**, the other half of
the contract:
`direct=0` random write holds at ~35k IOPS with the writeback cache still
aggregating, and a second `dd` of a warm file still comes out of the page
cache at gigabytes per second, so `FOPEN_KEEP_CACHE` survived on both reply
shapes.

`mmap(MAP_SHARED)` on an `O_DIRECT` descriptor now returns `ENODEV`. The mount
declines to negotiate `FUSE_DIRECT_IO_ALLOW_MMAP` on purpose — fuser 0.18
reaches the bit, so this is a choice about coherence rather than a limit — and
spec §11 carries the follow-up.

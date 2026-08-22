# NFS comparison, 2026-08-22

Kernel NFSv4.2 on the same two-VM pair that produced the lbfs numbers, under
the same eight fio jobs and the same drain step. Server 192.168.77.10, client
192.168.77.11, Ubuntu 26.04, kernel 7.0.0-28-generic, 2 vCPU and 1962 MB each.
The `lbfs-server` unit stayed up and untouched throughout; the NFS export used
a separate directory beneath the same export root.

Two full passes: one with the export marked `sync`, one with it marked
`async`. The lbfs column reproduces the 2026-08-22 drained figures from the
new client carrying `FUSE_ASYNC_DIO`.

## Setup

| Item | Value |
|---|---|
| Server package | `nfs-kernel-server` 1:2.8.5-1ubuntu1 |
| Client package | `nfs-common` 1:2.8.5-1ubuntu1 |
| Export path | `/srv/exports/data/nfsbench` |
| Export line, pass 1 | `192.168.77.11(rw,sync,no_subtree_check,no_root_squash)` |
| Export line, pass 2 | `192.168.77.11(rw,async,no_subtree_check,no_root_squash)` |
| Effective flags | `wdelay,hide,sec=sys,secure,no_all_squash` on top of the above |
| Client mount | `mount -t nfs -o vers=4.2 192.168.77.10:/srv/exports/data/nfsbench /mnt/nfs` |
| Working set | `seq.dat` and `rand.dat`, 512 MiB each, laid out once with `dd oflag=direct` |
| fio | 3.41 |

## Mount options the client negotiated

From `/proc/self/mountstats`, which prints the attribute-cache timers that
`nfsstat -m` hides at their defaults:

```
rw,vers=4.2,rsize=262144,wsize=262144,namlen=255,
acregmin=3,acregmax=60,acdirmin=30,acdirmax=60,
hard,fatal_neterrors=none,proto=tcp,timeo=600,retrans=2,
sec=sys,clientaddr=192.168.77.11,local_lock=none
```

The same line came back after the `async` switch, so the two passes differ
only in what the server promises about durability.

| Option | Value |
|---|---|
| `vers` | 4.2 |
| `rsize` / `wsize` | 262144 / 262144 |
| `actimeo` | unset, so `acregmin=3 acregmax=60 acdirmin=30 acdirmax=60` |
| `proto` / `timeo` / `retrans` | tcp / 600 / 2 |
| Semantics | `hard`, `local_lock=none`, `sec=sys` |

A 1 MiB application write thus reaches the wire as four 256 KiB WRITE calls,
and a 1 MiB read as four READs.

## Method

Identical to the lbfs runs. Shared fio flags:

```
--size=512M --runtime=15 --time_based --direct=1 --numjobs=1 \
--group_reporting --output-format=json
```

Before every single job, a drain on the server guest: `sync`, then poll
`/proc/meminfo` until `Dirty + Writeback` drops under 8 MB. Sequential jobs
address `seq.dat`, random jobs address `rand.dat`, and both files stay warm
across the suite.

Between the two passes I rewrote `/etc/exports`, ran `exportfs -ra`, dropped
caches on both guests, remounted, and drained before the first job.

## Results, `sync` export

| job | value | mean µs | p99 µs |
|---|---|---|---|
| seq write 1M psync | 443.4 MB/s | 2352.2 | 3588.1 |
| seq read 1M psync | 1993.3 MB/s | 525.0 | 856.1 |
| randread 4k psync QD1 | 10087 IOPS | 98.4 | 152.6 |
| randwrite 4k psync QD1 | 939 IOPS | 1063.1 | 1482.8 |
| randread 4k libaio QD16 | 37109 IOPS | 424.7 | 823.3 |
| randwrite 4k libaio QD16 | 8107 IOPS | 1958.1 | 3129.3 |
| seq read 1M libaio QD8 | 2159.8 MB/s | 3769.9 | 6848.5 |
| seq write 1M libaio QD8 | 849.9 MB/s | 9616.2 | 16318.5 |

## Results, `async` export

| job | value | mean µs | p99 µs |
|---|---|---|---|
| seq write 1M psync | 1010.8 MB/s | 1028.2 | 2539.5 |
| seq read 1M psync | 1812.4 MB/s | 577.3 | 1003.5 |
| randread 4k psync QD1 | 7432 IOPS | 133.7 | 255.0 |
| randwrite 4k psync QD1 | 8935 IOPS | 110.6 | 181.2 |
| randread 4k libaio QD16 | 36427 IOPS | 432.4 | 856.1 |
| randwrite 4k libaio QD16 | 30390 IOPS | 520.5 | 1204.2 |
| seq read 1M libaio QD8 | 1867.0 MB/s | 4365.5 | 8290.3 |
| seq write 1M libaio QD8 | 1063.0 MB/s | 7582.8 | 15401.0 |

The two read rows in this pass ran against a server cache that the mandated
`drop_caches` had just emptied. See the anomalies section for the warm
re-runs, which land on the `sync` column.

## The comparison

| job (direct=1, 512M, 15 s) | NFS sync | NFS async | lbfs |
|---|---|---|---|
| seq write 1M psync | 443.4 MB/s, 2352 µs | 1010.8 MB/s, 1028 µs | 361.4 MB/s, 2757 µs |
| seq read 1M psync | 1993.3 MB/s, 525 µs | 1812.4 MB/s, 577 µs | 1580.2 MB/s, 632 µs |
| randread 4k psync QD1 | 10087 IOPS, 98.4 µs | 7432 IOPS, 133.7 µs | 8322 IOPS, 119.3 µs |
| randwrite 4k psync QD1 | 939 IOPS, 1063 µs | 8935 IOPS, 110.6 µs | 3365 IOPS, 296 µs |
| randread 4k libaio QD16 | 37109 IOPS, 425 µs | 36427 IOPS, 432 µs | 40290 IOPS, 393 µs |
| randwrite 4k libaio QD16 | 8107 IOPS, 1958 µs | 30390 IOPS, 521 µs | 4963 IOPS, 3023 µs |
| seq read 1M libaio QD8 | 2159.8 MB/s, 3770 µs | 1867.0 MB/s, 4366 µs | 1790 MB/s, 4440 µs |
| seq write 1M libaio QD8 | 849.9 MB/s, 9616 µs | 1063.0 MB/s, 7583 µs | 874 MB/s, 8009 µs |

lbfs as a fraction of NFS `async`, the mode that matches its durability
stance:

| job | lbfs / NFS async |
|---|---|
| seq write 1M psync | 36% |
| seq read 1M psync | 87% (99% against the warm re-run) |
| randread 4k psync QD1 | 112% (86% against the warm re-run) |
| randwrite 4k psync QD1 | 38% |
| randread 4k libaio QD16 | 111% |
| randwrite 4k libaio QD16 | 16% |
| seq read 1M libaio QD8 | 96% |
| seq write 1M libaio QD8 | 82% |

## Repeat samples

The sequential write job swung by four times during the lbfs campaign, so
each mode got three extra warm, drained repeats:

| mode | suite | repeat 1 | repeat 2 | repeat 3 |
|---|---|---|---|---|
| NFS sync, seq write 1M psync | 443.4 MB/s | 422.3 MB/s | 420.7 MB/s | 449.6 MB/s |
| NFS async, seq write 1M psync | 1010.8 MB/s | 1074.9 MB/s | 1071.6 MB/s | 1041.2 MB/s |

Neither mode reproduces the lbfs collapse. NFS `async` holds a 1.0-1.1 GB/s
sequential write across every drained warm sample, where lbfs fell from a
~1000 MB/s fresh-file burst to 361 MB/s once its server's page cache
saturated. Both stacks land client writes in the server's page cache, so the
difference points at how each one paces writeback rather than at the wire.
`wdelay` plus a 256 KiB RPC size gives nfsd larger, better-ordered batches to
hand the virtual disk.

Warm re-runs of the two `async` read jobs, after the server cache refilled:

| job | cold first pass | warm 1 | warm 2 | warm 3 |
|---|---|---|---|---|
| randread 4k psync QD1 | 7432 IOPS, 133.7 µs | 9449 IOPS, 105.0 µs | 9576 IOPS, 103.4 µs | 10028 IOPS, 98.8 µs |
| seq read 1M psync | 1812.4 MB/s, 577 µs | 1982.8 MB/s, 528 µs | — | — |

Random 4 KiB writes repeat tightly in both modes: `async` gave 8935, 9345,
9487 and 9614 IOPS; `sync` gave 939 and 945 IOPS.

## Where the durability line falls

lbfs answers a write as soon as the server's page cache holds the bytes. It
sends no commit and makes no stable-storage promise, so a server crash loses
whatever the kernel had not yet flushed. That is exactly the `async` export
contract, which makes `async` the column that prices lbfs against NFS on equal
terms.

The `sync` export forces the other contract: nfsd flushes each write to
stable storage before it answers. The price shows up whole in the 4 KiB
random write row — 939 IOPS against 8935, a factor of 9.5, and a mean latency
of 1063 µs against 110.6 µs on a wire whose hot round trip today measured
31 µs at the floor and 36 µs on average. Per-op RPC accounting from `/proc/self/mountstats`
puts the WRITE round trip at 1009 µs under `sync` and 83.7 µs under `async`,
so the server's flush owns roughly 925 µs of every small synchronous write.
Read it as the bill lbfs would face if it grew a durability promise, not as a
defect of NFS.

## The per-write GETATTR gap

lbfs pays a `GETATTR` round trip behind every `WRITE`, worth about 90 µs on a
296 µs operation. NFS pays nothing of the kind, and the operation counters say
so directly. Deltas across one 15-second 4 KiB random write job on the `async`
export:

| op | count | mean RPC round trip |
|---|---|---|
| WRITE | 140188 | 83.7 µs |
| GETATTR | 1 | — |
| COMMIT | 0 | — |
| OPEN / CLOSE / ACCESS | 1 each | — |

fio counted 140,178 write operations in that window. One RPC per write, one
`GETATTR` for the whole job, no commit traffic. The matching read job shows
150,426 READs against the same single `GETATTR`. The client's 3-second
`acregmin` covers the attribute refresh, and NFSv4 write replies carry post-op
attributes, so nothing invalidates the cache the way lbfs's write reply does.

What the NFS number implies about the lbfs gap:

| step | latency |
|---|---|
| NFS async, 4k randwrite, fio-visible | 110.6 µs (repeats 103-106 µs) |
| NFS async, WRITE RPC round trip | 83.7 µs |
| NFS client stack above the RPC | ~22 µs |
| lbfs mount, 4k randwrite | 296 µs |
| lbfs raw RPC, 4k randwrite, no FUSE | 146.4 µs |

Deleting the extra round trip would move lbfs from 296 µs to roughly 206 µs.
That closes half the distance and leaves NFS still twice as fast. The rest
sits below the FUSE layer: lbfs's own RPC path costs 146.4 µs per 4 KiB write
with no kernel filesystem in the picture at all, which already exceeds NFS's
complete mount-to-reply latency of ~104 µs. The `GETATTR` is worth removing,
then, and will not on its own reach parity — the protocol path needs the same
attention.

Reads tell a friendlier story. NFS spends 75.0 µs on the READ round trip and
98.8 µs end to end; lbfs spends 119.3 µs end to end. A 21% gap on a shape where
both stacks pay one round trip per operation.

## Queue depth 16

| shape | NFS sync | NFS async | lbfs | lbfs raw RPC |
|---|---|---|---|---|
| randread 4k QD16 | 37109 IOPS | 36427 IOPS | 40290 IOPS | 48766 IOPS |
| randwrite 4k QD16 | 8107 IOPS | 30390 IOPS | 4963 IOPS | — |

Queued random reads are the one shape where lbfs beats kernel NFS outright:
40.3k against 36.4k, an 11% lead, with lower mean latency too (393 µs against
432 µs). The export mode does not touch this row, as it should not. The
`FUSE_ASYNC_DIO` fix did more than repair a regression, then — it carried
lbfs past the reference implementation on its best shape. The 48.8k raw-RPC
ceiling says another 21% remains inside the protocol, and even the ceiling
sits only 34% above what nfsd delivers, so the lead is real but narrow.

Queued random writes invert the ranking. NFS `async` reaches 30.4k IOPS
against 4963 for lbfs, a factor of 6.1, and even `sync` with its per-write
flush manages 8107. lbfs gains only 47% from QD1 to QD16 on writes (3365 to
4963) where NFS gains 240% (8935 to 30390). Two known lbfs behaviours explain
the flat curve: the per-inode exclusive lock in the kernel write path pins
concurrent writers to one file at the single-writer rate, and the `GETATTR`
doubles the round trips that queue has to hide. NFS avoids both — its client
issues WRITEs against a stateid without serialising on the inode, and it never
asks for attributes.

The `sync` QD16 write row deserves a note of its own: 8107 IOPS is 8.6 times
the `sync` QD1 figure of 939, far more scaling than `async` shows. Depth lets
nfsd fold concurrent writes into shared disk flushes, so the per-write fsync
cost amortises across the queue.

## Sequential ceilings against the wire

iperf3 measured 36.58 Gbit/s forward and 33.71 Gbit/s reverse on this pair.
Converting the best figure each stack reached:

| shape | best MB/s | as Gbit/s | share of the wire |
|---|---|---|---|
| NFS read, QD8 sync | 2159.8 | 17.3 | 47% of 36.6 |
| lbfs read, QD8 | 1790 | 14.3 | 42% of 33.7 |
| NFS write, QD8 async | 1063.0 | 8.5 | 23% of 36.6 |
| lbfs write, QD8 | 874 | 7.0 | 19% of 36.6 |

Nothing here approaches the link. The best read run leaves 53% of the wire
idle and the best write run leaves 77%. Both stacks hit the same wall the
bottleneck analysis identified: per-megabyte software cost on two vCPUs, paid
in copies and syscalls. Adding queue depth buys NFS reads 8% (1993 to 2160
MB/s) and lbfs reads 13% (1580 to 1790), which is the signature of a limit that
concurrency cannot lift.

The ranking is consistent and modest. NFS reads 8-26% faster than lbfs
depending on depth, and NFS `async` writes 22% faster at QD8. Against the warm
re-run the QD1 sequential read gap narrows to 1983 against 1580 MB/s, still
26%. For a userspace filesystem crossing a real socket against an in-kernel
server with 40 years of tuning, landing within a quarter of it on streaming
reads is a reasonable place to be.

## Anomalies

**The `async` read rows in the first pass ran cold.** The instructions call for
a `drop_caches` on both guests when switching export modes, and the `async`
suite starts with a write job followed immediately by the two read jobs, so
`rand.dat` and `seq.dat` came off the virtual disk rather than the server's
page cache. Three warm re-runs put `randread 4k psync` at 9449, 9576 and 10028
IOPS against the cold 7432, and `seq read 1M psync` at 1983 MB/s against the
cold 1812. The `sync` column, which faced no cache drop, reads 10087 IOPS and
1993 MB/s. Read paths behave the same under both export modes, as the protocol
predicts, and the QD16 read rows — which ran later in the suite with the cache
refilled — agree at 37109 against 36427.

**The drain never took longer than one second in either mode.** Under `sync`
there is nothing to drain by construction. Under `async` the 512 MiB working
set bounds how many pages a `time_based` job can dirty no matter how long it
runs, so a single `sync` clears the backlog. lbfs saw the same bound; its writeback
problem came from the rate at which the server's kernel could retire those
pages, not from an unbounded dirty set.

**NFS `async` sustained 1.0-1.1 GB/s sequential writes where lbfs collapsed to
361 MB/s.** Four drained warm samples, no downward drift. Since both stacks
write into the server's page cache, this gap belongs to writeback pacing rather
than to durability semantics, and it makes the 36% lbfs-to-NFS write ratio the
most actionable number in this document.

**`nfsstat --zero` needs privileges this run did not use**, so the per-op
figures come from `/proc/self/mountstats` deltas taken around each job instead.
Those deltas are per-mount and exact; the cumulative `nfsstat -c` totals in my
notes mix workloads, so no one should treat them as per-job figures.

**The `sync` QD1 random write at 939 IOPS is the slowest number in this
document by a wide margin** and the only one that a faster disk would move
much. It measures the server's flush path, not NFS.

## Cleanup performed

| Step | Verification |
|---|---|
| `umount /mnt/nfs` on the client | `grep -c nfs /proc/mounts` returns 0 |
| `/mnt/nfs` mountpoint removed | `ls -d /mnt/nfs` reports no such file |
| `/etc/exports` entry removed | file truncated to 0 bytes |
| `exportfs -ra` | `exportfs -v` prints nothing |
| `systemctl disable --now nfs-server` | active: `inactive`, enabled: `disabled` |
| `/srv/exports/data/nfsbench` deleted | `find /srv/exports/data -mindepth 1` returns 0 entries |
| `lbfs-server` untouched | `systemctl is-active lbfs-server` returns `active` |
| Domains | `lbfs-server` and `lbfs-client` both running |

`nfs-kernel-server` stays installed on the server guest and `nfs-common` on the
client, both inert. Removing them takes one `apt-get purge` on each if a later
run wants a clean image.

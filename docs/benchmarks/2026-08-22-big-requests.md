# Bigger FUSE requests, 2026-08-22

Two-VM pair: server 192.168.77.10 (`lbfs-server`, export `/srv/exports/data`),
client 192.168.77.11. Ubuntu 26.04, kernel 7.0.0-28-generic, 2 vCPU and
1962 MB each. Experiment code on branch `perf/big-requests` (`00f1672`),
which raises `DEFAULT_MAX_IO_SIZE` from 1 MiB to 4 MiB.

The question: the bottleneck analysis found streaming throughput bound by
per-request software cost rather than by the wire (36 Gbit/s) or by CPU, so
quadrupling the request size should quarter the request count and lift the
rate. It does not. Bigger requests slow sequential writes by 30% and buy reads
at most 14%, and the shape that wins every comparison is the one that keeps
requests small and runs four or sixteen of them at once.

## What moved

One constant. `DEFAULT_MAX_IO_SIZE` becomes `4 << 20`, and both ends carry it
end to end already: the server floors and clamps it in `HELLO`, sizes its
pooled buffers from it and reads `max_io_size` out of its config file, while
the client proposes it, mounts with `max_read=`, and hands it to
`set_max_write`. Three dependents follow the constant instead of repeating the
old literal — the config default test, `lbfs-bench`'s block-size ceiling, and
the client's "never above the proposal" handshake check.

On the client guest, `sysctl -w fs.fuse.max_pages_limit=4096` (runtime only,
no file under `/etc/sysctl.d`, no reboot). The default of 256 pages caps a
FUSE request at 1 MiB whatever the handshake says.

## The negotiation took

Client log and mount table, with the sysctl raised:

```
INFO lbfs_client::conn: attached max_inflight=128 max_io_size=4194304 writeback=true
INFO lbfs_client::fuse: mount initialized max_io=4194304 writeback=true ttl=1s
lbfs /mnt/lbfs fuse rw,nosuid,nodev,relatime,...,max_read=4194304 0 0
```

No warning from `set_max_write`, which would name the nearest value the kernel
would take. `strace` on the client's session thread during twelve 4 MiB
`O_DIRECT` writes shows what the kernel actually hands over:

```
max_pages_limit=4096:  12 × read() = 4194368       (12 requests for 48 MiB)
max_pages_limit=256:   12 × read() = 1048640
                       36 × read() = 1048656       (48 requests for 48 MiB)
```

4194368 is 4 MiB plus a 64-byte header. One application write, one request.
With the sysctl back at 256 the same 4 MiB write arrives as four 1 MiB
requests, which is what makes the control column below a true control: same
binaries, same negotiated 4 MiB ceiling, only the kernel's split boundary
differs.

## Method

Every job: `direct=1`, `size=512M`, `runtime=15`, `time_based`, `numjobs=1`,
through the network mount, against two pre-laid 512 MiB files (one for reads,
one for writes). Before every single job the driver runs `sync` on the server
guest and polls `/proc/meminfo` until `Dirty + Writeback` falls under 8 MB —
the drain the previous campaign showed to be mandatory. Every drain in this
campaign finished in one second.

Latency columns are fio's `lat_ns.mean` and `clat_ns` p99, in microseconds.

## The eight-job sequence, 4 MiB requests

Baselines come from the 2026-08-22 bottleneck campaign at a 1 MiB ceiling.
The gap in the first row is the reason the sequence alone cannot answer the
question — see the control column that follows.

| job (direct=1, 512M, 15 s) | MB/s | mean µs | p99 µs | earlier baseline |
|---|---|---|---|---|
| 1. seq write 1M psync | 851.1 | 1230.6 | 3981.3 | 361 MB/s, 2757 µs |
| 2. seq read 1M psync | 1519.7 | 688.9 | 1089.5 | 1580 MB/s, 632 µs |
| 3. seq write 4M psync | 884.9 | 4735.4 | 8847.4 | — |
| 4. seq read 4M psync | 1299.8 | 3224.1 | 5079.0 | — |
| 5. seq write 4M libaio QD4 | 916.3 | 18297.7 | 21364.7 | 874 MB/s QD8, 8009 µs |
| 6. seq read 4M libaio QD4 | 1324.0 | 12661.7 | 22675.5 | 1790 MB/s QD8, 4440 µs |
| 7. seq write 16M psync | 1010.8 | 16589.4 | 32636.9 | — |
| 8. seq read 16M psync | 900.7 | 18618.1 | 27656.2 | — |

Job 1 at 851 MB/s against a 361 MB/s baseline says nothing about request size:
a 1 MiB application write rides a 1 MiB request in both configurations, and
the client's syscall counters confirm it (12176 reads off `/dev/fuse` for
12176 MiB moved). The whole delta belongs to the server's cache state, the
4× swing the earlier campaign called out. Job 8 is the same story in reverse:
the 16 MiB read ran straight after a 15-second write job that had evicted the
read file, and a clean rerun of the same job gave 1268 MB/s.

## Same-session control: 1 MiB split, same binaries

`sysctl -w fs.fuse.max_pages_limit=256`, remount, rerun. Both columns run the
branch build with a 4 MiB negotiated ceiling; only the kernel's split boundary
differs.

| job | 4 MiB requests | 1 MiB requests | change |
|---|---|---|---|
| seq write 1M psync | 851.1 / 805.6 MB/s | 821.5 MB/s | none (same request size) |
| seq read 1M psync | 1519.7 / 1547.0 MB/s | 1302.2 MB/s | none (same request size) |
| seq write 4M psync | 884.9 / 841.0 MB/s | 1106.6 MB/s | 1 MiB wins |
| seq read 4M psync | 1299.8 / 1340.9 MB/s | 1083.9 MB/s | 4 MiB wins |
| seq write 4M QD4 | 916.3 MB/s | 1101.1 MB/s | 1 MiB wins |
| seq read 4M QD4 | 1324.0 MB/s | 1369.6 MB/s | a wash |
| seq write 16M psync | 1010.8 / 1085.9 MB/s | 1235.6 MB/s | 1 MiB wins |
| seq read 16M psync | 900.7 / 1268.0 MB/s | 1280.8 MB/s | a wash |

The 1 MiB rows swing by up to 19% between passes with nothing changed, so the
table above cannot carry the conclusion on its own either.

## Interleaved A/B

Remount between every pair, alternate the two configurations, repeat. Drift in
the server's cache state hits both columns equally.

`bs=4M psync`, three repetitions each:

| rep | write, 4 MiB req | write, 1 MiB req | read, 4 MiB req | read, 1 MiB req |
|---|---|---|---|---|
| 1 | 741.6 MB/s | 1122.0 MB/s | 1111.5 MB/s | 990.6 MB/s |
| 2 | 749.1 MB/s | 1061.1 MB/s | 1237.7 MB/s | 1044.2 MB/s |
| 3 | 826.9 MB/s | 1127.5 MB/s | 1219.3 MB/s | 1086.9 MB/s |
| mean | **772.5** | **1103.5** | **1189.5** | **1040.6** |
| mean latency | 5436 µs | 3799 µs | 3531 µs | 4032 µs |

`bs=16M psync`, two repetitions each:

| rep | write, 4 MiB req | write, 1 MiB req | read, 4 MiB req | read, 1 MiB req |
|---|---|---|---|---|
| 1 | 1025.9 MB/s | 1241.4 MB/s | 1045.6 MB/s | 1262.0 MB/s |
| 2 | 1032.2 MB/s | 1289.7 MB/s | 1108.4 MB/s | 1267.0 MB/s |
| mean | **1029.1** | **1265.6** | **1077.0** | **1264.5** |
| mean latency | 16294 µs | 13252 µs | 15584 µs | 13260 µs |

Every repetition agrees with its column mean. At `bs=4M` the smaller request
takes writes up 43% and costs reads 14%. At `bs=16M` the smaller request wins
both: writes up 23%, reads up 17%.

## Cost per megabyte

The number the experiment set out to shrink:

| shape | 1 MiB requests | 4 MiB requests |
|---|---|---|
| mount, seq read | 677-804 µs/MiB | 783-883 µs/MiB |
| mount, seq write | 1231-1300 µs/MiB | 1184-1359 µs/MiB |
| raw RPC, seq read QD1 | 546 µs/MiB | 553 µs/MiB |
| raw RPC, seq write QD1 | 820 µs/MiB | 811 µs/MiB |

Flat, in all four rows. A 4 MiB request costs almost exactly four times a
1 MiB request, which means the fixed per-request part of the cost is small
next to the per-byte part.

## Raw RPC, no FUSE anywhere

`lbfs-bench` from the client guest, 15 s per run, server drained before each:

| shape | MB/s | mean µs | p99 µs |
|---|---|---|---|
| read 4M qd1 seq | 1780.1 | 2211.2 | 4586.4 |
| read 1M qd1 seq | 1758.2 | 545.8 | 880.5 |
| write 4M qd1 seq | 1221.0 | 3244.0 | 6016.7 |
| write 1M qd1 seq | 1185.3 | 819.5 | 2513.0 |
| read 4M qd4 seq | 1383.6 | 11472.8 | 21318.8 |
| write 4M qd4 seq | 1313.9 | 11892.7 | 20022.2 |

Frame size buys the protocol layer 1.3% on reads and 3% on writes — noise.
Queue depth costs the read path 22%, matching the earlier finding that
streaming shapes do not pipeline through the raw RPC path.

## Analysis

Bigger requests did not lift streaming throughput, and the cost-per-megabyte
table says why: the per-request fixed cost that the plan aimed at is a small
part of what a megabyte costs. A 4 MiB read through the mount takes 3531 µs
against 4 × 806 µs for the same bytes in 1 MiB pieces, and a 4 MiB write takes
5436 µs against 4 × 1300 µs — four times the bytes, four times the wait, plus
a little. The cost lives in the copies: `/dev/fuse` to buffer, buffer to
socket, and the mirror image on the server.

What the 1 MiB column wins with is concurrency. The kernel splits one large
`O_DIRECT` call into requests and, with `FUSE_ASYNC_DIO`, issues them
together, so `bs=16M` over 1 MiB requests puts sixteen frames on the wire at
once and reaches 1265 MB/s write — the best sequential write of the campaign
and 3.5× the 361 MB/s baseline. The same call over 4 MiB requests puts four
frames in flight and manages 1029. Raising the request size trades away
pipelining depth, which is the resource that pays.

That is the NFS finding restated from the other side. Kernel NFS sustains
1011 MB/s writes on this pair with 256 KiB RPCs, four times smaller than
lbfs's smallest, because it keeps many of them in flight. The data here favors
client-side write pipelining over bigger requests without qualification: the
protocol window is 128, the deepest shape measured today used 16, and the
per-byte cost that dominates a large request does not shrink when the request
grows.

Nothing came close to the wire. The best read of the day, 1547 MB/s through
the mount, is 34% of the 4500 MB/s that 36 Gbit/s allows; the best write,
1290 MB/s, is 29%; raw RPC at 1780 MB/s reaches 40%. Two vCPUs per guest and
the copy path bound every one.

## Memory

The 4 MiB pool buffer raises the theoretical in-flight ceiling to 512 MiB per
side (128 window × 4 MiB), which on a 1962 MB guest deserves attention. It
stayed theoretical: `VmHWM` for the whole campaign reached 71 MB on
`lbfs-server` and 21.7 MB on `lbfs-client`, because QD1 and QD4 workloads
never approach a 128-deep window. A workload that does would want
`max_io_size` in the server's config file rather than the default.

## Anomalies

**Job 1 reads 851 MB/s against a 361 MB/s baseline** for an identical request
size. Server cache state, not the code — the earlier campaign measured this
job anywhere from 243 to 1002 MB/s and warned about exactly this.

**Job 8 (read 16M) reads 900.7 MB/s**, and the same job rerun without a write
job before it reads 1268 MB/s. A 15-second write job evicts the read file from
the server's page cache; the interleaved A/B avoids this by pairing every
comparison.

**The 1 MiB read job swings 1302-1547 MB/s across passes**, a 19% spread with
nothing changed. Any single-pass comparison below about 20% means nothing on
this pair, which is why the A/B carries the conclusion.

**Raw RPC reads lose 22% going from QD1 to QD4** at 4 MiB. Streaming shapes
through `lbfs-bench` did not pipeline in the earlier campaign either.

**`/proc/<pid>/io` records no read syscalls for `lbfs-server`** while the
client writes gigabytes to it. The socket read path does not pass through
`vfs_read`, so `rchar` misses it. Client-side counters and `strace` carried
the request-size verification instead.

## Restore

* Client guest: `/mnt/lbfs` unmounted, no `lbfs-client` process,
  `fs.fuse.max_pages_limit` back to 256, my temporary files removed.
* Server guest: `find /srv/exports/data -mindepth 1` prints nothing.
  `lbfs-server` active.
* Both guests run main-matching binaries rebuilt from `b089e2f`:
  `lbfs-server` `c8e301160b4387929ba2c1c9938ef010`, `lbfs-client`
  `1b172c97e16b21e4fc9dd56d899338b5`, `lbfs-bench`
  `23542ac9d89f2dc3d9eb518792ea90fe`, each matching `target/guest/release`.
* Repository on `main`, clean. Branch `perf/big-requests` (`00f1672`) keeps
  the experiment code, unmerged and unpushed.
* Both domains still running. No guest rebooted at any point.

# Readahead, 2026-08-28

Two-VM pair: server 192.168.77.10 (`lbfs-server`, export `/srv/exports/data`),
client 192.168.77.11. Ubuntu 26.04, kernel 7.0.0-28-generic, 2 vCPU and
1962 MB each. No code change — the experiment writes one sysfs knob.

The question comes from the follow-ups section of
`2026-08-22-bottleneck-analysis.md`: the client asks the kernel for 1 MiB of
readahead through `set_max_readahead`, the kernel clamps that to the backing
device's `read_ahead_kb` of 128, and every benchmark this repository had run
used `direct=1`, which bypasses readahead entirely. The cap had never cost a
measured number either way.

It costs half the throughput of a buffered sequential read.

## Headline

Raising `read_ahead_kb` from 128 to 1024 roughly doubles buffered sequential
reads, and the curve flattens exactly at 1024 because that is where the
readahead window meets the negotiated 1 MiB `max_io_size`. Random reads do not
change, the wire carries slightly *fewer* bytes, and the cost is about 6 MB of
client memory. At 4096 throughput stops improving and p99 latency inflates two
to three times, so 1024 is the setting rather than "as high as it goes".

## Method

Every job: `direct=0`, 512 MiB file, single pass, QD1, `fadvise_hint=0`. Three
interleaved rounds — 128, 1024, 4096, repeated — with the server drained
(`sync`, then poll `/proc/meminfo` until `Dirty + Writeback` falls under 8 MB)
and the client's page cache dropped before every single job. Medians of three,
with the individual values shown, because this pair drifts with the host's page
cache and one campaign here already produced a phantom 16× swing from running
one configuration after the other instead of alternating them.

Reads and writes ran as two separate interleaved campaigns, so the 512 MiB
write job could not evict the read file from the server's page cache mid
comparison.

## Buffered sequential read, MB/s

| job | ra=128 | ra=1024 | ra=4096 | best gain |
|---|---|---|---|---|
| `bs=128k` | 806 926 915 → **915** | 1704 1556 1864 → **1704** | 1710 1683 1721 → **1710** | **+87%** |
| `bs=1M` | 934 843 811 → **843** | 1627 1556 1864 → **1627** | 1530 1579 1579 → **1579** | **+93%** |
| `bs=4M` | 937 918 919 → **919** | 1339 1254 1428 → **1339** | 1459 1534 1491 → **1491** | **+62%** |
| `dd bs=1M`, cold | 793 913 722 → **793** | 1589 1625 1487 → **1589** | 1495 1412 1607 → **1495** | **+100%** |

**The `direct=1` control did not move**: 1721, 1715, 1721 MB/s across the three
settings, a 0.3% spread against an 87-100% move in the buffered rows. That
control is what makes the rest of this table worth reading — it separates the
knob from the drift.

At `ra≥1024` a buffered 128 KiB read reaches 1704 MB/s against the direct-I/O
1 MiB read's 1721. At `ra=128` it runs at 53% of it.

Filling in the curve at `bs=1M`, separate interleaved pass, medians of three:

| read_ahead_kb | 128 | 256 | 512 | 1024 | 2048 |
|---|---|---|---|---|---|
| MB/s | 871 | 1152 | 1447 | 1698 | 1704 |

Monotonic to 1024, flat after.

## Why it flattens at 1024

`strace -c -f -e trace=read` on `lbfs-client` across one 512 MiB `dd`, counting
requests pulled off `/dev/fuse`:

| read_ahead_kb | `read()` calls | implied request size |
|---|---|---|
| 128 | 4102 | 128 KiB — 4096 requests for 512 MiB |
| 1024 | 517 | 1 MiB — 512 requests |
| 4096 | 517 | 1 MiB — the identical stream |

`ra=4096` produces byte-for-byte the same request stream as `ra=1024`, because
`fc->max_pages` (256 pages, 1 MiB, the negotiated `max_io_size`) caps a single
FUSE request at 1 MiB whatever the readahead window says. The larger window
only queues more 1 MiB requests ahead — in-flight requests rose from 2 to 8 —
and that depth buys nothing on this shape.

**The right default ties to `max_io_size` rather than to a literal.**
The win arrives when the readahead window reaches the FUSE request ceiling, and it stops
there.

One measurement worth keeping beside this: `fadvise_hint=1`, which is fio's
default and issues `POSIX_FADV_SEQUENTIAL`, doubles `ra_pages` for the file and
lifts a stock mount to 1248 MB/s median instead of 871. Applications that call
`posix_fadvise` already collect part of this win. Most do not.

## The cost

**Wire bytes: none, and slightly negative.** Client NIC `rx_bytes` against bytes
the application consumed:

| shape | ra=128 | ra=1024 | ra=4096 |
|---|---|---|---|
| buffered seq read | 1.0017× | 1.0010× | 1.0010× |
| buffered randread 4k | 0.991× | 0.993× | 0.993× |
| buffered randread 128k | 0.885× | 0.901× | 0.909× |

Fewer and larger frames mean fewer TCP headers, so the raised setting moves
about 330 KB *less* per 512 MiB. The random rows sit under 1.0 because
`--norandommap` re-reads some offsets; what matters is that they stay flat
across settings. The kernel's readahead heuristic detects random access and
declines to over-fetch, and the random throughput rows agree: 8376 / 8008 /
8103 IOPS at 4k and 4719 / 4376 / 4511 IOPS at 128k, all inside spread.

**Memory: about 6 MB.** `lbfs-client` RSS, fresh mount then after one 512 MiB
buffered read: 5204 → 5880 kB at ra=128, 5400 → 11692 kB at ra=1024, and
5348 → 21908 kB at ra=4096. Reply buffers that more concurrent requests touch.

**Tail latency is the real cost, and the argument against 4096.** p99
completion latency at `bs=128k` runs 338 µs at ra=128, 913 µs at 1024 and
2089 µs at 4096; at `bs=1M`, 1565 / 1401 / 3752 µs. A small read that misses
readahead waits behind a larger fetch. At 1024 the tail stays reasonable; at
4096 it inflates two to three times and buys no throughput.

## No-regression checks

Buffered sequential write, `bs=1M`, `end_fsync=1`, same interleaving: 79, 82 and
82 MB/s medians at 128, 1024 and 4096. Flat. The absolute figure is low because
`end_fsync` drives the whole 512 MiB through the server's writeback path, the
regime this pair already documents; all three columns sit in it equally.

`direct=1` sequential read: flat within 0.3%, as above.

## What to do about it

The client cannot fix this alone, and the reason is a permission rather than an
API gap:

- The mount's bdi name comes straight out of `/proc/self/mountinfo` field 3
  (`0:46` on this run), which matches the `/sys/class/bdi/` directory name. The
  client can find its own bdi with no `stat()` on the mountpoint, and so no
  FUSE round trip into itself.
- `/sys/class/bdi/<dev>/read_ahead_kb` is `root:root` mode 644, and
  `lbfs-client` runs as uid 1000 on this pair. Writing it returns `EACCES`.

The recommendation is both halves, best effort: a `--readahead-kb` flag
defaulting to `max_io_size / 1024`, written after `spawn_mount` returns, logged
at INFO on success; on `EACCES` a single WARN naming the exact command an
operator needs, and the mount carries on. Throughput is not correctness, and
this must never turn a working mount into a failed one. The knob resets on every
mount, so unprivileged deployments need it documented as an operator step.

`set_max_readahead(max_io)` in the client stays put. The call is not wrong — it
cannot win, because the kernel takes the smaller of the INIT reply's
`max_readahead` and the bdi's existing 128 KiB. The comment above it already
says the kernel reports its own ceiling; it now has a number behind it.

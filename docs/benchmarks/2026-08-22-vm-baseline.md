# lbfs VM baseline, 22 August 2026

A three-way comparison on the libvirt guest pair: the network ceiling from
iperf3, the server's backing disk from fio, and lbfs through the FUSE mount
from the same fio jobs.

Commit under test: `e6ebfaf`.

## Environment

### Host

| Item | Value |
| --- | --- |
| CPU | AMD Ryzen 9 5900XT, 16 cores, 32 threads |
| RAM | 62 GB |
| Kernel | `7.0.12-101.fc43.x86_64` (Fedora 43) |
| Guest images | `~/.local/share/libvirt/images/lbfs`, ext4 on LVM on NVMe (WD SN850 1 TB) |
| Guest disks | qcow2 on virtio-blk, libvirt default cache mode |

The host also runs the operator's desktop. Both guests share its CPUs and its
page cache. Every number below moves from run to run. Read each one as a band
of roughly ten percent, not as a constant.

### Guests

| Item | lbfs-server | lbfs-client |
| --- | --- | --- |
| Address | 192.168.77.10 | 192.168.77.11 |
| OS | Ubuntu 26.04 LTS | Ubuntu 26.04 LTS |
| Kernel | `7.0.0-28-generic` | `7.0.0-28-generic` |
| vCPU | 2 | 2 |
| RAM | 1962 MB | 1962 MB |
| NIC | virtio-net, MTU 1500 | virtio-net, MTU 1500 |
| Disk | 20 GB qcow2, ext4 root | 20 GB qcow2, ext4 root |

TSO, GSO and GRO stay on for both guests. The pair sits on the `lbfs-net`
libvirt NAT bridge.

### lbfs setup

| Item | Value |
| --- | --- |
| Server unit | `lbfs-server.service` on `0.0.0.0:9423` |
| Export | `/srv/exports/data` |
| fsync policy | `honor` |
| Client | `/usr/local/bin/lbfs-client`, mount `/mnt/lbfs` |
| Wire | one TCP connection, `TCP_NODELAY`, 128-request window |
| I/O ceiling | `max_io` 1 MiB; the mount reports `max_read=1048576` |
| Kernel cache | FUSE writeback cache on |

Tool versions: fio 3.41 and iperf3 3.20 on both guests.

## Method

Every command below ran over ssh through `vm_ssh` from `vm/lib.sh`. The mount
came up with the `mount_export` pattern from `vm/test.sh`: `nohup` the client,
then poll `/proc/mounts`.

### Round-trip time

```
ping -c 20 -i 0.2 192.168.77.10      # idle path
sudo ping -f -c 2000 192.168.77.10   # hot path
```

### Network ceiling

```
# on lbfs-server
iperf3 -s -D --logfile /tmp/iperf3d.log

# on lbfs-client, 10 s each
iperf3 -c 192.168.77.10 -t 10 -J
iperf3 -c 192.168.77.10 -t 10 -J -R
iperf3 -c 192.168.77.10 -t 10 -J -P 8
iperf3 -c 192.168.77.10 -t 10 -J -P 8 -R
iperf3 -c 192.168.77.10 -t 10 -J -w 4M
iperf3 -c 192.168.77.10 -t 10 -J -w 4M -R
```

### The five fio jobs

One job file shape, five runs, run first on the server in
`/srv/exports/data/bench` and then on the client in `/mnt/lbfs/bench`. Shared
flags:

```
--size=512M --runtime=15 --time_based --direct=1 --numjobs=1 \
--group_reporting --output-format=json
```

Per-job flags:

| Job | Flags |
| --- | --- |
| `seqwrite` | `--rw=write --bs=1M --ioengine=psync --iodepth=1` |
| `seqread` | `--rw=read --bs=1M --ioengine=psync --iodepth=1` |
| `randread4k` | `--rw=randread --bs=4k --ioengine=psync --iodepth=1` |
| `randwrite4k` | `--rw=randwrite --bs=4k --ioengine=psync --iodepth=1` |
| `randread4k-qd16` | `--rw=randread --bs=4k --ioengine=libaio --iodepth=16` |

Each run wrote its own backing file. `jq` pulled `bw_bytes`, `iops`,
`clat_ns.mean` and `clat_ns.percentile."99.000000"` out of the JSON.

### Cache-effect runs, client side only

```
# writeback-cache number
fio ... --rw=write --bs=1M --ioengine=psync --direct=0

# server cache hot, straight after the direct seq read above
fio ... --rw=read --bs=1M --ioengine=psync --direct=1

# server cache cold
vm_ssh $SERVER_IP 'sudo sh -c "echo 3 > /proc/sys/vm/drop_caches"'
fio ... --rw=read --bs=1M --ioengine=psync --direct=1
```

## Results

### Round-trip time

| Probe | min | p50 | max | mean |
| --- | --- | --- | --- | --- |
| `ping -c 20 -i 0.2` | 0.162 ms | 0.180 ms | 0.252 ms | 0.186 ms |
| `ping -f -c 2000` | 0.028 ms | — | 0.178 ms | 0.031 ms |

The two probes differ by six times. A 200 ms gap between packets lets both
vCPUs drop into an idle state, and the wakeup dominates the sample. The flood
figure of 31 µs is the right floor to compare against a busy fio loop.

### iperf3, 10 s per run

| Direction | Mode | Gbit/s | Retrans | Client CPU | Server CPU |
| --- | --- | --- | --- | --- | --- |
| client to server | 1 stream | 36.58 | 3 | 76.3% | 63.2% |
| server to client | 1 stream, `-R` | 33.71 | 0 | 89.9% | 67.6% |
| client to server | 8 streams | 28.98 | 1 | 104.1% | 65.1% |
| server to client | 8 streams, `-R` | 30.72 | 2 | 71.7% | 99.1% |
| client to server | 1 stream, `-w 4M` | 39.77 | 0 | 76.2% | 67.4% |
| server to client | 1 stream, `-w 4M`, `-R` | 43.75 | 0 | 97.6% | 59.2% |

Almost every CPU cycle lands in the kernel: the sender reports 0.9% user
against 75.4% system. Eight parallel streams run *slower* than one, which
places the link squarely in CPU-bound territory on 2 vCPU. The `-w 4M` runs
gain 9% and 30%, so the single flow sits a little under a window-limited
ceiling, but not far.

### fio, local disk against lbfs

Both columns come from the same five jobs with `direct=1`.

| Job | Local MB/s | lbfs MB/s | Local IOPS | lbfs IOPS | Local mean µs | lbfs mean µs | Local p99 µs | lbfs p99 µs |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| seq write 1M psync | 5057.2 | 1113.1 | 4823 | 1062 | 201.5 | 935.7 | 301.1 | 1990.7 |
| seq read 1M psync | 6500.0 | 1626.0 | 6199 | 1551 | 161.0 | 643.8 | 250.9 | 1036.3 |
| randread 4k psync QD1 | 103.7 | 33.8 | 25324 | 8263 | 39.2 | 120.1 | 53.5 | 156.7 |
| randwrite 4k psync QD1 | 90.8 | 14.2 | 22169 | 3463 | 44.7 | 287.4 | 82.4 | 415.7 |
| randread 4k libaio QD16 | 800.6 | 29.6 | 195467 | 7217 | 77.7 | 2078.8 | 130.6 | 2637.8 |

lbfs as a fraction of the local disk:

| Job | lbfs / local |
| --- | --- |
| seq write 1M | 22.0% |
| seq read 1M | 25.0% |
| randread 4k QD1 | 32.6% |
| randwrite 4k QD1 | 15.6% |
| randread 4k QD16 | 3.7% |

### Cache effects through the mount

| Run | MB/s | mean µs | p99 µs |
| --- | --- | --- | --- |
| seq write 1M, `direct=0` (client writeback cache) | 609.9 | 1598.3 | 6979.6 |
| seq read 1M, `direct=1`, server cache hot | 1646.6 | 635.6 | 1417.2 |
| seq read 1M, `direct=1`, server cache dropped | 1811.3 | 577.8 | 938.0 |

### Concurrency probes

Three extra client-side runs, outside the five-job matrix, to explain the QD16
result. All use `direct=1` and 4 fio threads.

| Probe | IOPS | MB/s | mean µs |
| --- | --- | --- | --- |
| randread 4k, psync, 4 threads, 4 files | 22379 | 91.7 | 177.2 |
| randread 4k, psync, 4 threads, 1 shared file | 20594 | 84.4 | 192.7 |
| randwrite 4k, psync, 4 threads, 4 files | 9469 | 38.8 | 420.6 |

## Analysis

### Sequential throughput against both ceilings

| Direction | lbfs | As Gbit/s | Share of 1-stream iperf3 | Share of local disk |
| --- | --- | --- | --- | --- |
| write (client to server) | 1113.1 MB/s | 8.90 | 24.3% of 36.58 | 22.0% of 5057.2 |
| read (server to client) | 1626.0 MB/s | 13.01 | 38.6% of 33.71 | 25.0% of 6500.0 |

Neither the link nor the disk bounds either direction. Both sit near a quarter
of each ceiling, and the two ceilings happen to land close together on this
host. What bounds them is CPU and per-request software cost on 2 vCPU. iperf3
already burns 76% to 90% of a guest CPU to move bytes with zero per-4k-block
work; lbfs adds FUSE entry and exit, a copy into the request buffer, protocol
framing, and a server-side `pread`/`pwrite` on top of that. The 8-stream
iperf3 runs finishing *below* the single-stream runs is the same story from
the other side: this pair has no spare CPU to hand to more concurrency.

Read beats write by 46% through the mount, which tracks the iperf3 asymmetry
only weakly (`-R` was 8% *slower* than forward). The gap comes from the write
path, not the wire.

### Per-operation cost at 4k QD1

| Metric | randread 4k | randwrite 4k |
| --- | --- | --- |
| local time per op | 39.5 µs | 45.1 µs |
| lbfs time per op | 121.0 µs | 288.8 µs |
| lbfs minus local | 81.5 µs | 243.7 µs |
| against the 31 µs hot RTT | 2.6× | 7.9× |
| against the 180 µs idle p50 | 0.45× | 1.35× |

Reads land within 2.6 round trips of the floor. Subtract one RTT for the wire
and about 50 µs remains for FUSE dispatch, the tokio hop, framing, and the
server's `pread`. For a userspace filesystem crossing a real socket, that is
close to the one-RTT-per-op floor.

Writes cost three times what reads cost, and that gap is the one open question
in this baseline. `fsync = honor` does not explain it, because these jobs never
call `fsync`. Two candidates stand out. First, `direct=1` on a mount holding
`FUSE_WRITEBACK_CACHE` makes the kernel flush the page-cache range for the file
and then drop it, under the inode lock, before each direct write. Second, the
server strips `O_DIRECT` and lands in its own page cache, where a 4k write into
a 512 MB file may pull in the surrounding page first. The 4-thread probe rules
out hard serialization: writes scale 2.7× from 3463 to 9469 IOPS, the same
factor reads scale by.

### The QD16 collapse

`libaio` at `iodepth=16` returns 7217 IOPS against 8263 at QD1, with mean
latency up 17× to 2079 µs. Throughput flat and latency linear in queue depth
is the signature of a queue that never fills. The client's capability list in
`crates/lbfs-client/src/fuse.rs` asks for `FUSE_DO_READDIRPLUS`,
`FUSE_READDIRPLUS_AUTO` and `FUSE_WRITEBACK_CACHE`, and `fuser` adds
`FUSE_ASYNC_READ`, `FUSE_BIG_WRITES` and `FUSE_MAX_PAGES` by default.
`FUSE_ASYNC_DIO` appears in neither set. Without it the kernel's
`fuse_direct_IO` completes each request inline, so an async submission blocks
the submitting thread and `iodepth` stops meaning anything.

The probes confirm the diagnosis. Four psync threads reach 22379 IOPS, 2.7×
the single-thread figure, and a single shared file gives 20594 — within noise
of the four-file case, so no per-inode lock is throttling reads either. The
protocol pipeline and the 128-request window work fine. Only the `libaio`
path cannot reach them. Adding `FUSE_ASYNC_DIO` to the capability list is the
obvious next experiment.

### Numbers that caching flatters

Four warnings for anyone reading these figures as storage numbers.

1. **The local disk column is host RAM, not NVMe.** `direct=1` in the guest
   skips the guest page cache. The qcow2 file behind `vda` uses the libvirt
   default cache mode, so the host absorbs it all. 5.1 GB/s of "disk" write and
   195k IOPS at QD16 are host page-cache speeds on a 62 GB host holding a
   512 MB working set. Real NVMe would land lower. The lbfs-to-local ratios
   above understate lbfs against a disk-bound server.

2. **`drop_caches` in the server guest does not reach the host.** The cold read
   run at 1811 MB/s beat the hot run at 1647 MB/s. Both still read from host
   RAM; the guest-cold run merely started with more free guest memory. This
   pair cannot produce a disk-cold read, and the 10% spread between the two
   runs measures host CPU noise, not cache state.

3. **The buffered write number is a memory measurement, and a slow one.**
   609.9 MB/s with `direct=0` and no `end_fsync` reports how fast the client
   filled dirty pages, not how fast bytes reached the server. It comes in
   *below* the 1113 MB/s direct write because a 512 MB file against 1962 MB of
   guest RAM hits dirty-ratio throttling, and the extra copy through the page
   cache costs CPU the guest cannot spare. The p99 of 6980 µs against 1991 µs
   direct is the throttle showing up. Use `end_fsync=1`, as `vm/tests/fio.sh`
   does, for a writeback number that means anything.

4. **512 MB against 1962 MB of guest RAM is a small working set.** Both guests
   can cache a large fraction of it. `direct=1` keeps the client honest, but
   the server has no such protection: production behaviour, since the server
   strips `O_DIRECT` by design, and worth stating so nobody reads these as
   cold-cache figures.

## Reproduction notes

- Both guests must stay up. Their live domain XML carries
  `on_reboot=destroy`, so a reboot inside a guest powers the domain off.
  Recover with `virsh --connect qemu:///system start <name>`.
- `iperf3` ships with neither guest image. Install it with
  `sudo apt-get install -y iperf3` on both, and kill the daemon afterwards.
- Run the local fio pass before mounting, or point it at a directory the mount
  pass does not share. `/srv/exports/data/bench` and `/mnt/lbfs/bench` are the
  same directory seen from two sides.
- Lay-out cost is real. Five 512 MB files through the mount take a few minutes
  before the first timed second starts.
- Leave the export empty afterwards: `sudo rm -rf /srv/exports/data/bench` on
  the server, after `fusermount3 -u /mnt/lbfs` on the client.
- Expect run-to-run drift on a shared host. Repeat any figure that drives a
  decision, and prefer the ratio between two runs taken minutes apart over the
  absolute value of either.

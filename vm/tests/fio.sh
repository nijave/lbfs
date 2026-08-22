#!/usr/bin/env bash
#
# Integrity and throughput, run on the client VM inside the mount.
#
# The integrity half is the one that can fail interestingly: fio's crc32c verify
# writes tagged blocks in random order and reads every one of them back, so a
# reply matched to the wrong request, a short write, or an off-by-one offset
# shows up as a checksum mismatch rather than as data nobody looks at again.
#
# The throughput half is a sanity line, not a benchmark. Two guests on one host
# share the same CPUs and a virtio link, so the number says "the data path is
# not accidentally serialised", and the floor is set low enough that it only
# fires when something is actually wrong.
set -euo pipefail

MNT="${1:?usage: fio.sh <mountpoint> <verify-job> [floor-MB/s]}"
JOB="${2:?usage: fio.sh <mountpoint> <verify-job> [floor-MB/s]}"
FLOOR="${3:-20}"
SIZE="${LBFS_FIO_SIZE:-256M}"
OUT=/tmp/lbfs-fio.json

ok() { printf '  ok    %s\n' "$1"; }

die() {
  printf '  FAIL  %s\n' "$1" >&2
  exit 1
}

grep -q " $MNT fuse" /proc/mounts || die "$MNT is not a fuse mount"

# Every read below has to come off the wire. Without this the client's page
# cache answers most of it and the number measures memcpy.
drop_caches() {
  sync
  sudo sysctl -q vm.drop_caches=3
}

# fio's JSON reports bandwidth in bytes per second; everything printed here is
# decimal MB/s so it lines up with what dd prints.
mbps() {
  awk -v b="$1" 'BEGIN { printf "%.1f", b / 1000000 }'
}

# Both an exit status and the per-job error field: fio can complete a job that
# hit an I/O error and still exit zero if the error was on the verify path in
# some configurations, and the field is the unambiguous one.
run_fio() {
  if ! fio --output-format=json --output="$OUT" "$@" >/dev/null 2>/tmp/lbfs-fio.err; then
    cat /tmp/lbfs-fio.err >&2
    if [ -s "$OUT" ]; then
      jq -r '.jobs[]? | "job \(.jobname) error \(.error)"' "$OUT" >&2
    fi
    return 1
  fi
  local err
  err="$(jq -r '[.jobs[].error] | add' "$OUT")"
  if [ "$err" != 0 ]; then
    jq -r '.jobs[] | "job \(.jobname) error \(.error)"' "$OUT" >&2
    return 1
  fi
}

# --- integrity --------------------------------------------------------------

if ! run_fio --directory="$MNT" "$JOB"; then
  die 'fio crc32c verify reported an error'
fi
verified="$(jq -r '.jobs[0].read.io_bytes' "$OUT")"
ok "fio crc32c verify re-read and checked $((verified / 1048576)) MiB with no mismatch"
rm -f "$MNT"/verify-rw.*

# --- sequential write -------------------------------------------------------

# end_fsync so the number covers getting the bytes to the server rather than
# into the client's dirty page cache; without it a writeback mount reports the
# speed of memory.
if ! run_fio --name=seq --directory="$MNT" --rw=write --bs=1M --size="$SIZE" \
  --ioengine=psync --direct=0 --numjobs=1 --end_fsync=1 --group_reporting; then
  die 'the sequential write job failed'
fi
write_bps="$(jq -r '.jobs[0].write.bw_bytes' "$OUT")"
write_mbps="$(mbps "$write_bps")"

# --- sequential read --------------------------------------------------------

drop_caches
if ! run_fio --name=seq --directory="$MNT" --rw=read --bs=1M --size="$SIZE" \
  --ioengine=psync --direct=0 --numjobs=1 --group_reporting; then
  die 'the sequential read job failed'
fi
read_bps="$(jq -r '.jobs[0].read.bw_bytes' "$OUT")"
read_mbps="$(mbps "$read_bps")"

rm -f "$MNT"/seq.*
rm -f "$OUT" /tmp/lbfs-fio.err

printf 'THROUGHPUT seq-write %s MB/s  seq-read %s MB/s  (%s, bs=1M, fsync'\''d writes)\n' \
  "$write_mbps" "$read_mbps" "$SIZE"

for pair in "write:$write_mbps" "read:$read_mbps"; do
  what="${pair%%:*}"
  got="${pair##*:}"
  if awk -v g="$got" -v f="$FLOOR" 'BEGIN { exit !(g < f) }'; then
    die "sequential $what managed $got MB/s, under the $FLOOR MB/s floor"
  fi
  ok "sequential $what $got MB/s clears the $FLOOR MB/s floor"
done

echo 'FIO OK'

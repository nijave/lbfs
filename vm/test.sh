#!/usr/bin/env bash
#
# The end-to-end suite: a real client-VM FUSE mount of the server VM's export,
# over the network, driven from here over ssh.
#
# This is the only thing in the tree that exercises the whole product at once.
# Everything below it — the unit tests, the loopback mount — runs both halves in
# one kernel and often in one process. Here the client's kernel, the client's
# FUSE bridge, a TCP connection, the server's io_uring backend and the server's
# actual filesystem are five separate things that have to agree, and each step
# is chosen for a way they could disagree.
#
# Structure: this script owns the mount lifecycle and the pass/fail reporting;
# the scripts under vm/tests/ own the workloads. Steps that need both ends —
# "the client wrote it, so the server must have it" — run one script on each
# guest and compare. The first failure aborts the run with the client log and
# the server journal, because the interesting evidence is on the guests and
# ssh'ing in afterwards is a race against the cleanup.
#
# Idempotent by construction: every run starts by tearing down whatever the last
# one left and emptying the export, and ends the same way, so `make vm-test`
# twice in a row is the same run twice.
set -euo pipefail

# shellcheck source-path=SCRIPTDIR
# shellcheck source=lib.sh
source "$(dirname "$0")/lib.sh"

TESTS="$VM_DIR/tests"
CLIENT_LOG=/tmp/lbfs-client.log
FLOOR_MBPS="${LBFS_FLOOR_MBPS:-20}"
# Where the live-node probe gives up if it never runs out of descriptors. Twice
# systemd's default RLIMIT_NOFILE, so the ceiling this pair actually has lands
# comfortably inside it.
NODE_PROBE_CAP="${LBFS_NODE_PROBE_CAP:-2048}"

STEP=0
CURRENT=""

step() {
  STEP=$((STEP + 1))
  CURRENT="$1"
  printf '\n--- step %d: %s\n' "$STEP" "$CURRENT"
}

pass() {
  printf 'PASS  step %d: %s\n' "$STEP" "$CURRENT"
  CURRENT=""
}

# Called from the EXIT trap, so it has to work on a pair in any state: a wedged
# mount, a dead server, a client that is still holding /mnt/lbfs.
reset_pair() {
  vm_ssh "$CLIENT_IP" "fusermount3 -u $CLIENT_MOUNT 2>/dev/null ||
      fusermount3 -uz $CLIENT_MOUNT 2>/dev/null || true
    pkill -x lbfs-client 2>/dev/null || true
    for _ in \$(seq 1 20); do pgrep -x lbfs-client >/dev/null || break; sleep 0.5; done
    pkill -KILL -x lbfs-client 2>/dev/null || true
    rm -f /tmp/lbfs-*.log /tmp/lbfs-*.job /tmp/lbfs-*.json /tmp/lbfs-*.err \
      /tmp/lbfs-*.txt /tmp/lbfs-*.bin /tmp/lbfs-dd-rc \
      /tmp/lbfs-e2e.sh /tmp/lbfs-build.sh /tmp/lbfs-fio.sh
    exit 0" || true
  vm_ssh "$SERVER_IP" "systemctl is-active --quiet lbfs-server ||
      sudo systemctl start lbfs-server
    find $SERVER_EXPORT -mindepth 1 -delete
    rm -f /tmp/lbfs-server-check.sh
    exit 0" || true
}

on_exit() {
  local rc=$?
  if [ "$rc" -ne 0 ]; then
    if [ -n "$CURRENT" ]; then
      printf '\nFAIL  step %d: %s (exit %d)\n' "$STEP" "$CURRENT" "$rc" >&2
    fi
    dump_context
  fi
  reset_pair
  exit "$rc"
}

dump_context() {
  printf '\n--- client %s\n' "$CLIENT_IP" >&2
  vm_ssh "$CLIENT_IP" "grep ' $CLIENT_MOUNT ' /proc/mounts || echo '(nothing mounted on $CLIENT_MOUNT)'
    pgrep -a lbfs-client || echo '(no lbfs-client running)'
    echo '--- last 40 lines of the client log'
    tail -n 40 $CLIENT_LOG 2>/dev/null || echo '(no client log)'
    exit 0" >&2 || echo '(client unreachable)' >&2
  printf '\n--- server %s\n' "$SERVER_IP" >&2
  vm_ssh "$SERVER_IP" "systemctl is-active lbfs-server
    echo '--- last 40 journal lines'
    sudo journalctl -u lbfs-server -n 40 --no-pager
    echo '--- export contents'
    find $SERVER_EXPORT -mindepth 1 | head -40
    exit 0" >&2 || echo '(server unreachable)' >&2
}

# Bring the mount up and leave it up. The client is detached from this ssh
# session on purpose: every later step is a separate connection, so a mount that
# only lived as long as one `ssh` would prove nothing about the product and the
# suite could not check the server's view while the client still has it open.
mount_export() {
  vm_ssh "$CLIENT_IP" "nohup lbfs-client $SERVER_IP:$SERVER_PORT $SERVER_EXPORT $CLIENT_MOUNT \
      >$CLIENT_LOG 2>&1 </dev/null &
    for _ in \$(seq 1 100); do
      grep -q ' $CLIENT_MOUNT fuse' /proc/mounts && exit 0
      sleep 0.2
    done
    exit 1"
}

trap on_exit EXIT

echo "lbfs e2e: client $CLIENT_IP -> server $SERVER_IP:$SERVER_PORT export $SERVER_EXPORT"

# ---------------------------------------------------------------------------

step 'preconditions: both guests reachable, server serving, client binary in place'
for vm in "${VMS[@]}"; do
  vm_exists "$vm" || {
    echo "domain $vm does not exist - run 'make vm-up'" >&2
    exit 1
  }
  state="$("${VIRSH[@]}" domstate "$vm")"
  [ "$state" = running ] || {
    echo "domain $vm is $state - run 'virsh --connect qemu:///system start $vm'" >&2
    exit 1
  }
done
vm_ssh "$SERVER_IP" true || {
  echo "no ssh to the server at $SERVER_IP" >&2
  exit 1
}
vm_ssh "$CLIENT_IP" true || {
  echo "no ssh to the client at $CLIENT_IP" >&2
  exit 1
}
vm_ssh "$SERVER_IP" "systemctl is-active --quiet lbfs-server && [ -d $SERVER_EXPORT ]" || {
  echo "lbfs-server is not serving $SERVER_EXPORT - run 'make vm-deploy'" >&2
  exit 1
}
vm_ssh "$CLIENT_IP" '[ -x /usr/local/bin/lbfs-client ]' || {
  echo "lbfs-client is not installed on $CLIENT_IP - run 'make vm-deploy'" >&2
  exit 1
}
# Whatever a previous run or a manual poke left behind, from here the pair looks
# the same every time.
reset_pair
vm_ssh "$CLIENT_IP" "[ -z \"\$(ls -A $CLIENT_MOUNT)\" ]" || {
  echo "$CLIENT_MOUNT is not empty after cleanup" >&2
  exit 1
}
pass

# ---------------------------------------------------------------------------

step 'mount the export over the network'
mount_export || {
  echo 'the client never mounted' >&2
  exit 1
}
# A second ssh session: proves the mount outlived the one that created it, which
# is the only reason the rest of the suite can be written as separate steps.
opts="$(vm_ssh "$CLIENT_IP" "grep ' $CLIENT_MOUNT ' /proc/mounts")"
echo "  $opts"
grep -q 'fuse ' <<<"$opts" || {
  echo 'the mount is not a fuse mount' >&2
  exit 1
}
grep -q 'max_read=1048576' <<<"$opts" || echo "  note: max_read is not the negotiated 1 MiB" >&2
pass

# ---------------------------------------------------------------------------

step 'POSIX workload through the mount'
vm_scp "$TESTS/e2e.sh" "ubuntu@$CLIENT_IP:/tmp/lbfs-e2e.sh" >/dev/null
vm_ssh "$CLIENT_IP" "chmod +x /tmp/lbfs-e2e.sh && /tmp/lbfs-e2e.sh $CLIENT_MOUNT"
pass

# ---------------------------------------------------------------------------

step 'the server agrees: same tree, same metadata, same bytes on its own disk'
vm_scp "$TESTS/server-check.sh" "ubuntu@$SERVER_IP:/tmp/lbfs-server-check.sh" >/dev/null
vm_ssh "$SERVER_IP" "chmod +x /tmp/lbfs-server-check.sh && /tmp/lbfs-server-check.sh $SERVER_EXPORT"
vm_ssh "$CLIENT_IP" "rm -rf $CLIENT_MOUNT/witness"
vm_ssh "$SERVER_IP" "[ ! -e $SERVER_EXPORT/witness ]" || {
  echo 'removing the witness tree through the mount did not remove it on the server' >&2
  exit 1
}
echo '  ok    removing the tree through the mount removed it on the server too'
pass

# ---------------------------------------------------------------------------

step 'build workload: compile a tree inside the mount, then churn small files'
vm_scp "$TESTS/build.sh" "ubuntu@$CLIENT_IP:/tmp/lbfs-build.sh" >/dev/null
vm_ssh "$CLIENT_IP" "chmod +x /tmp/lbfs-build.sh && /tmp/lbfs-build.sh $CLIENT_MOUNT"
pass

# ---------------------------------------------------------------------------

step 'live-node ceiling: how many files the client can hold open at the server'
# The server keeps one O_PATH descriptor per node the client still remembers
# (spec §"node table"), so its RLIMIT_NOFILE is a hard ceiling on how large a
# tree a client may have live at once. Nothing in the product raises that limit
# and nothing in the unit file sets it, so on a stock systemd service it is
# 1024, which a build tree reaches easily. Measured rather than assumed: the
# number is printed on every run, so raising the limit shows up here.
probe="$(vm_ssh "$CLIENT_IP" "cd $CLIENT_MOUNT && rm -rf nodeprobe && mkdir nodeprobe && cd nodeprobe
  n=0
  for i in \$(seq 1 $NODE_PROBE_CAP); do
    if : > \"p-\$i\" 2>/dev/null; then n=\$i; else break; fi
  done
  echo \$n")"
# Recovery is the client forgetting the nodes, not the server reclaiming them.
# It has to happen before the rm: at the ceiling the server cannot even open the
# directory to read it, so `rm -r` fails too.
vm_ssh "$CLIENT_IP" "sync; sudo sysctl -q vm.drop_caches=3"
vm_ssh "$CLIENT_IP" "rm -rf $CLIENT_MOUNT/nodeprobe"
if [ "$probe" -lt "$NODE_PROBE_CAP" ]; then
  printf '  NOTE  the server refused the %dth simultaneously live node (EMFILE)\n' \
    "$((probe + 1))"
  printf '  NOTE  one O_PATH fd per live node, and neither lbfs-server nor its unit\n'
  printf '  NOTE  raises RLIMIT_NOFILE, so a tree this size wedges the export until\n'
  printf '  NOTE  the client drops its inode cache or unmounts\n'
else
  printf '  ok    %d simultaneously live nodes with no ceiling reached\n' "$probe"
fi
[ "$probe" -ge 256 ] || {
  echo "the server ran out of descriptors after only $probe live nodes" >&2
  exit 1
}
echo "  ok    the export is usable again after the client dropped its inode cache"
vm_ssh "$CLIENT_IP" "echo probe > $CLIENT_MOUNT/recheck &&
  [ \"\$(cat $CLIENT_MOUNT/recheck)\" = probe ] && rm -f $CLIENT_MOUNT/recheck"
pass

# ---------------------------------------------------------------------------

step 'integrity and throughput'
vm_scp "$TESTS/fio.sh" "ubuntu@$CLIENT_IP:/tmp/lbfs-fio.sh" >/dev/null
vm_scp "$TESTS/fio-verify.job" "ubuntu@$CLIENT_IP:/tmp/lbfs-verify.job" >/dev/null
vm_ssh "$CLIENT_IP" \
  "chmod +x /tmp/lbfs-fio.sh && /tmp/lbfs-fio.sh $CLIENT_MOUNT /tmp/lbfs-verify.job $FLOOR_MBPS"
pass

# ---------------------------------------------------------------------------

step 'fsync durability: the bytes are on the server before fsync returns'
# The mount runs with the writeback cache on, so an unsynced write is still in
# the client's page cache and the server has never heard of it. That is what
# makes this a test: the checksum is compared while the mount is still up, with
# nothing but the fsync to have pushed the data across.
digest="$(vm_ssh "$CLIENT_IP" "head -c 4194304 /dev/urandom > /tmp/lbfs-payload.bin &&
  dd if=/tmp/lbfs-payload.bin of=$CLIENT_MOUNT/fsynced.bin bs=1M conv=fsync status=none &&
  md5sum /tmp/lbfs-payload.bin | cut -d' ' -f1")"
server_digest="$(vm_ssh "$SERVER_IP" "md5sum $SERVER_EXPORT/fsynced.bin | cut -d' ' -f1")"
[ "$digest" = "$server_digest" ] || {
  echo "the server's copy hashes $server_digest, the client wrote $digest" >&2
  exit 1
}
echo "  ok    4 MiB fsync'd through the mount matches on the server ($server_digest)"
vm_ssh "$CLIENT_IP" "rm -f $CLIENT_MOUNT/fsynced.bin /tmp/lbfs-payload.bin"
pass

# ---------------------------------------------------------------------------

step 'unmount cleanliness'
vm_ssh "$CLIENT_IP" "fusermount3 -u $CLIENT_MOUNT"
vm_ssh "$CLIENT_IP" "for _ in \$(seq 1 60); do pgrep -x lbfs-client >/dev/null || exit 0; sleep 0.5; done
  echo 'lbfs-client is still running 30s after the unmount' >&2
  exit 1"
vm_ssh "$CLIENT_IP" "! grep -q ' $CLIENT_MOUNT ' /proc/mounts" || {
  echo "something is still mounted on $CLIENT_MOUNT" >&2
  exit 1
}
vm_ssh "$CLIENT_IP" "[ -z \"\$(ls -A $CLIENT_MOUNT)\" ]" || {
  echo "$CLIENT_MOUNT is not empty after the unmount" >&2
  exit 1
}
echo '  ok    fusermount3 -u unmounted, the client exited, the mountpoint is bare'
pass

# ---------------------------------------------------------------------------

step 'attach denial: a path the server does not export'
# /etc exists and is a directory, so the server has to refuse it on the
# allowlist rather than on a failed open — which is the distinct status the
# handshake carries, and the message the CLI is expected to print (spec §8).
denied="$(vm_ssh "$CLIENT_IP" "mkdir -p /tmp/lbfs-deny &&
  lbfs-client $SERVER_IP:$SERVER_PORT /etc /tmp/lbfs-deny 2>&1; echo rc=\$?")"
echo "  $denied"
grep -q 'refused access to that export' <<<"$denied" || {
  echo 'the client did not report the attach denial distinctly' >&2
  exit 1
}
grep -q 'rc=1' <<<"$denied" || {
  echo 'the client exited zero on a refused attach' >&2
  exit 1
}
vm_ssh "$CLIENT_IP" "! grep -q ' /tmp/lbfs-deny ' /proc/mounts && rmdir /tmp/lbfs-deny" || {
  echo 'a refused attach left a mount behind' >&2
  exit 1
}
# A path that does not exist at all is the other status, and the messages have
# to differ or the operator cannot tell "fix the path" from "fix the allowlist".
missing="$(vm_ssh "$CLIENT_IP" "mkdir -p /tmp/lbfs-deny &&
  lbfs-client $SERVER_IP:$SERVER_PORT /srv/exports/nonexistent /tmp/lbfs-deny 2>&1; echo rc=\$?")"
echo "  $missing"
grep -q 'rc=1' <<<"$missing" || {
  echo 'the client exited zero attaching a path that does not exist' >&2
  exit 1
}
[ "$denied" != "$missing" ] || {
  echo 'a denied export and a missing export produce the same message' >&2
  exit 1
}
vm_ssh "$CLIENT_IP" 'rmdir /tmp/lbfs-deny'
pass

# ---------------------------------------------------------------------------

step 'disconnect drill: kill the server mid-write'
"$TESTS/disconnect.sh"
pass

# ---------------------------------------------------------------------------

step 'the pair is left clean'
reset_pair
vm_ssh "$SERVER_IP" "[ -z \"\$(ls -A $SERVER_EXPORT)\" ]" || {
  echo "$SERVER_EXPORT is not empty" >&2
  exit 1
}
vm_ssh "$CLIENT_IP" "! grep -q ' $CLIENT_MOUNT ' /proc/mounts" || {
  echo "something is still mounted on $CLIENT_MOUNT" >&2
  exit 1
}
vm_ssh "$CLIENT_IP" '! pgrep -x lbfs-client >/dev/null' || {
  echo 'an lbfs-client is still running' >&2
  exit 1
}
vm_ssh "$SERVER_IP" 'systemctl is-active --quiet lbfs-server' || {
  echo 'lbfs-server is not running' >&2
  exit 1
}
echo '  ok    export empty, nothing mounted, no client running, server up'
pass

printf '\nALL VM TESTS PASSED (%d steps)\n' "$STEP"

#!/usr/bin/env bash
#
# The disconnect drill, driven from the host: kill the server underneath a
# mount that is in the middle of writing, and check that every promise the
# design makes about that moment holds.
#
# Reconnection is explicitly not a v1 feature. What is promised instead is that
# the failure is loud and local: in-flight I/O fails rather than hanging or
# silently losing data, the mount stays present and answers EIO instead of
# wedging the process that touched it, the client does not exit on its own, and
# a plain `fusermount3 -u` still tears it down. Then a fresh mount works, which
# is the actual recovery story.
#
# Self-contained on purpose. It owns its mount from start to finish so it can be
# run on its own after a failure, and it leaves the server running and the
# export empty whichever way it goes.
set -euo pipefail

# shellcheck source-path=SCRIPTDIR/..
# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

BIG=disconnect-big
RC_FILE=/tmp/lbfs-dd-rc
# Large enough that a virtio link doing ~300 MB/s cannot finish it inside the
# handful of seconds this takes to notice the write and stop the server. Only a
# fraction of it ever lands: the server dies a second or so in.
COUNT="${LBFS_DISCONNECT_MB:-4096}"
# How much has to have crossed before the interruption counts as mid-I/O.
INFLIGHT_BYTES="${LBFS_DISCONNECT_INFLIGHT:-33554432}"

ok() { printf '  ok    %s\n' "$1"; }

die() {
  printf '  FAIL  %s\n' "$1" >&2
  exit 1
}

# Whatever happens, the server comes back and the mount goes away. A drill that
# leaves the pair broken would make every later step lie about why it failed.
restore() {
  local rc=$?
  vm_ssh "$CLIENT_IP" "fusermount3 -u $CLIENT_MOUNT 2>/dev/null || fusermount3 -uz $CLIENT_MOUNT 2>/dev/null || true
    pkill -x lbfs-client 2>/dev/null || true
    rm -f $RC_FILE
    exit 0" || true
  vm_ssh "$SERVER_IP" \
    'systemctl is-active --quiet lbfs-server || sudo systemctl start lbfs-server' || true
  return "$rc"
}
trap restore EXIT

mount_client() {
  vm_ssh "$CLIENT_IP" "nohup lbfs-client $SERVER_IP:$SERVER_PORT $SERVER_EXPORT $CLIENT_MOUNT \
      >/tmp/lbfs-disconnect.log 2>&1 </dev/null &
    for _ in \$(seq 1 50); do
      grep -q ' $CLIENT_MOUNT fuse' /proc/mounts && exit 0
      sleep 0.2
    done
    cat /tmp/lbfs-disconnect.log >&2
    exit 1"
}

mount_client || die 'the client did not mount'
ok 'mounted'

# Backgrounded with its stdio detached, because otherwise ssh waits for the
# write to finish and there is no "mid-I/O" left to interrupt. conv=fsync so the
# rc covers the flush and not just the page cache.
vm_ssh "$CLIENT_IP" "rm -f $RC_FILE
  nohup sh -c 'dd if=/dev/zero of=$CLIENT_MOUNT/$BIG bs=1M count=$COUNT conv=fsync status=none;
    echo \$? > $RC_FILE' >/dev/null 2>&1 </dev/null &
  exit 0"

# "Mid-I/O" measured, not slept for. A fixed pause races both ways: too short
# and the write has not reached the wire, too long and a fast link has already
# finished it. The server watching its own file grow is the unambiguous signal
# that bytes are crossing right now, which is the moment worth interrupting.
deadline=$((SECONDS + 60))
until [ "$(vm_ssh "$SERVER_IP" "stat -c%s $SERVER_EXPORT/$BIG 2>/dev/null || echo 0")" \
  -ge "$INFLIGHT_BYTES" ]; do
  if ! vm_ssh "$CLIENT_IP" "[ ! -f $RC_FILE ]"; then
    die "the ${COUNT}MiB write finished before the server could be stopped; raise LBFS_DISCONNECT_MB"
  fi
  if [ "$SECONDS" -ge "$deadline" ]; then
    die "the write never reached $INFLIGHT_BYTES bytes on the server"
  fi
  sleep 0.2
done
ok "a ${COUNT}MiB write is in flight and the server has taken $INFLIGHT_BYTES bytes of it"

vm_ssh "$SERVER_IP" 'sudo systemctl stop lbfs-server'
ok 'stopped lbfs-server underneath the mount'

# Poll rather than sleep a fixed window: the failure should be prompt, and if it
# is not, the timeout is the interesting number.
deadline=$((SECONDS + 60))
until vm_ssh "$CLIENT_IP" "[ -f $RC_FILE ]"; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    die 'the write neither completed nor failed 60s after the server died'
  fi
  sleep 1
done
dd_rc="$(vm_ssh "$CLIENT_IP" "cat $RC_FILE")"
if [ "$dd_rc" = 0 ]; then
  die 'the write reported success after the server was stopped'
fi
ok "the in-flight write failed (dd exit $dd_rc) instead of hanging or lying"

# EIO, not ENOTCONN and not a hang: the mount is still there, it just cannot
# answer. `timeout` is the assertion that it does not block.
errs="$(vm_ssh "$CLIENT_IP" "timeout 20 ls $CLIENT_MOUNT 2>&1 >/dev/null || true
  timeout 20 sh -c 'echo x > $CLIENT_MOUNT/after-death' 2>&1 || true
  timeout 20 stat $CLIENT_MOUNT/$BIG 2>&1 >/dev/null || true")"
if ! grep -qi 'input/output error' <<<"$errs"; then
  die "operations on the dead mount did not report EIO; got: $errs"
fi
ok 'reads, writes and stats on the dead mount all report EIO'

if ! vm_ssh "$CLIENT_IP" "grep -q ' $CLIENT_MOUNT fuse' /proc/mounts"; then
  die 'the mount disappeared on its own; it is supposed to stay and answer EIO'
fi
if ! vm_ssh "$CLIENT_IP" 'pgrep -x lbfs-client >/dev/null'; then
  die 'lbfs-client exited on connection loss; it is supposed to wait for an unmount'
fi
ok 'the mount and the client are still there, as spec §7 requires'

if ! vm_ssh "$CLIENT_IP" "timeout 30 fusermount3 -u $CLIENT_MOUNT"; then
  die 'fusermount3 -u could not unmount the dead mount'
fi
deadline=$((SECONDS + 30))
while vm_ssh "$CLIENT_IP" 'pgrep -x lbfs-client >/dev/null'; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    die 'lbfs-client did not exit 30s after the unmount'
  fi
  sleep 1
done
ok 'unmounted cleanly and the client exited'

vm_ssh "$SERVER_IP" 'sudo systemctl start lbfs-server'
deadline=$((SECONDS + 30))
until vm_ssh "$SERVER_IP" 'systemctl is-active --quiet lbfs-server'; do
  if [ "$SECONDS" -ge "$deadline" ]; then die 'lbfs-server did not come back'; fi
  sleep 1
done
ok 'restarted lbfs-server'

mount_client || die 'the client could not mount again after the server came back'
if ! vm_ssh "$CLIENT_IP" "echo reconnected > $CLIENT_MOUNT/probe &&
  [ \"\$(cat $CLIENT_MOUNT/probe)\" = reconnected ] &&
  rm -f $CLIENT_MOUNT/probe $CLIENT_MOUNT/$BIG $CLIENT_MOUNT/after-death"; then
  die 'the fresh mount does not work'
fi
ok 'a fresh mount over the restarted server reads and writes normally'

vm_ssh "$CLIENT_IP" "fusermount3 -u $CLIENT_MOUNT"
ok 'unmounted the fresh mount'

echo 'DISCONNECT OK'

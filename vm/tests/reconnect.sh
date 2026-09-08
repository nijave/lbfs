#!/usr/bin/env bash
#
# The reconnect drill, driven from the host: kill the TCP connection underneath
# a mount that is in the middle of writing, leave the server running, and check
# that the transport failure costs latency rather than the mount.
#
# The complement of disconnect.sh. That drill stops the server, which empties
# the session registry, so the mount dies exactly as spec §7 promises. Here the
# server keeps the session — node table, open handles, directory snapshots —
# and the client claims it back with the ticket ATTACH handed it: a write in
# flight at the break may fail (the design fails it on purpose), a fresh write
# parks for the reconnect and then lands, a descriptor opened before the break
# still reads and writes through it, and the server's journal carries the claim
# line and, after the unmount, the detach line. Those two lines are the drill's
# only window into the registry; nothing else exposes a session count.
#
# Self-contained on purpose, like its sibling: it owns its mount from start to
# finish so it can be run on its own after a failure, and it leaves the server
# running and its scratch files gone whichever way it goes.
set -euo pipefail

# shellcheck source-path=SCRIPTDIR/..
# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

BIG=reconnect-big
FRESH=reconnect-fresh
HELD=reconnect-held
RC_FILE=/tmp/lbfs-reconnect-rc
HELD_OPEN=/tmp/lbfs-reconnect-held-open
HELD_GO=/tmp/lbfs-reconnect-held-go
HELD_RESULT=/tmp/lbfs-reconnect-held-result
# Same sizing argument as disconnect.sh: large enough that a virtio link doing
# ~300 MB/s cannot finish it before the sever. Unlike over there, the whole
# file eventually crosses — the mount survives — so the dd's tail also gives
# the resumed connection real work to carry.
COUNT="${LBFS_RECONNECT_MB:-4096}"
# How much has to have crossed before the interruption counts as mid-I/O.
INFLIGHT_BYTES="${LBFS_RECONNECT_INFLIGHT:-33554432}"

# The two lines rpc/mod.rs logs: one on a successful RESUME claim, one on
# DETACH. Grepped from the journal, so a rename of either breaks this drill on
# purpose — the log line is the observable contract.
CLAIM_LINE='resumed a retained session'
DETACH_LINE='detached a session at the client'

ok() { printf '  ok    %s\n' "$1"; }

die() {
  printf '  FAIL  %s\n' "$1" >&2
  exit 1
}

# Whatever happens, the mount goes away, the held-descriptor helper dies, and
# the scratch files leave the export. The server should never have stopped;
# start it anyway, so a failed drill cannot poison the steps after it.
restore() {
  local rc=$?
  vm_ssh "$CLIENT_IP" "pkill -f $HELD 2>/dev/null || true
    fusermount3 -u $CLIENT_MOUNT 2>/dev/null || fusermount3 -uz $CLIENT_MOUNT 2>/dev/null || true
    pkill -x lbfs-client 2>/dev/null || true
    rm -f $RC_FILE $HELD_OPEN $HELD_GO $HELD_RESULT
    exit 0" || true
  vm_ssh "$SERVER_IP" "systemctl is-active --quiet lbfs-server || sudo systemctl start lbfs-server
    sudo rm -f $SERVER_EXPORT/$BIG $SERVER_EXPORT/$FRESH $SERVER_EXPORT/$HELD
    exit 0" || true
  return "$rc"
}
trap restore EXIT

mount_client() {
  vm_ssh "$CLIENT_IP" "nohup lbfs-client $SERVER_IP:$SERVER_PORT $SERVER_EXPORT $CLIENT_MOUNT \
      >/tmp/lbfs-reconnect.log 2>&1 </dev/null &
    for _ in \$(seq 1 50); do
      grep -q ' $CLIENT_MOUNT fuse' /proc/mounts && exit 0
      sleep 0.2
    done
    cat /tmp/lbfs-reconnect.log >&2
    exit 1"
}

mount_client || die 'the client did not mount'
ok 'mounted'

# A descriptor opened now and held across the sever, by a helper that outlives
# its ssh session. The drill signals it with marker files: it opens the file
# and reports, waits for the go signal, then reads the line it wrote at create
# time back through fd 3 and appends another after it. `read` advances the
# offset past the first line, so the append lands after it and the file ends up
# two lines long — which the server's copy is checked against, byte for byte.
# The wait is self-limiting (120 s) so an aborted drill cannot strand it.
vm_ssh "$CLIENT_IP" "rm -f $HELD_OPEN $HELD_GO $HELD_RESULT
  echo held-before > $CLIENT_MOUNT/$HELD
  nohup sh -c '
    exec 3<>$CLIENT_MOUNT/$HELD || exit 1
    : > $HELD_OPEN
    for _ in \$(seq 1 600); do [ -f $HELD_GO ] && break; sleep 0.2; done
    IFS= read -r first <&3 || first=read-failed
    wrote=ok
    printf \"held-after\\n\" >&3 || wrote=write-failed
    exec 3>&-
    printf \"%s|%s\\n\" \"\$first\" \"\$wrote\" > $HELD_RESULT
  ' >/dev/null 2>&1 </dev/null &
  exit 0"
deadline=$((SECONDS + 20))
until vm_ssh "$CLIENT_IP" "[ -f $HELD_OPEN ]"; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    die 'the helper never opened its descriptor on the mount'
  fi
  sleep 0.5
done
ok 'a helper holds an open descriptor on the mount'

# Backgrounded with its stdio detached, exactly as in disconnect.sh: otherwise
# ssh waits for the write to finish and there is no "mid-I/O" left to
# interrupt. conv=fsync so the rc covers the flush and not just the page cache.
vm_ssh "$CLIENT_IP" "rm -f $RC_FILE
  nohup sh -c 'dd if=/dev/zero of=$CLIENT_MOUNT/$BIG bs=1M count=$COUNT conv=fsync status=none;
    echo \$? > $RC_FILE' >/dev/null 2>&1 </dev/null &
  exit 0"

# "Mid-I/O" measured, not slept for, for disconnect.sh's reason: the server
# watching its own file grow is the unambiguous signal that bytes are crossing
# right now, which is the moment worth interrupting.
deadline=$((SECONDS + 60))
until [ "$(vm_ssh "$SERVER_IP" "stat -c%s $SERVER_EXPORT/$BIG 2>/dev/null || echo 0")" \
  -ge "$INFLIGHT_BYTES" ]; do
  if ! vm_ssh "$CLIENT_IP" "[ ! -f $RC_FILE ]"; then
    die "the ${COUNT}MiB write finished before the connection could be severed; raise LBFS_RECONNECT_MB"
  fi
  if [ "$SECONDS" -ge "$deadline" ]; then
    die "the write never reached $INFLIGHT_BYTES bytes on the server"
  fi
  sleep 0.2
done
ok "a ${COUNT}MiB write is in flight and the server has taken $INFLIGHT_BYTES bytes of it"

# The journal window for the claim and detach greps opens here, on the server's
# own clock, just before the sever puts anything in it.
since="$(vm_ssh "$SERVER_IP" 'date "+%Y-%m-%d %H:%M:%S"')"

# The sever itself. `ss -K` (CONFIG_INET_DIAG_DESTROY plus root, both present
# on the guests) aborts the TCP connection from the server's side, which resets
# the client's end too — a transport failure with the server left standing,
# which is the one case disconnect.sh cannot produce. Run on the server, the
# connection's *local* port is 9423 and the client holds the ephemeral end, so
# the filter selects on sport; dport would describe the client's view. `ss -K`
# exits zero whether or not anything matched, so the assertion is on the socket
# it prints having been the client's.
killed="$(vm_ssh "$SERVER_IP" "sudo ss -K dst $CLIENT_IP sport = :$SERVER_PORT 2>&1")"
grep -q "$CLIENT_IP" <<<"$killed" || die "ss -K matched no connection from $CLIENT_IP; got: $killed"
vm_ssh "$SERVER_IP" 'systemctl is-active --quiet lbfs-server' ||
  die 'lbfs-server itself died at the sever; the drill needs it standing'
ok 'severed the TCP connection; the server is still up'

# The whole feature, timed: a write issued after the break parks while the
# client re-attaches, then lands. The 20-second timeout is the assertion that
# it does not hang; the measured figure (ssh round trips included) is what
# "costs latency" turns out to mean, and should sit near the reconnect backoff
# — well under the client's 10-second deadline.
t0="$(date +%s%3N)"
vm_ssh "$CLIENT_IP" \
  "timeout 20 dd if=/dev/zero of=$CLIENT_MOUNT/$FRESH bs=4k count=1 conv=fsync status=none" ||
  die 'a fresh write did not land within 20s of the sever'
elapsed_ms=$(($(date +%s%3N) - t0))
ok "a fresh write to the mount landed ${elapsed_ms}ms after the sever"

# The interrupted dd may go either way, and the drill records which rather than
# demanding one: writes in flight at the break fail EIO by design, and
# conv=fsync surfaces that at the end — but a dd whose pages all happened to be
# acknowledged before the break has nothing to fail. Both are honest outcomes
# of the same contract. The generous deadline is for the tail of the file
# crossing the resumed connection.
deadline=$((SECONDS + 180))
until vm_ssh "$CLIENT_IP" "[ -f $RC_FILE ]"; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    die 'the interrupted write neither completed nor failed 180s after the sever'
  fi
  sleep 1
done
dd_rc="$(vm_ssh "$CLIENT_IP" "cat $RC_FILE")"
if [ "$dd_rc" = 0 ]; then
  ok 'the interrupted dd ran to completion (exit 0): nothing of it was in flight at the break'
else
  ok "the interrupted dd failed (exit $dd_rc): its in-flight writes died with the socket, as designed"
fi

# Now the held descriptor: reads and writes through the same fd 3 the helper
# opened before the sever, then the bytes on the server, in order.
vm_ssh "$CLIENT_IP" ": > $HELD_GO"
deadline=$((SECONDS + 30))
until vm_ssh "$CLIENT_IP" "[ -f $HELD_RESULT ]"; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    die 'the held-descriptor helper never reported back'
  fi
  sleep 0.5
done
held="$(vm_ssh "$CLIENT_IP" "cat $HELD_RESULT")"
[ "$held" = 'held-before|ok' ] ||
  die "the descriptor held across the sever broke (read|write = $held)"
# fsync by name so the writeback cache cannot hide the append from the server.
vm_ssh "$CLIENT_IP" "sync $CLIENT_MOUNT/$HELD"
got="$(vm_ssh "$SERVER_IP" "cat $SERVER_EXPORT/$HELD")"
expected="$(printf 'held-before\nheld-after')"
[ "$got" = "$expected" ] ||
  die "the server's copy of $HELD reads '$got', not both lines in order"
ok 'a descriptor opened before the sever still reads and writes, and the bytes landed in order'

# The registry's side of the story: the reconnect above must have been a RESUME
# claim on the retained session, not anything quieter.
vm_ssh "$SERVER_IP" "sudo journalctl -u lbfs-server --since '$since' --no-pager" |
  grep -q "$CLAIM_LINE" ||
  die "the server journal carries no '$CLAIM_LINE' line since the sever"
ok "the server logged the claim ('$CLAIM_LINE')"

vm_ssh "$CLIENT_IP" "rm -f $CLIENT_MOUNT/$BIG $CLIENT_MOUNT/$FRESH $CLIENT_MOUNT/$HELD" ||
  die 'could not remove the scratch files through the mount'

if ! vm_ssh "$CLIENT_IP" "timeout 30 fusermount3 -u $CLIENT_MOUNT"; then
  die 'fusermount3 -u could not unmount'
fi
deadline=$((SECONDS + 30))
while vm_ssh "$CLIENT_IP" 'pgrep -x lbfs-client >/dev/null'; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    die 'lbfs-client did not exit 30s after the unmount'
  fi
  sleep 1
done
ok 'unmounted cleanly and the client exited'

# The clean unmount must have detached, so the session is not squatting on the
# export's descriptors for the rest of the grace. The client exits only after
# the DETACH reply, so one short poll absorbs journal latency and no more.
deadline=$((SECONDS + 15))
until vm_ssh "$SERVER_IP" "sudo journalctl -u lbfs-server --since '$since' --no-pager" |
  grep -q "$DETACH_LINE"; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    die "the server journal carries no '$DETACH_LINE' line after the unmount"
  fi
  sleep 1
done
ok "the server logged the detach ('$DETACH_LINE')"

echo 'RECONNECT OK'

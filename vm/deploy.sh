#!/usr/bin/env bash
#
# Push the guest-built binaries onto the running pair and (re)start the server.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR
# shellcheck source=lib.sh
source "$(dirname "$0")/lib.sh"

BIN="${LBFS_GUEST_BIN:-$REPO_DIR/target/guest/release}"

for b in lbfs-server lbfs-client; do
  [ -x "$BIN/$b" ] || {
    echo "missing $BIN/$b - run 'make build-guest'" >&2
    exit 1
  }
done

vm_scp "$BIN/lbfs-server" "$VM_DIR/server-config.toml" "$VM_DIR/lbfs-server.service" \
  "ubuntu@$SERVER_IP:/tmp/"
vm_ssh "$SERVER_IP" 'sudo install -m755 /tmp/lbfs-server /usr/local/bin/ &&
  sudo install -m644 /tmp/server-config.toml /etc/lbfs.toml &&
  sudo install -m644 /tmp/lbfs-server.service /etc/systemd/system/ &&
  sudo systemctl daemon-reload &&
  sudo systemctl enable lbfs-server >/dev/null &&
  sudo systemctl restart lbfs-server'

# `restart` returns as soon as the unit is activating, and the unit now carries
# `Restart=on-failure`, so a server that dies on a bad config no longer dies
# silently — it dies over and over, and a single `is-active` would sooner or
# later catch it during one of those lives and call the deploy good. Watch it
# for ten seconds instead, and demand two things at once: the unit stays active,
# and systemd's restart counter stays at zero. The manual `systemctl restart`
# above resets NRestarts, so any nonzero value inside this window is this
# deploy's server crash-looping. Sampled throughout rather than once at the end,
# because a server that dies at second three and is back by second four is
# exactly what a single late look would miss.
#
# SC2016: the substitutions below are the guest's, evaluated over there against
# the guest's own systemd.
# shellcheck disable=SC2016
if ! vm_ssh "$SERVER_IP" '
  for _ in $(seq 1 20); do
    restarts=$(systemctl show -p NRestarts --value lbfs-server)
    state=$(systemctl is-active lbfs-server)
    if [ "${restarts:-0}" -ne 0 ]; then
      echo "lbfs-server has restarted ${restarts} time(s) since the deploy" >&2
      exit 1
    fi
    if [ "$state" != active ]; then
      echo "lbfs-server is ${state}" >&2
      exit 1
    fi
    sleep 0.5
  done'; then
  vm_ssh "$SERVER_IP" 'sudo journalctl -u lbfs-server -n 20 --no-pager' >&2 || true
  echo "lbfs-server did not stay up for ten seconds on $SERVER_IP" >&2
  exit 1
fi

vm_scp "$BIN/lbfs-client" "ubuntu@$CLIENT_IP:/tmp/"
vm_ssh "$CLIENT_IP" 'sudo install -m755 /tmp/lbfs-client /usr/local/bin/'

echo "deployed."
echo "  server  $SERVER_IP  lbfs-server is $(vm_ssh "$SERVER_IP" 'systemctl is-active lbfs-server') on :$SERVER_PORT, exporting $SERVER_EXPORT"
echo "  client  $CLIENT_IP  /usr/local/bin/lbfs-client"
echo "  mount   lbfs-client $SERVER_IP:$SERVER_PORT $SERVER_EXPORT $CLIENT_MOUNT"

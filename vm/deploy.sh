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

# `restart` returns as soon as the unit is activating, and `Restart=no` means a
# server that dies on a bad config dies silently. Ask, and print the journal if
# the answer is no — a deploy that reports success over a dead server is the
# one failure mode that would waste the most of Task 18's time.
if ! vm_ssh "$SERVER_IP" 'systemctl is-active --quiet lbfs-server'; then
  vm_ssh "$SERVER_IP" 'sudo journalctl -u lbfs-server -n 50 --no-pager' >&2 || true
  echo "lbfs-server is not active on $SERVER_IP" >&2
  exit 1
fi

vm_scp "$BIN/lbfs-client" "ubuntu@$CLIENT_IP:/tmp/"
vm_ssh "$CLIENT_IP" 'sudo install -m755 /tmp/lbfs-client /usr/local/bin/'

echo "deployed."
echo "  server  $SERVER_IP  lbfs-server is $(vm_ssh "$SERVER_IP" 'systemctl is-active lbfs-server') on :$SERVER_PORT, exporting $SERVER_EXPORT"
echo "  client  $CLIENT_IP  /usr/local/bin/lbfs-client"
echo "  mount   lbfs-client $SERVER_IP:$SERVER_PORT $SERVER_EXPORT $CLIENT_MOUNT"

# Shared definitions for the lbfs VM harness. Source it; do not run it.
#
# Everything here is deliberately inert: names, paths and helpers. The scripts
# that source it (up, down, deploy, test) are the ones that do something.
# shellcheck shell=bash
#
# SC2034: this is a library. Every definition here is consumed by a sourcing
# script, which shellcheck cannot see from inside this file.
# shellcheck disable=SC2034

VM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$VM_DIR/.." && pwd)"

# Disk images live outside the checkout on purpose. `qemu:///system` runs QEMU
# as uid `qemu`, and a developer checkout normally sits behind at least one
# 0770 directory that uid cannot traverse; widening those modes to let it in is
# a worse trade than putting the disks in the XDG libvirt image directory,
# which is world-traversable and already carries an svirt file label. Point
# LBFS_VM_IMAGES somewhere else if your checkout is already qemu-readable.
IMAGES="${LBFS_VM_IMAGES:-$HOME/.local/share/libvirt/images/lbfs}"

# Generated host-side files: the rendered cloud-init user-data, and a throwaway
# ssh key if the caller has none. QEMU never reads these, so they can stay in
# the (gitignored) checkout where they are easy to inspect after a failure.
GEN="$VM_DIR/images"

BASE_IMG="$IMAGES/ubuntu-2604-base.img"
UBUNTU_IMG_URL="https://cloud-images.ubuntu.com/releases/26.04/release/ubuntu-26.04-server-cloudimg-amd64.img"

NET_NAME="lbfs-net"
SERVER_VM="lbfs-server"
CLIENT_VM="lbfs-client"
VMS=("$SERVER_VM" "$CLIENT_VM")
SERVER_IP="192.168.77.10"
CLIENT_IP="192.168.77.11"
SERVER_MAC="52:54:00:77:00:10"
CLIENT_MAC="52:54:00:77:00:11"

# The export the server offers and the directory the client mounts it on. Both
# are created by cloud-init; Task 18's suite works inside them.
SERVER_EXPORT="/srv/exports/data"
CLIENT_MOUNT="/mnt/lbfs"
SERVER_PORT="9423"

# What cloud-init must have installed before a guest counts as ready. Kept
# beside the addresses because it is the same kind of promise: up.sh asserts it,
# and the template in cloud-init/user-data.tmpl.yaml has to match.
#
# jq is on the list because vm/tests/fio.sh reads fio's JSON with it. Today's
# Ubuntu cloud image happens to ship it, which is exactly the kind of luck that
# turns into "jq: command not found" on the first respin, in a step whose
# failure would look like a throughput regression.
GUEST_PACKAGES="fuse3 attr fio gcc curl make jq"

VIRSH=(virsh --connect qemu:///system)

SSH_OPTS=(
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o ConnectTimeout=5
  -o LogLevel=ERROR
)

# The key the guests will trust. An explicit SSH_PUBKEY wins; otherwise the
# caller's own ed25519 key if they have one, and failing that a throwaway pair
# minted under vm/images. Nothing in this tree ever writes to ~/.ssh.
if [ -z "${SSH_PUBKEY:-}" ]; then
  if [ -f "$HOME/.ssh/id_ed25519.pub" ]; then
    SSH_PUBKEY="$HOME/.ssh/id_ed25519.pub"
  else
    SSH_PUBKEY="$GEN/lbfs_ed25519.pub"
  fi
fi
SSH_KEY="${SSH_PUBKEY%.pub}"

# Create the throwaway pair if that is what we settled on. Only up.sh calls
# this: the other scripts want to fail loudly rather than mint a key the
# running guests have never heard of.
vm_ensure_ssh_key() {
  if [ -f "$SSH_PUBKEY" ]; then
    return 0
  fi
  if [ "$SSH_PUBKEY" != "$GEN/lbfs_ed25519.pub" ]; then
    echo "missing ssh public key $SSH_PUBKEY" >&2
    return 1
  fi
  mkdir -p "$GEN"
  echo "no ed25519 key found; minting a throwaway pair at $SSH_KEY"
  ssh-keygen -q -t ed25519 -N '' -C lbfs-vm -f "$SSH_KEY"
}

# The identity is recomputed per call, not once at source time, because up.sh
# mints the throwaway key after lib.sh has been sourced. An absent private key
# is not an error: the caller may have named a public key whose private half
# lives in an agent.
vm_ssh() {
  local ip="$1"
  shift
  local id=()
  if [ -f "$SSH_KEY" ]; then id=(-i "$SSH_KEY" -o IdentitiesOnly=yes); fi
  # SC2029: expanding the command on the guest is the entire point of the call.
  # shellcheck disable=SC2029
  ssh "${SSH_OPTS[@]}" "${id[@]}" "ubuntu@$ip" "$@"
}

vm_scp() {
  local id=()
  if [ -f "$SSH_KEY" ]; then id=(-i "$SSH_KEY" -o IdentitiesOnly=yes); fi
  scp "${SSH_OPTS[@]}" "${id[@]}" "$@"
}

vm_exists() {
  "${VIRSH[@]}" dominfo "$1" >/dev/null 2>&1
}

# Both halves matter. On a modular libvirt install virtqemud can be up while
# virtnetworkd is not, and `version` alone would happily report success on a
# host that cannot define a network.
vm_libvirt_ready() {
  "${VIRSH[@]}" version >/dev/null 2>&1 && "${VIRSH[@]}" net-list --all >/dev/null 2>&1
}

# libvirt 9+ splits the monolithic libvirtd into per-driver daemons, each
# socket-activated and each independently enabled. A workstation that has never
# run a VM typically has some of them masked off, so rather than making the
# operator guess which one is missing, start the four this harness needs.
#
# `start`, never `enable`: this is a test harness helping itself to a running
# daemon for the length of a session, not a claim on how the host should boot.
vm_preflight() {
  if vm_libvirt_ready; then
    return 0
  fi
  echo "libvirt is not answering on qemu:///system; starting its sockets (sudo)" >&2
  sudo systemctl start \
    virtqemud.socket virtnetworkd.socket virtstoraged.socket virtnodedevd.socket
  for _ in $(seq 1 10); do
    if vm_libvirt_ready; then
      return 0
    fi
    sleep 1
  done
  echo "libvirt is still unreachable on qemu:///system" >&2
  return 1
}

# The osinfo database this host ships may predate the guest release. 26.04 if
# it is known, the newest LTS this harness was written against otherwise: the
# id only seeds device defaults, and 24.04's are right for 26.04 as well.
vm_osinfo() {
  if osinfo-query --fields=short-id os 2>/dev/null | tr -d '[:blank:]' |
    grep -Fxq 'ubuntu26.04'; then
    echo 'ubuntu26.04'
  else
    echo 'ubuntu24.04'
  fi
}

vm_wait_ssh() {
  local ip="$1" tries="${2:-90}"
  for _ in $(seq 1 "$tries"); do
    if vm_ssh "$ip" true 2>/dev/null; then
      return 0
    fi
    sleep 5
  done
  echo "no ssh on $ip after $((tries * 5))s" >&2
  return 1
}

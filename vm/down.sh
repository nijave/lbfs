#!/usr/bin/env bash
#
# Tear the pair down. Safe to run when there is nothing to tear down.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR
# shellcheck source=lib.sh
source "$(dirname "$0")/lib.sh"

vm_preflight

for vm in "${VMS[@]}"; do
  "${VIRSH[@]}" destroy "$vm" 2>/dev/null || true
  "${VIRSH[@]}" undefine "$vm" --nvram 2>/dev/null || true
  rm -f "$IMAGES/$vm.qcow2"
done

# The base image and the network stay: re-downloading 820 MiB and re-defining
# lbfs-net for every kernel swap is the whole cost this harness exists to avoid.
echo "down. base image and $NET_NAME left in place for the next vm-up."

#!/usr/bin/env bash
#
# Tear the pair down. Safe to run when there is nothing to tear down.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR
# shellcheck source=lib.sh
source "$(dirname "$0")/lib.sh"

vm_preflight

failed=0
for vm in "${VMS[@]}"; do
  # A domain that is already shut off, or absent, makes `destroy` fail. That is
  # not news, so it stays quiet; the check that matters comes after.
  "${VIRSH[@]}" destroy "$vm" >/dev/null 2>&1 || true

  # From here the errors are worth hearing. libvirt's own message goes to the
  # terminal and the recheck below decides — because deleting a disk out from
  # under a domain that is still defined is the worst of the outcomes: every
  # later vm-up refuses (the domain exists) and every later vm-down repeats the
  # same failure, with nothing on screen either time to say why.
  if vm_exists "$vm"; then
    "${VIRSH[@]}" undefine "$vm" --nvram || true
  fi

  if vm_exists "$vm"; then
    echo "$vm is still defined; leaving $IMAGES/$vm.qcow2 in place" >&2
    failed=1
    continue
  fi

  rm -f "$IMAGES/$vm.qcow2"
done

if [ "$failed" -ne 0 ]; then
  echo "vm-down did not finish; see the errors above" >&2
  exit 1
fi

# The base image and the network stay: re-downloading 820 MiB and re-defining
# lbfs-net for every kernel swap is the whole cost this harness exists to avoid.
echo "down. base image and $NET_NAME left in place for the next vm-up."
echo
echo "$NET_NAME is defined with autostart, so it outlives a reboot. To remove it:"
echo "  ${VIRSH[*]} net-destroy $NET_NAME"
echo "  ${VIRSH[*]} net-undefine $NET_NAME   # clears the autostart flag too"
echo "The base image and any staged kernels are under $IMAGES."

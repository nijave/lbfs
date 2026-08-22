#!/usr/bin/env bash
#
# Boot the lbfs server/client pair on the local libvirt.
#
#   vm/up.sh [KERNEL]
#
# KERNEL names a directory under vm/kernels/ holding a `vmlinuz` + `initrd.img`
# pair to direct-boot instead of the one inside the cloud image. See
# vm/kernels/README.md.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR
# shellcheck source=lib.sh
source "$(dirname "$0")/lib.sh"

KERNEL="${1:-}"

# Validated before anything expensive happens. A mistyped kernel name should
# cost nothing — not an 820 MiB download, and certainly not half a defined pair
# that the operator then has to tear down by hand.
KERNEL_SRC=""
if [ -n "$KERNEL" ]; then
  KERNEL_SRC="$VM_DIR/kernels/$KERNEL"
  [ -f "$KERNEL_SRC/vmlinuz" ] || {
    echo "missing $KERNEL_SRC/vmlinuz" >&2
    exit 1
  }
  [ -f "$KERNEL_SRC/initrd.img" ] || {
    echo "missing $KERNEL_SRC/initrd.img" >&2
    exit 1
  }
fi

vm_preflight

# virt-install would refuse the second definition anyway, but only after the
# first guest had been created — so check the pair up front and leave the host
# exactly as it was found.
for vm in "${VMS[@]}"; do
  if vm_exists "$vm"; then
    echo "$vm already exists; run 'make vm-down' first" >&2
    exit 1
  fi
done

mkdir -p "$IMAGES" "$GEN"
# uid `qemu` opens the disks, so the directory holding them has to be
# traversable whatever umask the caller runs under. A umask of 077 would leave
# it 0700 and reintroduce the exact permission failure that putting the disks
# here avoided. (The XDG chain above it is 0755 by convention; this fixes the
# one directory the harness creates.)
chmod a+rx "$IMAGES"

if [ ! -f "$BASE_IMG" ]; then
  echo "fetching $UBUNTU_IMG_URL"
  # Through a .part file: an interrupted download must not leave something at
  # $BASE_IMG that the next run mistakes for a complete image.
  curl -fL --retry 3 "$UBUNTU_IMG_URL" -o "$BASE_IMG.part"
  mv "$BASE_IMG.part" "$BASE_IMG"
fi

# A direct-booted kernel is opened by QEMU, not by this script, so it has to be
# staged next to the disks for the same reason they live there (see lib.sh).
boot_args=()
if [ -n "$KERNEL" ]; then
  stage="$IMAGES/kernels/$KERNEL"
  mkdir -p "$stage"
  cp -f "$KERNEL_SRC/vmlinuz" "$KERNEL_SRC/initrd.img" "$stage/"
  chmod a+rx "$stage"
  chmod a+r "$stage/vmlinuz" "$stage/initrd.img"
  boot_args=(--boot "kernel=$stage/vmlinuz,initrd=$stage/initrd.img,kernel_args=root=LABEL=cloudimg-rootfs ro console=ttyS0")
fi

"${VIRSH[@]}" net-info "$NET_NAME" >/dev/null 2>&1 || {
  "${VIRSH[@]}" net-define "$VM_DIR/net.xml"
  "${VIRSH[@]}" net-autostart "$NET_NAME"
}
"${VIRSH[@]}" net-start "$NET_NAME" 2>/dev/null || true

vm_ensure_ssh_key
KEY="$(cat "$SSH_PUBKEY")"
OSINFO="$(vm_osinfo)"

declare -A MACS=([lbfs-server]="$SERVER_MAC" [lbfs-client]="$CLIENT_MAC")
for vm in "${VMS[@]}"; do
  # Per guest, because the hostname is substituted too: without it both boot as
  # `ubuntu` and every log line from Task 18 becomes ambiguous.
  user_data="$GEN/user-data-$vm.yaml"
  sed -e "s|__SSH_KEY__|$KEY|" -e "s|__HOSTNAME__|$vm|" \
    "$VM_DIR/cloud-init/user-data.tmpl.yaml" >"$user_data"

  overlay="$IMAGES/$vm.qcow2"
  rm -f "$overlay"
  qemu-img create -f qcow2 -b "$BASE_IMG" -F qcow2 "$overlay" 20G >/dev/null

  virt-install --connect qemu:///system \
    --name "$vm" --memory 2048 --vcpus 2 \
    --disk "path=$overlay,format=qcow2" \
    --import --osinfo "$OSINFO" \
    --network "network=$NET_NAME,mac=${MACS[$vm]}" \
    --cloud-init "user-data=$user_data" \
    --noautoconsole "${boot_args[@]}"
done

echo "waiting for ssh..."
for ip in "$SERVER_IP" "$CLIENT_IP"; do
  vm_wait_ssh "$ip"
done

# sshd answers well before cloud-init has finished installing packages, and
# every one of those packages is something Task 18 or the client needs. Waiting
# here is what makes `make vm-up && make vm-deploy` safe to type as one line.
#
# Bounded, because `--wait` is not: an unreachable archive mirror would
# otherwise hang vm-up with no output and no ceiling. Fifteen minutes is far
# past a healthy run — this pair converges in about twenty seconds — and far
# short of a wasted afternoon.
echo "waiting for cloud-init..."
for ip in "$SERVER_IP" "$CLIENT_IP"; do
  if ! vm_ssh "$ip" 'timeout 900 cloud-init status --wait >/dev/null'; then
    echo "warning: cloud-init on $ip did not finish cleanly" >&2
    vm_ssh "$ip" 'cloud-init status --long' >&2 || true
  fi

  # The verdict comes from dpkg, not from cloud-init's own opinion of itself.
  # `done` says nothing about whether a package landed, and `degraded` covers
  # plenty that Task 18 would never notice — so ask directly about the six the
  # suite and the client's libfuse3 link depend on, and refuse to report a
  # working pair without them. `db:Status-Status` reads `installed` only for a
  # package that is really there, and dpkg-query complains on stderr about one
  # it has never heard of, which is worth keeping in the diagnostic.
  missing="$(vm_ssh "$ip" \
    "dpkg-query -W -f='\${db:Status-Status} \${Package}\n' $GUEST_PACKAGES 2>&1 |
       grep -v '^installed ' || true")"
  if [ -n "$missing" ]; then
    echo "cloud-init left $ip without packages this harness promises:" >&2
    echo "$missing" >&2
    exit 1
  fi

  # SC2016: single-quoted so the guest, not this shell, answers.
  # shellcheck disable=SC2016
  vm_ssh "$ip" 'echo "$(hostname): $(uname -r)"'
done

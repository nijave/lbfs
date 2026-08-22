# Swappable guest kernels

The point of this directory is to answer "does lbfs still behave on kernel X?"
without rebuilding the guests. The cloud image's own kernel is the default; drop
a pair in here and `vm/up.sh` direct-boots that instead, with the same rootfs,
the same cloud-init and the same addresses.

## Dropping one in

```
vm/kernels/<name>/vmlinuz      # the uncompressed or bzImage kernel
vm/kernels/<name>/initrd.img   # an initramfs that can find LABEL=cloudimg-rootfs
```

Both names are exact — `up.sh` looks for those two files and nothing else.
Choose any `<name>`; that string becomes the value of `KERNEL=`.

Then:

```sh
make vm-down
make vm-up KERNEL=<name>
make vm-deploy
```

`vm-down` first: libvirt fixes the kernel at define time, so an existing pair
keeps whatever it booted with. Git ignores everything in this directory apart
from this file.

## Where to get a pair

Any of these produce something bootable here:

- **From a running Ubuntu guest.** `/boot/vmlinuz-<ver>` and
  `/boot/initrd.img-<ver>` copy straight across. Install the kernel you want in
  the guest (`apt install linux-image-<ver>`), then `scp` both files out.
- **From a `.deb`.** `dpkg-deb -x linux-image-....deb tmp/` and take
  `tmp/boot/vmlinuz-*`. That leaves you needing an initrd carrying the matching
  modules, which makes this a two-step: install the deb in a booted guest, let
  it build the initramfs, copy both out.
- **From your own build.** `arch/x86/boot/bzImage` after `make -j`, plus an
  initrd that carries the matching `/lib/modules`. A kernel with virtio, ext4
  and FUSE compiled in rather than modular boots with almost any initrd, which
  shortens a bisect considerably.

## What up.sh does with them

`up.sh` validates the pair before it touches libvirt: a missing file prints
`missing vm/kernels/<name>/vmlinuz` and creates nothing. It then copies both
into the disk-image directory and points libvirt at the copy. That copy is not
tidiness — QEMU opens these files itself, as uid `qemu`, which cannot read a
developer checkout. The disks live outside the repository for the same reason;
see the comment in `vm/lib.sh`.

The command line handed to the kernel is:

```
root=LABEL=cloudimg-rootfs ro console=ttyS0
```

which is what the Ubuntu cloud image expects. A kernel that cannot mount that
label — no virtio-blk, no ext4 — hangs in its initrd rather than failing
loudly. Look for the evidence on
`virsh --connect qemu:///system console lbfs-server`.

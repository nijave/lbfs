#!/usr/bin/env bash
#
# The other end of the POSIX matrix, run on the server VM directly against the
# exported directory.
#
# `e2e.sh` proved that the client is self-consistent. This proves that what the
# client saw is what is actually on the server's disk — the only thing a network
# filesystem is really for. Everything examined here is plain local I/O on
# $EXPORT; nothing in this script speaks the lbfs protocol.
set -euo pipefail

EXPORT="${1:?usage: server-check.sh <export-dir>}"
WITNESS="$EXPORT/witness"

ok() { printf '  ok    %s\n' "$1"; }

die() {
  printf '  FAIL  %s\n' "$1" >&2
  exit 1
}

# An empty side is a failed command, not a match. Two `stat`s that both failed
# would otherwise compare equal-and-empty and report success — the inode
# comparison below is exactly that shape.
check() {
  local what="$1" want="$2" got="$3"
  if [ -z "$want" ] || [ -z "$got" ]; then
    die "$what: an empty value means the command that produced it failed (want [$want], got [$got])"
  fi
  if [ "$want" != "$got" ]; then
    die "$what: want [$want], got [$got]"
  fi
  ok "$what = $got"
}

[ -d "$WITNESS" ] || die "no witness tree at $WITNESS"
cd "$WITNESS"

check 'file content on the server' 'hello lbfs' "$(cat dir/sub/file.txt)"
check 'mode on the server' 640 "$(stat -c%a dir/sub/file.txt)"
check 'mtime on the server' 1000000000 "$(stat -c%Y dir/sub/file.txt)"
check 'link count on the server' 2 "$(stat -c%h dir/sub/file.txt)"
check 'hardlink is the same inode on the server' \
  "$(stat -c%i dir/sub/file.txt)" "$(stat -c%i link.hard)"
check 'symlink target on the server' 'dir/sub/file.txt' "$(readlink link.sym)"
check 'xattr on the server' 'v1' "$(getfattr --only-values -n user.tag dir/sub/file.txt)"
check 'truncated size on the server' 4096 "$(stat -c%s truncated.bin)"
check 'renamed-over content on the server' 'second' "$(cat renamed.txt)"
check 'directory rename landed on the server' 'moved' "$(cat dirmove/b/inner.txt)"

if [ -e dirmove/a ]; then die 'the pre-rename directory name still exists on the server'; fi
if [ -e scratch ]; then die 'a removed directory still exists on the server'; fi
if [ -e wide ]; then die 'a removed 300-entry directory still exists on the server'; fi
if [ -e tmp.txt ] || [ -e replacement.txt ]; then
  die 'a renamed-away source name still exists on the server'
fi
ok 'every removed and renamed-away name is gone on the server'

# The xattr that was set and then removed must not be sitting on the inode.
if getfattr -d dir/sub/file.txt 2>/dev/null | grep -q '^user\.scratch='; then
  die 'a removed xattr is still on the inode on the server'
fi
ok 'the removed xattr is absent on the server'

check 'sparse apparent size on the server' 10485761 "$(stat -c%s sparse.bin)"
if [ "$(stat -c%b sparse.bin)" -ge 2048 ]; then
  die "the sparse file allocated $(stat -c%b sparse.bin) blocks on the server"
fi
ok "the hole reached the server's disk as a hole ($(stat -c%b sparse.bin) blocks)"

# The manifest was written through the mount by the client and is verified here
# against the server's own copies of the same files.
if ! md5sum -c MANIFEST >/dev/null; then
  die 'checksums written through the mount do not match the files on the server'
fi
ok "all $(wc -l <MANIFEST) checksums match on the server"

echo 'SERVER VIEW OK'

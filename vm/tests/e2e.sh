#!/usr/bin/env bash
#
# The POSIX matrix, run on the client VM against a mount that is already up.
#
# This half only asserts what the client can see. The point of the exercise is
# that the *server* agrees, and it cannot ssh anywhere, so the tree this builds
# is left in place under `witness/` for `server-check.sh` to re-examine from the
# other end. Anything checked here and not represented in the witness tree has
# only been proved to round-trip through the client's own caches.
#
# Nothing here needs root and nothing writes outside the mount except one
# listing file, so a failed run leaves the guest as it found it.
set -euo pipefail

MNT="${1:?usage: e2e.sh <mountpoint>}"
WITNESS="$MNT/witness"
LISTING=/tmp/lbfs-listing.txt

ok() { printf '  ok    %s\n' "$1"; }

die() {
  printf '  FAIL  %s\n' "$1" >&2
  exit 1
}

# Two arguments in `want got` order, because every call site reads better that
# way: the expectation is the short literal and the observation is the command
# substitution that wraps.
#
# Both sides must be non-empty. Several call sites compare one command
# substitution against another — two `stat`s that should agree — and a `stat`
# that fails prints nothing to stdout, so a pair of failures would compare
# empty against empty and pass. An empty expectation is never something this
# file means to assert.
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

grep -q " $MNT fuse" /proc/mounts || die "$MNT is not a fuse mount"

# A previous witness tree would make every assertion below ambiguous, so the
# suite owns this subtree outright and starts by taking it back.
rm -rf "${WITNESS:?}"
mkdir "$WITNESS"
cd "$WITNESS"

# --- create, write, read ---------------------------------------------------

echo 'hello lbfs' >tmp.txt
check 'read back a freshly written file' 'hello lbfs' "$(cat tmp.txt)"
check 'size of a freshly written file' 11 "$(stat -c%s tmp.txt)"

# --- mkdir, rename across directories, rmdir -------------------------------

mkdir -p dir/sub
mv tmp.txt dir/sub/file.txt
if [ -e tmp.txt ]; then die 'rename left the source name behind'; fi
check 'content survived a rename across directories' 'hello lbfs' "$(cat dir/sub/file.txt)"

mkdir dir/empty
rmdir dir/empty
if [ -e dir/empty ]; then die 'rmdir left the directory behind'; fi
ok 'rmdir removed an empty directory'

# A directory rename is a different server path from a file rename: the parent's
# `..` has to follow. `dirmove/b` stays in the witness so the server can confirm
# it landed under the new name.
mkdir -p dirmove/a
echo moved >dirmove/a/inner.txt
mv dirmove/a dirmove/b
check 'content survived a directory rename' 'moved' "$(cat dirmove/b/inner.txt)"

# Rename over an existing name must replace it, not fail.
echo first >renamed.txt
echo second >replacement.txt
mv replacement.txt renamed.txt
check 'rename replaced an existing destination' 'second' "$(cat renamed.txt)"

# --- symlinks --------------------------------------------------------------

ln -s dir/sub/file.txt link.sym
check 'readlink' 'dir/sub/file.txt' "$(readlink link.sym)"
check 'reading through a symlink' 'hello lbfs' "$(cat link.sym)"

# --- hard links ------------------------------------------------------------

ln dir/sub/file.txt link.hard
check 'link count after hardlink' 2 "$(stat -c%h link.hard)"
check 'hardlinks share one inode' "$(stat -c%i dir/sub/file.txt)" "$(stat -c%i link.hard)"
# Actually write, in both directions. Reading the same bytes through a second
# name only proves the name resolves; it takes a write to prove the server did
# not quietly hand out a file of its own.
echo 'via the hard link' >link.hard
check 'a write through one link is visible through the other' \
  'via the hard link' "$(cat dir/sub/file.txt)"
echo 'hello lbfs' >dir/sub/file.txt
check 'and a write back the other way' 'hello lbfs' "$(cat link.hard)"

# --- truncate, both directions ---------------------------------------------

dd if=/dev/zero of=truncated.bin bs=1k count=8 status=none
truncate -s 16384 truncated.bin
check 'truncate grew the file' 16384 "$(stat -c%s truncated.bin)"
truncate -s 4096 truncated.bin
check 'truncate shrank the file' 4096 "$(stat -c%s truncated.bin)"

# --- mode and timestamps ----------------------------------------------------

chmod 640 dir/sub/file.txt
check 'chmod' 640 "$(stat -c%a dir/sub/file.txt)"

# Last, so that nothing below quietly bumps it again. setxattr moves ctime, not
# mtime, but the ordering costs nothing and removes the question.
touch -d @1000000000 dir/sub/file.txt
check 'mtime after touch -d' 1000000000 "$(stat -c%Y dir/sub/file.txt)"

# --- extended attributes ----------------------------------------------------

# user.tag survives into the witness; user.scratch exists only to prove that
# listxattr sees a name and removexattr takes it away again.
setfattr -n user.tag -v v1 dir/sub/file.txt
check 'getxattr' 'v1' "$(getfattr --only-values -n user.tag dir/sub/file.txt)"

setfattr -n user.scratch -v gone dir/sub/file.txt
if ! getfattr -d dir/sub/file.txt 2>/dev/null | grep -q '^user\.scratch='; then
  die 'listxattr did not report user.scratch'
fi
ok 'listxattr reported both names'

setfattr -x user.scratch dir/sub/file.txt
if getfattr -d dir/sub/file.txt 2>/dev/null | grep -q '^user\.scratch='; then
  die 'removexattr left user.scratch behind'
fi
ok 'removexattr took the name away'

# --- sparse files, and the lseek(SEEK_DATA/SEEK_HOLE) path cp uses ----------

dd if=/dev/zero of=sparse.bin bs=1 count=1 seek=10M status=none
check 'apparent size of a sparse file' 10485761 "$(stat -c%s sparse.bin)"
cp --sparse=auto sparse.bin sparse.copy
check 'sparse copy has the same apparent size' \
  "$(stat -c%s sparse.bin)" "$(stat -c%s sparse.copy)"
if [ "$(stat -c%b sparse.copy)" -ge 2048 ]; then
  die "sparse copy allocated $(stat -c%b sparse.copy) blocks; the hole was filled in"
fi
ok "sparse copy kept the hole ($(stat -c%b sparse.copy) blocks allocated)"

# --- copy, which is where copy_file_range shows up if cp reaches for it -----

cp dir/sub/file.txt copied.txt
check 'copied content' 'hello lbfs' "$(cat copied.txt)"

# --- readdir ---------------------------------------------------------------

find . -type f | sort >"$LISTING"
for want in ./copied.txt ./dir/sub/file.txt ./dirmove/b/inner.txt ./link.hard \
  ./renamed.txt ./sparse.bin ./sparse.copy ./truncated.bin; do
  if ! grep -qxF "$want" "$LISTING"; then
    die "readdir walk did not find $want"
  fi
done
ok "readdir walk found all $(wc -l <"$LISTING") files"

# A directory big enough to need more than one READDIR round trip, so the
# snapshot semantics get exercised rather than assumed.
mkdir wide
for i in $(seq 1 300); do
  : >"wide/entry-$i"
done
check 'entries in a 300-entry directory' 300 "$(find wide -type f | wc -l)"
check 'a 300-entry directory sorts stably' 'wide/entry-1' "$(find wide -type f | sort | head -1)"

# --- unlink and recursive removal ------------------------------------------

rm -r wide
if [ -e wide ]; then die 'rm -r left the directory behind'; fi
ok 'rm -r removed a 300-entry directory'

mkdir -p scratch/deep
echo transient >scratch/deep/gone.txt
rm -r scratch
if [ -e scratch ]; then die 'rm -r left scratch behind'; fi
ok 'rm -r removed a nested directory'

# --- statfs ----------------------------------------------------------------

# df on an unimplemented statfs reports a zero-block filesystem, which is worse
# than an error because tools silently believe it.
blocks="$(df --output=size "$MNT" | tail -1 | tr -d '[:space:]')"
if [ "${blocks:-0}" -le 0 ]; then die "statfs reported $blocks blocks"; fi
ok "statfs reported $blocks 1K-blocks"

# --- a payload with a checksum the server will be asked to reproduce --------

head -c 1048576 /dev/urandom >data.bin
check 'payload size' 1048576 "$(stat -c%s data.bin)"

# The manifest travels through the filesystem under test and is checked again
# on the other side, so it proves the read path here and the write path there.
md5sum copied.txt data.bin dir/sub/file.txt link.hard renamed.txt sparse.bin \
  sparse.copy truncated.bin dirmove/b/inner.txt >MANIFEST
if ! md5sum -c --quiet MANIFEST; then
  die 'the manifest does not match the files it was just computed from'
fi
ok 'md5 manifest re-read cleanly through the mount'

# Writeback is on by default, so the server has seen none of the file data yet.
# Everything after this point — including the whole of server-check.sh — depends
# on this flush.
sync
ok 'sync flushed the writeback cache'

rm -f "$LISTING"
echo 'E2E OK'

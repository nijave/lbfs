#!/usr/bin/env bash
#
# The workload the design is actually aimed at: a build tree, run on the client
# VM inside the mount.
#
# A compile is almost all metadata. Every `cc` invocation stats a header it was
# told about, opens a source file, creates an object file, writes it in small
# pieces and closes it, and `make` stats every target and prerequisite before
# and after. That is a very different shape from the streaming numbers fio
# reports, and it is the shape that decides whether this filesystem is usable.
#
# Two phases, because they fail differently: a real link-and-run build, then a
# flat churn of many small files that isolates create/stat/rename/unlink from
# anything the compiler does.
set -euo pipefail

MNT="${1:?usage: build.sh <mountpoint>}"
UNITS="${2:-120}"
# A throughput sample, not a scale test. The server holds one O_PATH descriptor
# per node the client still remembers, and test.sh measures that ceiling in a
# step of its own; keeping this well under it means a number here always means
# what it says rather than "the descriptor budget ran out".
CHURN="${3:-500}"
TREE="$MNT/buildtree"
CHURNDIR="$MNT/churn"

ok() { printf '  ok    %s\n' "$1"; }

die() {
  printf '  FAIL  %s\n' "$1" >&2
  exit 1
}

# Seconds since an earlier `now`, to two decimal places, using bash's own clock
# so the guest does not need `bc`.
now() { printf '%s' "${EPOCHREALTIME/,/.}"; }
elapsed() { awk -v a="$1" -v b="$(now)" 'BEGIN { printf "%.2f", b - a }'; }
rate() { awk -v n="$1" -v s="$2" 'BEGIN { printf "%.0f", n / (s > 0 ? s : 1) }'; }

grep -q " $MNT fuse" /proc/mounts || die "$MNT is not a fuse mount"

rm -rf "${TREE:?}"
mkdir "$TREE"
cd "$TREE"

# --- phase 1: generate and compile -----------------------------------------

start="$(now)"
{
  echo '#ifndef COMMON_H'
  echo '#define COMMON_H'
  for i in $(seq 1 "$UNITS"); do echo "int unit_$i(int x);"; done
  echo '#endif'
} >common.h

for i in $(seq 1 "$UNITS"); do
  printf '#include "common.h"\nint unit_%s(int x) { return x + %s; }\n' "$i" "$i" >"unit_$i.c"
done

{
  printf '#include "common.h"\nint main(void) {\n  int acc = 0;\n'
  for i in $(seq 1 "$UNITS"); do printf '  acc += unit_%s(%s);\n' "$i" "$i"; done
  printf '  return acc == %s ? 0 : 1;\n}\n' "$((UNITS * (UNITS + 1)))"
} >main.c

# SC2016: `$(CC)`, `$@` and `$<` are make's variables, not this shell's, and the
# single quotes are what keeps them that way.
# shellcheck disable=SC2016
{
  printf 'OBJS :='
  for i in $(seq 1 "$UNITS"); do printf ' unit_%s.o' "$i"; done
  printf '\nCFLAGS := -O0 -Wall\nall: prog\n'
  printf 'prog: $(OBJS) main.o\n\t$(CC) -o $@ $^\n'
  printf '%%.o: %%.c common.h\n\t$(CC) $(CFLAGS) -c -o $@ $<\n'
} >Makefile
gen="$(elapsed "$start")"
ok "generated $((UNITS * 2 + 3)) source files in ${gen}s"

start="$(now)"
if ! make -j"$(nproc)" -s >/tmp/lbfs-build.log 2>&1; then
  sed -n '1,40p' /tmp/lbfs-build.log >&2
  die 'the build failed inside the mount'
fi
build="$(elapsed "$start")"
ok "compiled and linked $((UNITS + 1)) objects in ${build}s ($(rate "$((UNITS + 1))" "$build") objects/s)"

# Executing from the mount is its own path: the kernel maps the file, so this
# reads through the mount without any of the read() calls the rest of the suite
# makes.
if ! ./prog; then die 'the program built inside the mount did not run'; fi
ok 'the linked binary executed from the mount'

# An incremental rebuild is the case a build tree spends most of its life in:
# `make` stats everything and compiles nothing.
start="$(now)"
if ! make -j"$(nproc)" -s -q; then die 'make thinks the tree is out of date immediately after a build'; fi
ok "a no-op rebuild stat'd $((UNITS * 2 + 2)) paths in $(elapsed "$start")s"

# Touch one header and make sure the world rebuilds — this is the dependency
# graph seeing an mtime change that came back through the mount.
touch common.h
if make -j"$(nproc)" -s -q; then die 'make did not notice a touched header'; fi
ok 'make noticed a header whose mtime changed through the mount'

# The tree goes before the churn does, not after, so the churn is not competing
# with several hundred live build artefacts for the server's descriptor budget.
cd "$MNT"
rm -rf "${TREE:?}"
if [ -e "$TREE" ]; then die 'the build tree survived rm -r'; fi
rm -f /tmp/lbfs-build.log
ok 'removed the whole build tree'

# --- phase 2: small-file churn ---------------------------------------------

rm -rf "${CHURNDIR:?}"
mkdir "$CHURNDIR"
cd "$CHURNDIR"

start="$(now)"
for i in $(seq 1 "$CHURN"); do : >"f-$i"; done
create="$(elapsed "$start")"
ok "created $CHURN empty files in ${create}s ($(rate "$CHURN" "$create") creates/s)"

start="$(now)"
found="$(find . -type f | wc -l)"
[ "$found" -eq "$CHURN" ] || die "readdir saw $found of $CHURN files"
ok "walked $CHURN entries in $(elapsed "$start")s"

start="$(now)"
for i in $(seq 1 "$CHURN"); do mv "f-$i" "g-$i"; done
ok "renamed $CHURN files in $(elapsed "$start")s"

start="$(now)"
for i in $(seq 1 "$CHURN"); do rm "g-$i"; done
remove="$(elapsed "$start")"
ok "unlinked $CHURN files in ${remove}s ($(rate "$CHURN" "$remove") unlinks/s)"

if [ -n "$(ls -A)" ]; then die 'the churn directory is not empty after removing everything'; fi
cd "$MNT"
rmdir "$CHURNDIR"
if [ -e "$CHURNDIR" ]; then die 'the churn directory survived rmdir'; fi
ok 'removed the churn directory'

echo 'BUILD OK'

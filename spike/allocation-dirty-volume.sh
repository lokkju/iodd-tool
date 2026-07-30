#!/usr/bin/env bash
#
# Phase 0, third pass: strategy comparison on volumes with a HISTORY.
#
# WHY
#
# The second spike compared fallocate, sequential write, and fallocate+write
# on pristine volumes. All three produced one contiguous run, so the
# comparison said nothing: pristine is the easy case.
#
# The first spike's two-run result came from a volume that had been filled to
# ENOSPC and emptied. ntfsinfo showed why: the allocation started inside the
# MFT zone (clusters 0..131075, i.e. bytes 0..536883200) and had to jump out
# at the boundary. On a pristine volume the allocator skips the zone entirely.
#
# So volume history is the variable, and only fallocate was ever measured on a
# volume with history. A real IODD device is 87% used with years of history.
# This script measures the strategies under conditions that resemble that.
#
# SCENARIOS
#
#   dirty       filled to ENOSPC, then emptied.
#               Free space is whole again but the allocator state is not
#               pristine. This reproduces the S2 conditions.
#
#   fragmented  filled with N equal files, then every other one deleted.
#               Free space exists in alternating chunks, none large enough
#               for the target. No strategy can succeed here, so this
#               validates the REFUSAL path: the tool must detect and refuse
#               rather than silently emit a fragmented file.
#
# Each scenario runs all three strategies on its own freshly prepared volume.
#
# Needs sudo for mount/umount only. Touches nothing outside its temp directory.

set -euo pipefail

IMG_SIZE="${IMG_SIZE:-4G}"
ALLOC_SIZE="${ALLOC_SIZE:-1G}"
FRAG_FILES="${FRAG_FILES:-16}"
KEEP="${KEEP:-0}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIEMAP="$HERE/fiemap.py"

WORKDIR="$(mktemp -d /tmp/iodd-dirty.XXXXXX)"
IMG="$WORKDIR/ntfs.img"
MNT="$WORKDIR/mnt"

cleanup() {
    mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT" || true
    [[ "$KEEP" == "1" ]] && { echo; echo "artifacts kept in $WORKDIR"; } || rm -rf "$WORKDIR"
}
trap cleanup EXIT

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
sub() { printf '\n\033[1m-- %s\033[0m\n' "$*"; }

for tool in mkntfs fallocate mountpoint python3; do
    command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 1; }
done

mkdir -p "$MNT"

fresh_volume() {
    mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
    rm -f "$IMG"
    truncate -s "$IMG_SIZE" "$IMG"
    mkntfs --quick --force --label iodd-dirty "$IMG" >/dev/null 2>&1
    sudo mount -t ntfs3 -o "loop,uid=$(id -u),gid=$(id -g)" "$IMG" "$MNT"
}

# Fill to ENOSPC with one big file, then delete it.
prepare_dirty() {
    python3 - "$MNT/filler.bin" <<'PY' >/dev/null
import sys
block = bytes(4 * 1024 * 1024)
try:
    with open(sys.argv[1], "wb") as f:
        while True:
            f.write(block)
except OSError:
    pass
PY
    sync
    rm -f "$MNT/filler.bin"
    sync
}

# Fill with N equal files, delete every other one.
prepare_fragmented() {
    python3 - "$MNT" "$FRAG_FILES" <<'PY' >/dev/null
import os, sys
mnt, n = sys.argv[1], int(sys.argv[2])
free = os.statvfs(mnt)
budget = int(free.f_bavail * free.f_frsize * 0.95)
each = budget // n
block = bytes(4 * 1024 * 1024)
for i in range(n):
    try:
        with open(os.path.join(mnt, f"f{i:03d}.bin"), "wb") as f:
            written = 0
            while written < each:
                chunk = min(len(block), each - written)
                f.write(block[:chunk])
                written += chunk
    except OSError:
        break
PY
    sync
    # delete every other file, leaving alternating holes
    local i=0
    for f in "$MNT"/f*.bin; do
        [[ -e "$f" ]] || continue
        if (( i % 2 == 1 )); then rm -f "$f"; fi
        i=$((i + 1))
    done
    sync
}

measure() {
    local label="$1" seconds="$2"
    "$FIEMAP" --json "$MNT/target.bin" 2>/dev/null | python3 -c "
import json,sys
r = json.load(sys.stdin)[0]
runs = r['merged_runs']
mark = 'CONTIGUOUS' if runs == 1 else f'{runs} RUNS'
print(f\"   {'$label':<14} {mark:<12} unwritten={str(r['any_unwritten']):<5} {'$seconds'}s\")
for i, run in enumerate(r['runs'][:6]):
    print(f\"        run {i}: physical={run['physical']:<14} length={run['length']}\")
if len(r['runs']) > 6:
    print(f\"        ... {len(r['runs']) - 6} more runs\")
"
}

do_fallocate() { fallocate -l "$ALLOC_SIZE" "$MNT/target.bin"; }

do_seqwrite() {
    python3 - "$MNT/target.bin" "$ALLOC_SIZE" <<'PY'
import os, sys
path, size = sys.argv[1], sys.argv[2]
mult = {"K": 1024, "M": 1024**2, "G": 1024**3}
n = int(size[:-1]) * mult[size[-1].upper()] if size[-1].isalpha() else int(size)
block = bytes(4 * 1024 * 1024)
with open(path, "wb") as f:
    written = 0
    while written < n:
        f.write(block[: min(len(block), n - written)])
        written += min(len(block), n - written)
    f.flush()
    os.fsync(f.fileno())
PY
}

do_fallocate_write() {
    fallocate -l "$ALLOC_SIZE" "$MNT/target.bin"
    python3 - "$MNT/target.bin" <<'PY'
import os, sys
size = os.path.getsize(sys.argv[1])
block = bytes(4 * 1024 * 1024)
with open(sys.argv[1], "r+b") as f:
    written = 0
    while written < size:
        f.write(block[: min(len(block), size - written)])
        written += min(len(block), size - written)
    f.flush()
    os.fsync(f.fileno())
PY
}

run_scenario() {
    local scenario="$1"
    say "scenario: $scenario"
    for strategy in fallocate seqwrite fallocate_write; do
        fresh_volume
        "prepare_$scenario"
        df -h "$MNT" | tail -1 | sed 's/^/   free: /'
        local start=$SECONDS
        if ! "do_$strategy" 2>/dev/null; then
            echo "   ${strategy}: FAILED (likely ENOSPC — itself a valid outcome)"
            continue
        fi
        sync
        measure "$strategy" "$((SECONDS - start))"
    done
}

say "environment"
uname -r
echo "image: $IMG_SIZE   target: $ALLOC_SIZE   fragment files: $FRAG_FILES"

run_scenario dirty
run_scenario fragmented

say "how to read this"
cat <<'EOF'
   dirty scenario
     If one strategy is CONTIGUOUS where others are not, that is the
     strategy the engine should use. If all fragment, allocation on ntfs3
     cannot be steered from userspace under realistic conditions and the
     tool must verify and refuse rather than promise contiguity.

   fragmented scenario
     Everything is expected to fragment or fail here. That is the point:
     it is the refusal path, and the tool must detect it rather than
     emitting a file the device will not mount.
EOF

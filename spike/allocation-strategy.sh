#!/usr/bin/env bash
#
# Phase 0 follow-up: which allocation strategy yields ONE extent on ntfs3?
#
# WHY
#
# The first spike found that a single fallocate(2) of 1 GiB on a freshly
# emptied 4 GiB ntfs3 volume comes back in TWO runs, separated by a ~1.5 GiB
# physical gap. Zero fragmentation is this tool's entire contract, so "call
# fallocate and verify" cannot be the strategy if it fails on an empty volume.
#
# The first spike also found that fallocate does not zero, and that the tool
# must therefore write zeros across the data region regardless. That raises a
# hypothesis worth testing: if a plain sequential write lands contiguously
# where fallocate does not, then one pass both allocates and zeroes, and the
# zeroing cost disappears into work we already had to do.
#
# STRATEGIES COMPARED, each on a freshly formatted volume
#
#   fallocate     one fallocate(2) call                       (baseline)
#   seqwrite      sequential write of zeros, no fallocate     (the hypothesis)
#   fallocate+w   fallocate, then overwrite with zeros        (does the write
#                                                              relocate or
#                                                              clear UNWRITTEN?)
#
# Each is measured for: run count, whether any extent is UNWRITTEN, and
# elapsed time.
#
# Also reports the MFT zone from ntfsinfo, to test the hypothesis that the
# gap in the baseline is the MFT reservation.
#
# Needs sudo for mount/umount only. Touches nothing outside its temp directory.

set -euo pipefail

IMG_SIZE="${IMG_SIZE:-4G}"
ALLOC_SIZE="${ALLOC_SIZE:-1G}"
KEEP="${KEEP:-0}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIEMAP="$HERE/fiemap.py"

WORKDIR="$(mktemp -d /tmp/iodd-alloc.XXXXXX)"
IMG="$WORKDIR/ntfs.img"
MNT="$WORKDIR/mnt"

cleanup() {
    mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT" || true
    [[ "$KEEP" == "1" ]] && { echo; echo "artifacts kept in $WORKDIR"; } || rm -rf "$WORKDIR"
}
trap cleanup EXIT

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

for tool in mkntfs fallocate mountpoint python3; do
    command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 1; }
done

mkdir -p "$MNT"

fresh_volume() {
    mountpoint -q "$MNT" && sudo umount "$MNT"
    rm -f "$IMG"
    truncate -s "$IMG_SIZE" "$IMG"
    mkntfs --quick --force --label iodd-alloc "$IMG" >/dev/null
    sudo mount -t ntfs3 -o "loop,uid=$(id -u),gid=$(id -g)" "$IMG" "$MNT"
}

# Report run count / unwritten / contiguity for the target file.
measure() {
    local label="$1" seconds="$2"
    "$FIEMAP" --json "$MNT/target.bin" | python3 -c "
import json,sys
r = json.load(sys.stdin)[0]
runs = r['merged_runs']
mark = 'CONTIGUOUS' if runs == 1 else f'{runs} RUNS'
print(f\"   {'$label':<14} {mark:<12} unwritten={str(r['any_unwritten']):<5} \"
      f\"sparse={str(r['sparse']):<5} {'$seconds'}s\")
for i, run in enumerate(r['runs']):
    print(f\"        run {i}: physical={run['physical']:<14} length={run['length']}\")
if runs > 1:
    gaps = []
    rs = r['runs']
    for a, b in zip(rs, rs[1:]):
        gaps.append(b['physical'] - (a['physical'] + a['length']))
    print(f'        physical gaps: {gaps}')
"
}

say "0. environment"
uname -r
echo "image: $IMG_SIZE   target: $ALLOC_SIZE"
echo "workdir: $WORKDIR"

say "1. baseline — one fallocate(2) call"
fresh_volume
start=$SECONDS
fallocate -l "$ALLOC_SIZE" "$MNT/target.bin"
sync
measure "fallocate" "$((SECONDS - start))"

say "2. hypothesis — sequential zero write, no fallocate"
fresh_volume
start=$SECONDS
python3 - "$MNT/target.bin" "$ALLOC_SIZE" <<'PY'
import sys
path, size = sys.argv[1], sys.argv[2]
mult = {"K": 1024, "M": 1024**2, "G": 1024**3}
n = int(size[:-1]) * mult[size[-1].upper()] if size[-1].isalpha() else int(size)
block = bytes(4 * 1024 * 1024)
with open(path, "wb") as f:
    written = 0
    while written < n:
        chunk = min(len(block), n - written)
        f.write(block[:chunk])
        written += chunk
    f.flush()
    import os
    os.fsync(f.fileno())
PY
sync
measure "seqwrite" "$((SECONDS - start))"

say "3. fallocate then overwrite with zeros"
fresh_volume
start=$SECONDS
fallocate -l "$ALLOC_SIZE" "$MNT/target.bin"
python3 - "$MNT/target.bin" <<'PY'
import os, sys
path = sys.argv[1]
size = os.path.getsize(path)
block = bytes(4 * 1024 * 1024)
with open(path, "r+b") as f:
    written = 0
    while written < size:
        chunk = min(len(block), size - written)
        f.write(block[:chunk])
        written += chunk
    f.flush()
    os.fsync(f.fileno())
PY
sync
measure "fallocate+w" "$((SECONDS - start))"

say "4. is the baseline gap the MFT zone?"
sudo umount "$MNT"
if command -v ntfsinfo >/dev/null; then
    ntfsinfo -m "$IMG" 2>/dev/null | grep -iE 'cluster size|volume size|mft|sectors' | head -20 \
        || echo "   ntfsinfo produced no usable output"
else
    echo "   ntfsinfo not installed; skipping"
fi

say "summary"
cat <<'EOF'
   Read the run counts above.

   If seqwrite is CONTIGUOUS and fallocate is not, the engine should write
   zeros sequentially and skip fallocate entirely: one pass allocates and
   zeroes, and the S1 zeroing requirement costs nothing extra.

   If neither is reliably contiguous, allocation on ntfs3 cannot be steered
   from userspace and the tool's honest position is to verify and refuse,
   with the operator freeing contiguous space first.

   If fallocate+w clears the unwritten flag, that flag is a sound post-write
   verification that the data region is initialized.
EOF

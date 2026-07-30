#!/usr/bin/env bash
#
# Phase 0 spike: does ntfs3's fallocate(2) leave stale cluster contents?
#
# WHY THIS MATTERS
#
# IODD devices map a file's extents once at mount time and stream sectors
# straight off the block device. They never ask the filesystem. NTFS tracks
# valid data length separately from allocated length, so a range that reads as
# zeros *through the filesystem* may hold arbitrary old bytes on disk.
#
# If fallocate alone leaves stale clusters, then a "blank" VHD created that way
# hands the guest whatever was previously on those sectors: wrong data, and a
# disclosure leak. The tool would have to zero the data region explicitly.
#
# THE EXPERIMENT
#
#   1. mkfs.ntfs on a loop-backed image (no partition table, so filesystem
#      offset 0 == image file offset 0 and FIEMAP offsets map directly)
#   2. fill the volume with a recognizable pattern, which also forces the
#      backing image to hold real bytes rather than sparse holes
#   3. delete the pattern file, freeing the clusters
#   4. fallocate a fresh file over that freed space
#   5. read it through the filesystem  -> expected to be zeros either way
#   6. unmount, read the BACKING IMAGE at the physical offsets FIEMAP reported
#
# Pattern found at step 6 => fallocate does NOT zero; zeroing must be default.
# Zeros found at step 6   => ntfs3 already zeroes; the write can be skipped.
#
# Needs sudo for mount/umount only. Everything else runs as you.
#
# Touches nothing outside its own temp directory. It never references a real
# block device.

set -euo pipefail

IMG_SIZE="${IMG_SIZE:-4G}"
ALLOC_SIZE="${ALLOC_SIZE:-1G}"
PATTERN="${PATTERN:-IODDSPIKEPATTERN}"
KEEP="${KEEP:-0}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIEMAP="$HERE/fiemap.py"

WORKDIR="$(mktemp -d /tmp/iodd-spike.XXXXXX)"
IMG="$WORKDIR/ntfs.img"
MNT="$WORKDIR/mnt"
RESULT="$WORKDIR/result.json"

cleanup() {
    if mountpoint -q "$MNT" 2>/dev/null; then
        sudo umount "$MNT" || true
    fi
    if [[ "$KEEP" == "1" ]]; then
        echo
        echo "artifacts kept in $WORKDIR"
    else
        rm -rf "$WORKDIR"
    fi
}
trap cleanup EXIT

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

for tool in mkntfs fallocate mountpoint python3; do
    command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 1; }
done

say "0. environment"
uname -r
grep -q ntfs3 /proc/filesystems || sudo modprobe ntfs3
echo "ntfs3 available: $(grep -c ntfs3 /proc/filesystems)"
echo "workdir: $WORKDIR"
echo "image: $IMG_SIZE   fallocate target: $ALLOC_SIZE   pattern: '$PATTERN'"

say "1. create and format loop image"
truncate -s "$IMG_SIZE" "$IMG"
mkntfs --quick --force --label iodd-spike "$IMG" >/dev/null
mkdir -p "$MNT"
sudo mount -t ntfs3 -o "loop,uid=$(id -u),gid=$(id -g)" "$IMG" "$MNT"
df -h "$MNT" | tail -1

say "2. fill the volume with the pattern"
python3 - "$MNT/pattern.bin" "$PATTERN" <<'PY'
import sys
path, pattern = sys.argv[1], sys.argv[2].encode()
block = (pattern * (1024 * 1024 // len(pattern) + 1))[: 1024 * 1024]
written = 0
try:
    with open(path, "wb") as f:
        while True:
            f.write(block)
            written += len(block)
            if written % (256 * 1024 * 1024) == 0:
                f.flush()
except OSError as exc:
    print(f"   stopped at {written} bytes ({written/1024**3:.2f} GiB): {exc.strerror}")
else:
    print(f"   wrote {written} bytes")
PY
sync

say "3. delete the pattern file, freeing those clusters"
rm -f "$MNT/pattern.bin"
sync
df -h "$MNT" | tail -1

say "4. fallocate a fresh file over the freed space"
if ! fallocate -l "$ALLOC_SIZE" "$MNT/target.bin"; then
    echo "!! fallocate(2) FAILED on ntfs3 — this is itself a finding" >&2
    echo "   (glibc posix_fallocate would fall back to zero-writing; musl would not)" >&2
    exit 3
fi
echo "   fallocate succeeded"
ls -l "$MNT/target.bin"

say "5. extent map (prototype of the Rust extents module)"
"$FIEMAP" "$MNT/target.bin"
"$FIEMAP" --json "$MNT/target.bin" > "$RESULT"

say "6. read the file THROUGH the filesystem"
python3 - "$MNT/target.bin" "$PATTERN" <<'PY'
import sys
path, pattern = sys.argv[1], sys.argv[2].encode()
with open(path, "rb") as f:
    head = f.read(1024 * 1024)
print(f"   first 1 MiB: all zeros = {head == bytes(len(head))}")
print(f"   pattern present via filesystem = {pattern in head}")
PY

say "7. unmount and read the BACKING IMAGE at the reported physical offsets"
sudo umount "$MNT"
sync

python3 - "$IMG" "$RESULT" "$PATTERN" <<'PY'
import json, sys
img, result_path, pattern = sys.argv[1], sys.argv[2], sys.argv[3].encode()
res = json.load(open(result_path))[0]

SAMPLE = 1024 * 1024
hits = zeros = other = 0
samples = []

with open(img, "rb") as f:
    for run in res["runs"]:
        base, length = run["physical"], run["length"]
        # sample the start, middle, and end of every run
        for label, off in (
            ("start", base),
            ("mid", base + length // 2),
            ("end", max(base, base + length - SAMPLE)),
        ):
            f.seek(off)
            buf = f.read(min(SAMPLE, length))
            if not buf:
                continue
            if pattern in buf:
                hits += 1
                verdict = "PATTERN"
            elif buf == bytes(len(buf)):
                zeros += 1
                verdict = "zeros"
            else:
                other += 1
                verdict = f"other (first 16: {buf[:16].hex()})"
            samples.append((label, off, len(buf), verdict))

for label, off, n, verdict in samples:
    print(f"   {label:>5} @ {off:<14} {n:>9} bytes -> {verdict}")

print()
print(f"   samples with pattern: {hits}   zeros: {zeros}   other: {other}")
print()
if hits:
    print("   VERDICT: fallocate does NOT zero. Stale cluster contents survive.")
    print("            The tool MUST zero the data region explicitly.")
elif zeros and not other:
    print("   VERDICT: the allocated region reads as zeros on the raw device.")
    print("            ntfs3 appears to zero on allocation; explicit zeroing")
    print("            may be skippable. Re-run with a larger ALLOC_SIZE to")
    print("            confirm before relying on this.")
else:
    print("   VERDICT: inconclusive — neither pattern nor clean zeros.")
    print("            Inspect the samples above by hand.")
PY

say "done"
echo "unwritten flag seen: $(python3 -c "import json;print(json.load(open('$RESULT'))[0]['any_unwritten'])")"
echo "merged runs: $(python3 -c "import json;print(json.load(open('$RESULT'))[0]['merged_runs'])")"

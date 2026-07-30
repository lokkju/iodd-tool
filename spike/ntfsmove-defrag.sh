#!/usr/bin/env bash
#
# Quick spike: can any Linux-native tool defragment a file on NTFS?
#
# BACKGROUND
#
# S7 established that ntfs3's allocator cannot be steered from userspace, so
# the tool can verify contiguity but not produce it. That conclusion assumed
# no Linux tool can relocate NTFS clusters. This tests that assumption.
#
# THE ONLY CANDIDATE
#
# ntfsmove, shipped in ntfs-3g. It has exactly the right primitives:
#
#     -S  move to the start of the volume
#     -B  move to the best place on the volume
#     -E  move to the end of the volume
#     -C  move to a specific cluster offset
#     -n  dry run
#
# Caveats known before running:
#
#   * it takes a DEVICE, not a mounted path, so the volume must be unmounted
#   * it marks the volume dirty unless -D, meaning Windows chkdsk is needed
#     afterwards, which partly defeats the point of avoiding Windows
#   * it has no man page in this distribution, and the upstream ntfsprogs
#     documentation does not list it at all, which suggests it was never
#     finished
#   * ntfsdefrag does not exist despite what search results claim
#
# WHAT THIS DOES
#
#   1. build a volume with fragmented free space
#   2. create a target file that lands in several runs
#   3. unmount
#   4. ntfsmove -n  (dry run first, to see what it claims)
#   5. ntfsmove -B  (for real, best-place)
#   6. remount and re-measure the extent map
#
# A drop to one run means Linux-native defrag is possible and the tool's
# claim can widen. Anything else confirms verify-and-refuse.
#
# Operates only on its own scratch image. Never touches a real device.

set -euo pipefail

IMG_SIZE="${IMG_SIZE:-4G}"
ALLOC_SIZE="${ALLOC_SIZE:-512M}"
FRAG_FILES="${FRAG_FILES:-16}"
KEEP="${KEEP:-0}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIEMAP="$HERE/fiemap.py"

WORKDIR="$(mktemp -d /tmp/iodd-ntfsmove.XXXXXX)"
IMG="$WORKDIR/ntfs.img"
MNT="$WORKDIR/mnt"

cleanup() {
    mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT" || true
    [[ "$KEEP" == "1" ]] && { echo; echo "artifacts kept in $WORKDIR"; } || rm -rf "$WORKDIR"
}
trap cleanup EXIT

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

command -v ntfsmove >/dev/null || { echo "ntfsmove not installed (apt install ntfs-3g)" >&2; exit 1; }
for tool in mkntfs mountpoint python3; do
    command -v "$tool" >/dev/null || { echo "missing: $tool" >&2; exit 1; }
done

mkdir -p "$MNT"

say "0. environment"
uname -r
ntfsmove --version 2>&1 | head -3
echo "image: $IMG_SIZE   target: $ALLOC_SIZE"

say "1. build a volume with fragmented free space"
truncate -s "$IMG_SIZE" "$IMG"
mkntfs --quick --force --label iodd-move "$IMG" >/dev/null 2>&1
sudo mount -t ntfs3 -o "loop,uid=$(id -u),gid=$(id -g)" "$IMG" "$MNT"

python3 - "$MNT" "$FRAG_FILES" <<'PY' >/dev/null
import os, sys
mnt, n = sys.argv[1], int(sys.argv[2])
st = os.statvfs(mnt)
each = int(st.f_bavail * st.f_frsize * 0.95) // n
block = bytes(4 * 1024 * 1024)
for i in range(n):
    try:
        with open(os.path.join(mnt, f"f{i:03d}.bin"), "wb") as f:
            w = 0
            while w < each:
                c = min(len(block), each - w)
                f.write(block[:c]); w += c
    except OSError:
        break
PY
sync
i=0
for f in "$MNT"/f*.bin; do
    [[ -e "$f" ]] || continue
    (( i % 2 == 1 )) && rm -f "$f"
    i=$((i + 1))
done
sync
df -h "$MNT" | tail -1 | sed 's/^/   /'

say "2. create the target file"
python3 - "$MNT/target.bin" "$ALLOC_SIZE" <<'PY'
import os, sys
mult = {"K": 1024, "M": 1024**2, "G": 1024**3}
s = sys.argv[2]
n = int(s[:-1]) * mult[s[-1].upper()] if s[-1].isalpha() else int(s)
block = bytes(4 * 1024 * 1024)
with open(sys.argv[1], "wb") as f:
    w = 0
    while w < n:
        c = min(len(block), n - w)
        f.write(block[:c]); w += c
    f.flush(); os.fsync(f.fileno())
PY
sync

say "3. extent map BEFORE"
"$FIEMAP" "$MNT/target.bin"
BEFORE=$("$FIEMAP" --json "$MNT/target.bin" | python3 -c "import json,sys;print(json.load(sys.stdin)[0]['merged_runs'])")
echo "   runs before: $BEFORE"

say "4. unmount (ntfsmove needs the volume offline)"
sudo umount "$MNT"
sync

say "5. ntfsmove dry run (-n -B)"
set +e
ntfsmove -n -B "$IMG" /target.bin 2>&1 | head -30
echo "   dry-run exit: $?"
set -e

say "6. ntfsmove for real (-B)"
set +e
ntfsmove -B "$IMG" /target.bin 2>&1 | head -30
MOVE_RC=$?
echo "   exit: $MOVE_RC"
set -e

say "7. extent map AFTER"
sudo mount -t ntfs3 -o "loop,uid=$(id -u),gid=$(id -g)" "$IMG" "$MNT"
if [[ -f "$MNT/target.bin" ]]; then
    "$FIEMAP" "$MNT/target.bin"
    AFTER=$("$FIEMAP" --json "$MNT/target.bin" | python3 -c "import json,sys;print(json.load(sys.stdin)[0]['merged_runs'])")
else
    AFTER="FILE GONE"
fi

say "verdict"
echo "   runs before: $BEFORE"
echo "   runs after:  $AFTER"
echo
if [[ "$AFTER" == "FILE GONE" ]]; then
    echo "   ntfsmove DESTROYED the file. Do not use it."
elif [[ "$AFTER" == "1" && "$BEFORE" != "1" ]]; then
    echo "   ntfsmove DEFRAGMENTED the file. Linux-native defrag is possible."
    echo "   Caveats still apply: offline volume, and the volume is now marked"
    echo "   dirty unless -D was used, so Windows chkdsk is expected."
elif [[ "$AFTER" == "$BEFORE" ]]; then
    echo "   ntfsmove changed nothing. Verify-and-refuse stands."
else
    echo "   ntfsmove changed the layout but did not reach one run."
fi
echo
echo "   Data integrity check (file should still read as zeros):"
if [[ -f "$MNT/target.bin" ]]; then
    python3 -c "
d = open('$MNT/target.bin','rb').read(1024*1024)
print('     first 1 MiB all zeros:', d == bytes(len(d)))
print('     size on disk:', __import__('os').path.getsize('$MNT/target.bin'))
"
fi

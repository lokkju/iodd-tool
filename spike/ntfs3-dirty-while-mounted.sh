#!/usr/bin/env bash
#
# S11. Does ntfs3 hold the volume-dirty flag for the duration of a read-write
# mount, or does it only set it on error?
#
# This decides whether `iodd doctor` can usefully report a dirty volume it is
# currently scanning. ntfs-3g sets the flag on rw mount and clears it on clean
# unmount, by design — that is how it signals "unclean shutdown" to Windows.
# If ntfs3 does the same, then every healthy read-write mount reads as dirty,
# the warning fires constantly, and the check is only meaningful against an
# unmounted volume.
#
# Circumstantial evidence says ntfs3 is error-triggered, not mount-triggered:
# the kernel message we captured was "Mark volume as dirty due to NTFS
# errors", and the IODD volume does remount after ordinary use. But that is
# consistent with set-on-mount/clear-on-unmount too, so it has to be measured.
#
# Needs sudo (losetup + mount). Touches nothing outside its own temp dir.
set -u

REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO/target/debug/ntfs-contig"
WORK="$(mktemp -d)"
IMG="$WORK/vol.ntfs"
MNT="$WORK/mnt"
LOOP=""

cleanup() {
  mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
  [ -n "$LOOP" ] && sudo losetup -d "$LOOP" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

command -v mkntfs >/dev/null || { echo "need ntfsprogs (mkntfs)"; exit 1; }
[ -x "$BIN" ] || { echo "build first: cargo build -p ntfs-contig"; exit 1; }

flags() {
  # Read straight off the loop device. Works regardless of mount state,
  # which is the whole point.
  sudo "$BIN" volume "$LOOP" --report-only 2>/dev/null \
    | grep -E '^\s+flags' | sed 's/^ *//' || echo "flags        <unreadable>"
}

echo "=== 0. build a clean 64 MiB NTFS volume ==="
truncate -s 64M "$IMG"
mkntfs -F -q -L SPIKE11 "$IMG" >/dev/null 2>&1 || { echo "mkntfs failed"; exit 1; }
mkdir -p "$MNT"
LOOP="$(sudo losetup --show -f "$IMG")"
echo "loop device: $LOOP"
echo

echo "=== 1. unmounted baseline ==="
flags
echo

echo "=== 2. mounted read-write with ntfs3 ==="
if ! sudo mount -t ntfs3 -o rw "$LOOP" "$MNT" 2>"$WORK/mounterr"; then
  echo "  ntfs3 mount failed:"; sed 's/^/    /' "$WORK/mounterr"
  echo "  (kernel may lack ntfs3; the ntfs-3g comparison below still runs)"
else
  echo "  mounted."
  flags
  echo
  echo "=== 3. after writing a file and syncing ==="
  echo hello | sudo tee "$MNT/probe.txt" >/dev/null
  sync
  flags
  echo
  echo "=== 4. after clean unmount ==="
  sudo umount "$MNT"
  flags
fi
echo

echo '=== 4b. iodd doctor pointed straight at the device ==='
# The one code path that cannot be covered without a real block device.
if [ -x "$REPO/target/debug/iodd" ]; then
  sudo "$REPO/target/debug/iodd" doctor "$LOOP"
  echo "  exit=$?  (0 clean, 4 dirty)"
else
  echo "  build first: cargo build -p iodd"
fi
echo

echo "=== 5. comparison: ntfs-3g, which is known to set-on-mount ==="
if command -v ntfs-3g >/dev/null; then
  if sudo ntfs-3g "$LOOP" "$MNT" 2>/dev/null; then
    echo "  mounted rw via FUSE."
    flags
    sudo umount "$MNT"
    echo "  after clean unmount:"
    flags
  else
    echo "  ntfs-3g mount failed"
  fi
else
  echo "  ntfs-3g not installed"
fi
echo

cat <<'EOF'
=== how to read this ===

If step 2/3 shows flags 0x0001 (dirty) and step 4 shows 0x0000, ntfs3 holds
the flag for the duration of a rw mount. `doctor` must then NOT warn about a
volume it is currently scanning read-write; the check is only meaningful on
an unmounted device.

If steps 2/3 stay 0x0000, ntfs3 sets the flag only on error, and `doctor` can
warn while mounted — which is the more useful behaviour, because it catches
the problem before you unplug rather than after.

Step 5 is the control: ntfs-3g is expected to show dirty while mounted.
EOF

#!/usr/bin/env bash
#
# S11b. The positive control S11 lacked.
#
# S11 showed the on-disk dirty flag stays 0x0000 through a healthy ntfs3
# read-write mount, which is the answer `doctor` needed. But its intended
# control — ntfs-3g, assumed to set the flag on mount — also read 0x0000, so
# that run never demonstrated the reading could detect a flag set by a driver
# at all, as opposed to one we patched in ourselves.
#
# This reproduces the failure we actually care about. The IODD drops the volume
# off the USB bus when it switches modes and then brings it back; a dm-error
# target does the same thing to a mounted filesystem, on purpose and
# reversibly:
#
#     linear -> error   the device starts failing I/O, mid-mount
#     error  -> linear  the device comes back
#
# Two questions come out of it:
#
#   1. Does ntfs3 set the on-disk flag at all when it hits I/O errors?
#   2. If the write fails while the device is gone (which is exactly what
#      "ino=3, ntfs_set_state failed, -5" records), does it retry once the
#      device returns — or is the flag lost, leaving a volume that refuses to
#      mount for some other reason?
#
# Needs sudo (losetup, dmsetup, mount). Everything is confined to a temp dir
# and one dm device named below; nothing touches a real disk.
set -u

REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO/target/debug/ntfs-contig"
WORK="$(mktemp -d)"
IMG="$WORK/vol.ntfs"
MNT="$WORK/mnt"
DM="iodd-spike11b"
LOOP=""
SIZE=""

cleanup() {
  mountpoint -q "$MNT" 2>/dev/null && sudo umount -l "$MNT" 2>/dev/null
  sudo dmsetup remove "$DM" 2>/dev/null
  [ -n "$LOOP" ] && sudo losetup -d "$LOOP" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

for t in mkntfs dmsetup losetup; do
  command -v "$t" >/dev/null || { echo "need $t"; exit 1; }
done
[ -x "$BIN" ] || { echo "build first: cargo build -p ntfs-contig"; exit 1; }

# Read flags off the *backing image*, not through the dm device, so the answer
# does not depend on whichever mapping is live at the time.
flags() {
  sudo "$BIN" volume "$IMG" --report-only 2>/dev/null \
    | grep -E '^\s+flags' | sed 's/^ */  /' || echo "  flags        <unreadable>"
}

linear() { echo "0 $SIZE linear $LOOP 0"; }
error()  { echo "0 $SIZE error"; }

swap_table() {
  sudo dmsetup suspend "$DM"
  echo "$1" | sudo dmsetup load "$DM"
  sudo dmsetup resume "$DM"
}

echo "=== 0. clean 64 MiB NTFS volume on a dm-linear device ==="
truncate -s 64M "$IMG"
mkntfs -F -q -L SPIKE11B "$IMG" >/dev/null 2>&1 || { echo "mkntfs failed"; exit 1; }
mkdir -p "$MNT"
LOOP="$(sudo losetup --show -f "$IMG")"
SIZE="$(sudo blockdev --getsz "$LOOP")"
sudo dmsetup remove "$DM" 2>/dev/null
linear | sudo dmsetup create "$DM" || { echo "dmsetup create failed"; exit 1; }
echo "loop=$LOOP  dm=/dev/mapper/$DM  sectors=$SIZE"
flags
echo

echo "=== 1. mount rw and write, while the device is healthy ==="
if ! sudo mount -t ntfs3 -o rw "/dev/mapper/$DM" "$MNT" 2>"$WORK/e"; then
  echo "  ntfs3 mount failed:"; sed 's/^/    /' "$WORK/e"; exit 1
fi
echo hello | sudo tee "$MNT/before.txt" >/dev/null
sync
echo "  wrote before.txt"
flags
echo

echo "=== 2. yank the device: swap in a dm-error target ==="
swap_table "$(error)"
echo "  all I/O to /dev/mapper/$DM now fails"
# Force the driver to notice. These are expected to fail.
echo bang | sudo tee "$MNT/during.txt" >/dev/null 2>&1
sudo ls "$MNT" >/dev/null 2>&1
sync 2>/dev/null
sleep 1
echo "  kernel says:"
sudo dmesg 2>/dev/null | grep -i ntfs3 | tail -5 | sed 's/^/    /' \
  || echo "    (no ntfs3 messages; try journalctl -k)"
echo "  flags on the backing image while the device is gone:"
flags
echo

echo "=== 3. bring the device back ==="
swap_table "$(linear)"
echo "  I/O restored"
sync 2>/dev/null
sleep 1
flags
echo

echo "=== 4. unmount, then read again ==="
sudo umount "$MNT" 2>"$WORK/e" || { echo "  clean umount failed:"; sed 's/^/    /' "$WORK/e"; sudo umount -l "$MNT" 2>/dev/null; }
sync
flags
echo "  kernel says:"
sudo dmesg 2>/dev/null | grep -i ntfs3 | tail -5 | sed 's/^/    /'
echo

echo "=== 5. can it be mounted again? ==="
if sudo mount -t ntfs3 -o ro "/dev/mapper/$DM" "$MNT" 2>"$WORK/e"; then
  echo "  yes, mounts read-only"
  sudo umount "$MNT"
else
  echo "  no:"; sed 's/^/    /' "$WORK/e"
fi
echo

echo "=== 6. what iodd doctor says about it ==="
if [ -x "$REPO/target/debug/iodd" ]; then
  sudo "$REPO/target/debug/iodd" doctor "/dev/mapper/$DM"
  echo "  exit=$?  (0 clean, 4 dirty)"
fi
echo

cat <<'EOF'
=== how to read this ===

The question is whether the on-disk flag ever gets set by the driver, not by
us patching bytes.

- Flags go 0x0001 at step 3 or 4: ntfs3 does record it once the device is
  reachable again, the reading detects a driver-set flag, and `iodd doctor`
  genuinely diagnoses this failure. S11's clean result is then trustworthy.

- Flags stay 0x0000 throughout but step 5 refuses to mount: the volume is
  unmountable for a reason that is NOT the on-disk dirty flag. The kernel
  message from acceptance — "ino=3, ntfs_set_state failed, -5" — says the
  write to $Volume errored, so this outcome is entirely plausible. It would
  mean the dirty-flag check, while correct, does not explain the IODD failure,
  and the real cause needs finding.

- Flags stay 0x0000 and step 5 mounts fine: the error injection was not severe
  enough to make ntfs3 give up. Inconclusive; make the outage longer or write
  considerably more during it.

Either way, note whether step 2 shows "ntfs_set_state failed" — that is the
same message the real device produced, and seeing it here means this is a
faithful reproduction rather than an analogy.
EOF

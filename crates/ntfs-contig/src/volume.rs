//! Reading NTFS volume state directly from a block device.
//!
//! # Why this exists
//!
//! The IODD drops the volume off the USB bus when it switches between
//! presenting the disk and presenting a virtual drive. If the host still has
//! it mounted read-write, the driver is mid-write, takes an I/O error, and
//! sets the volume-dirty flag. The volume then refuses to mount until
//! `chkdsk` or `ntfsfix -d` clears it — and the only trace is a kernel
//! message that has usually scrolled away by the time anyone looks.
//!
//! Nothing in the mounted-filesystem interface exposes that flag. ntfs3
//! publishes nothing under `/sys/fs`, and by the time it matters the volume
//! will not mount, so there is no path to interrogate. The flag lives in
//! filesystem metadata, so reading it means reading the device.
//!
//! # Scope
//!
//! Strictly read-only, and deliberately shallow: the boot sector, one MFT
//! record, and one attribute within it. This is not an NTFS implementation
//! and must not grow into one — it never writes, never follows a non-resident
//! attribute's run list, and treats anything it does not recognise as
//! "unreadable" rather than guessing.
//!
//! # `$Volume` exists twice
//!
//! NTFS mirrors the first four MFT records into `$MFTMirr`, and record 3 is
//! `$Volume` — so the flags read here are stored in two places. Reading is
//! unaffected: `$MFT` is authoritative and is what this module consults.
//!
//! A *writer* is a different matter. Changing one copy and not the other
//! leaves ntfsprogs reporting `$MFTMirr does not match $MFT (record 3)` and
//! refusing the volume as inconsistent rather than as dirty. Measured in
//! `tests/volume_real.rs`; worth knowing before anything in this project ever
//! writes NTFS metadata.
//!
//! # Layout
//!
//! [`parse_boot_sector`] and [`parse_volume_record`] are pure functions over
//! byte slices, so every branch below is exercised by unit tests over
//! synthetic records rather than requiring a real volume. [`read`] is the thin
//! I/O wrapper that composes them.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// `$Volume` is always MFT record 3.
const VOLUME_MFT_RECORD: u64 = 3;

/// `$VOLUME_NAME`.
const ATTR_VOLUME_NAME: u32 = 0x0000_0060;

/// `$VOLUME_INFORMATION`.
const ATTR_VOLUME_INFORMATION: u32 = 0x0000_0070;

/// Attribute-list terminator.
const ATTR_END: u32 = 0xFFFF_FFFF;

/// The volume was not unmounted cleanly, or the driver hit an error. Windows
/// runs `chkdsk` on next mount; Linux drivers generally refuse to mount at all.
pub const VOLUME_IS_DIRTY: u16 = 0x0001;
/// `$LogFile` should be resized on next mount.
pub const VOLUME_RESIZE_LOG_FILE: u16 = 0x0002;
/// The volume should be upgraded on next mount.
pub const VOLUME_UPGRADE_ON_MOUNT: u16 = 0x0004;
/// Last mounted by Windows NT 4.
pub const VOLUME_MOUNTED_ON_NT4: u16 = 0x0008;
/// A USN journal deletion was interrupted.
pub const VOLUME_DELETE_USN_UNDERWAY: u16 = 0x0010;
/// Object IDs need repair.
pub const VOLUME_REPAIR_OBJECT_ID: u16 = 0x0020;
/// `chkdsk` has modified the volume.
pub const VOLUME_MODIFIED_BY_CHKDSK: u16 = 0x8000;

/// Geometry read out of the NTFS boot sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootSector {
    pub bytes_per_sector: u32,
    pub cluster_size: u64,
    pub mft_lcn: u64,
    pub mft_record_size: u64,
    pub total_sectors: u64,
}

impl BootSector {
    /// Byte offset of an MFT record.
    #[must_use]
    pub fn mft_record_offset(&self, record: u64) -> Option<u64> {
        self.mft_lcn
            .checked_mul(self.cluster_size)?
            .checked_add(record.checked_mul(self.mft_record_size)?)
    }
}

/// `$VOLUME_INFORMATION`, plus the volume label if one is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// `None` when the volume has no label, which is normal.
    pub label: Option<String>,
    /// NTFS version major, e.g. 3 for the 3.1 written by every Windows since XP.
    pub major: u8,
    pub minor: u8,
    pub flags: u16,
}

impl VolumeInfo {
    /// The volume needs `chkdsk` or `ntfsfix -d` before it will mount again.
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.flags & VOLUME_IS_DIRTY != 0
    }

    /// Human-readable flag names, for diagnostics. Empty when nothing is set.
    #[must_use]
    pub fn flag_names(&self) -> Vec<&'static str> {
        const NAMES: [(u16, &str); 7] = [
            (VOLUME_IS_DIRTY, "dirty"),
            (VOLUME_RESIZE_LOG_FILE, "resize-logfile"),
            (VOLUME_UPGRADE_ON_MOUNT, "upgrade-on-mount"),
            (VOLUME_MOUNTED_ON_NT4, "mounted-on-nt4"),
            (VOLUME_DELETE_USN_UNDERWAY, "delete-usn-underway"),
            (VOLUME_REPAIR_OBJECT_ID, "repair-object-id"),
            (VOLUME_MODIFIED_BY_CHKDSK, "modified-by-chkdsk"),
        ];
        NAMES
            .iter()
            .filter(|(bit, _)| self.flags & bit != 0)
            .map(|(_, name)| *name)
            .collect()
    }
}

fn unreadable(path: &Path, reason: impl Into<String>) -> Error {
    Error::VolumeUnreadable {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    let raw: [u8; 2] = b.get(off..off.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(raw))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let raw: [u8; 4] = b.get(off..off.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    let raw: [u8; 8] = b.get(off..off.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(raw))
}

/// NTFS encodes "clusters per X" as a signed byte: positive is a count,
/// negative is a binary shift, used when the size exceeds one cluster.
fn sized_field(raw: u8, unit: u64) -> Option<u64> {
    #[allow(clippy::cast_possible_wrap)]
    let signed = raw as i8;
    if signed > 0 {
        unit.checked_mul(u64::from(raw))
    } else {
        let shift = u32::try_from(-i32::from(signed)).ok()?;
        (shift < 64).then(|| 1u64 << shift)
    }
}

/// Parse the NTFS boot sector, the first 512 bytes of the partition.
///
/// # Errors
///
/// [`Error::VolumeUnreadable`] when the OEM identifier is absent (the volume
/// is not NTFS) or the geometry is not self-consistent.
pub fn parse_boot_sector(boot: &[u8], path: &Path) -> Result<BootSector> {
    if boot.get(3..11) != Some(b"NTFS    ") {
        return Err(unreadable(path, "no NTFS signature in the boot sector"));
    }

    let bytes_per_sector =
        u32::from(u16_at(boot, 0x0B).ok_or_else(|| unreadable(path, "truncated boot sector"))?);
    if !(256..=8192).contains(&bytes_per_sector) || !bytes_per_sector.is_power_of_two() {
        return Err(unreadable(
            path,
            format!("implausible sector size {bytes_per_sector}"),
        ));
    }

    let spc = u8_at(boot, 0x0D).ok_or_else(|| unreadable(path, "truncated boot sector"))?;
    let cluster_size = sized_field(spc, u64::from(bytes_per_sector))
        .ok_or_else(|| unreadable(path, "implausible cluster size"))?;
    if cluster_size == 0 || cluster_size > 2 * 1024 * 1024 || !cluster_size.is_power_of_two() {
        return Err(unreadable(
            path,
            format!("implausible cluster size {cluster_size}"),
        ));
    }

    let total_sectors = u64_at(boot, 0x28).ok_or_else(|| unreadable(path, "truncated"))?;
    let mft_lcn = u64_at(boot, 0x30).ok_or_else(|| unreadable(path, "truncated"))?;

    let cpr = u8_at(boot, 0x40).ok_or_else(|| unreadable(path, "truncated boot sector"))?;
    let mft_record_size =
        sized_field(cpr, cluster_size).ok_or_else(|| unreadable(path, "implausible MFT record"))?;
    // 1024 in every volume anyone will meet, but the field is authoritative.
    if !(256..=65536).contains(&mft_record_size) || !mft_record_size.is_power_of_two() {
        return Err(unreadable(
            path,
            format!("implausible MFT record size {mft_record_size}"),
        ));
    }

    Ok(BootSector {
        bytes_per_sector,
        cluster_size,
        mft_lcn,
        mft_record_size,
        total_sectors,
    })
}

/// Reverse the update-sequence-array substitution, in place.
///
/// NTFS overwrites the last two bytes of every sector in a metadata record
/// with a sequence number, stashing the real bytes in an array in the record
/// header. If a sector was written but its neighbours were not, the numbers
/// disagree and the record is known to be torn. Undoing this is mandatory
/// before reading any field that might straddle a sector boundary.
fn apply_fixups(record: &mut [u8], bytes_per_sector: u32, path: &Path) -> Result<()> {
    let usa_offset =
        usize::from(u16_at(record, 0x04).ok_or_else(|| unreadable(path, "truncated MFT record"))?);
    let usa_count =
        usize::from(u16_at(record, 0x06).ok_or_else(|| unreadable(path, "truncated MFT record"))?);
    if usa_count == 0 {
        return Err(unreadable(path, "MFT record has no update sequence array"));
    }
    let sector = bytes_per_sector as usize;
    if (usa_count - 1).checked_mul(sector) != Some(record.len()) {
        return Err(unreadable(
            path,
            "update sequence array does not span the record",
        ));
    }

    let usn = u16_at(record, usa_offset).ok_or_else(|| unreadable(path, "USA outside record"))?;

    for i in 1..usa_count {
        let entry = u16_at(record, usa_offset + i * 2)
            .ok_or_else(|| unreadable(path, "USA entry outside record"))?;
        let tail = i * sector - 2;
        let present =
            u16_at(record, tail).ok_or_else(|| unreadable(path, "sector tail outside record"))?;
        if present != usn {
            return Err(unreadable(
                path,
                format!("torn MFT record: sector {i} was not written with the rest"),
            ));
        }
        record
            .get_mut(tail..tail + 2)
            .ok_or_else(|| unreadable(path, "sector tail outside record"))?
            .copy_from_slice(&entry.to_le_bytes());
    }
    Ok(())
}

/// Decode a resident `$VOLUME_NAME` value: UTF-16LE, not NUL-terminated.
fn decode_label(value: &[u8]) -> Option<String> {
    if value.is_empty() || value.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = value
        .chunks_exact(2)
        .filter_map(|c| Some(u16::from_le_bytes(c.try_into().ok()?)))
        .collect();
    let s = String::from_utf16_lossy(&units);
    let trimmed = s.trim_end_matches('\0').to_owned();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Parse the `$Volume` MFT record. Applies fixups to `record` in place.
///
/// # Errors
///
/// [`Error::VolumeUnreadable`] when the record is not a FILE record, is torn,
/// or carries no resident `$VOLUME_INFORMATION`.
pub fn parse_volume_record(
    record: &mut [u8],
    bytes_per_sector: u32,
    path: &Path,
) -> Result<VolumeInfo> {
    if record.get(0..4) != Some(b"FILE") {
        return Err(unreadable(path, "$Volume is not a FILE record"));
    }
    apply_fixups(record, bytes_per_sector, path)?;

    let first = usize::from(u16_at(record, 0x14).ok_or_else(|| unreadable(path, "truncated"))?);
    let used = u32_at(record, 0x18).ok_or_else(|| unreadable(path, "truncated"))? as usize;
    let end = used.min(record.len());

    let mut label = None;
    let mut info: Option<(u8, u8, u16)> = None;
    let mut off = first;

    while off < end {
        let ty = u32_at(record, off).ok_or_else(|| unreadable(path, "attribute past record"))?;
        if ty == ATTR_END {
            break;
        }
        let len = u32_at(record, off + 4).ok_or_else(|| unreadable(path, "attribute truncated"))?
            as usize;
        // A zero length would spin forever; treat it as corruption.
        if len == 0 || len > end - off {
            return Err(unreadable(path, "attribute has an implausible length"));
        }

        // Only resident attributes are read. $VOLUME_INFORMATION is always
        // resident, and a non-resident $VOLUME_NAME would mean following a
        // run list, which is out of scope.
        let resident = u8_at(record, off + 8) == Some(0);
        if resident && (ty == ATTR_VOLUME_NAME || ty == ATTR_VOLUME_INFORMATION) {
            let vlen = u32_at(record, off + 0x10)
                .ok_or_else(|| unreadable(path, "attribute truncated"))?
                as usize;
            let voff = usize::from(
                u16_at(record, off + 0x14).ok_or_else(|| unreadable(path, "attribute truncated"))?,
            );
            let value = record
                .get(off + voff..off + voff + vlen)
                .ok_or_else(|| unreadable(path, "attribute value outside record"))?;

            if ty == ATTR_VOLUME_NAME {
                label = decode_label(value);
            } else {
                let major =
                    u8_at(value, 8).ok_or_else(|| unreadable(path, "$VOLUME_INFORMATION short"))?;
                let minor =
                    u8_at(value, 9).ok_or_else(|| unreadable(path, "$VOLUME_INFORMATION short"))?;
                let flags = u16_at(value, 10)
                    .ok_or_else(|| unreadable(path, "$VOLUME_INFORMATION short"))?;
                info = Some((major, minor, flags));
            }
        }
        off += len;
    }

    let (major, minor, flags) =
        info.ok_or_else(|| unreadable(path, "no resident $VOLUME_INFORMATION in $Volume"))?;
    Ok(VolumeInfo {
        label,
        major,
        minor,
        flags,
    })
}

/// Read volume state from a block device or image file.
///
/// Requires read access to the device, which usually means root or the `disk`
/// group. Callers treat failure as "unknown" rather than as a fault: this is
/// a diagnostic, and an audit that cannot open `/dev/sdb1` is still a useful
/// audit.
///
/// # Errors
///
/// [`Error::Io`] if the device cannot be opened or read, and
/// [`Error::VolumeUnreadable`] if what is there is not a legible NTFS volume.
pub fn read(dev: &Path) -> Result<VolumeInfo> {
    use std::os::unix::fs::FileExt;

    let file = crate::fsinfo::open_readonly_nofollow(dev)?;
    let io = |source| Error::Io {
        path: dev.to_path_buf(),
        source,
    };

    let mut boot = [0u8; 512];
    file.read_exact_at(&mut boot, 0).map_err(io)?;
    let bs = parse_boot_sector(&boot, dev)?;

    let offset = bs
        .mft_record_offset(VOLUME_MFT_RECORD)
        .ok_or_else(|| unreadable(dev, "MFT offset overflows"))?;
    let size = usize::try_from(bs.mft_record_size)
        .map_err(|_| unreadable(dev, "MFT record larger than addressable memory"))?;

    let mut record = vec![0u8; size];
    file.read_exact_at(&mut record, offset).map_err(io)?;
    parse_volume_record(&mut record, bs.bytes_per_sector, dev)
}

/// The block device backing the filesystem that `path` lives on.
///
/// Resolved through `/sys/dev/block/<major>:<minor>`, so it reflects what the
/// kernel actually has mounted rather than whatever `/proc/mounts` was told.
/// `None` for anything not backed by a real block device — tmpfs, overlayfs,
/// a loopback whose device node is gone.
#[must_use]
pub fn backing_device(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let dev = std::fs::metadata(path).ok()?.dev();
    let link = std::fs::read_link(format!(
        "/sys/dev/block/{}:{}",
        libc::major(dev),
        libc::minor(dev)
    ))
    .ok()?;
    let node = Path::new("/dev").join(link.file_name()?);
    node.exists().then_some(node)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "/dev/test";

    fn path() -> &'static Path {
        Path::new(DEV)
    }

    /// A minimal but structurally valid NTFS boot sector.
    fn boot_sector(spc: u8, clusters_per_mft_record: u8) -> [u8; 512] {
        let mut b = [0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        b[0x0D] = spc;
        b[0x28..0x30].copy_from_slice(&1_000_000u64.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        b[0x40] = clusters_per_mft_record;
        b
    }

    /// A `$Volume` MFT record carrying the given flags and label, with the
    /// update sequence array applied as NTFS would write it.
    fn volume_record(flags: u16, label: Option<&str>) -> Vec<u8> {
        const SECTOR: usize = 512;
        const SIZE: usize = 1024;
        let mut r = vec![0u8; SIZE];
        r[0..4].copy_from_slice(b"FILE");
        let usa_offset = 0x30u16;
        // One entry for the USN itself, plus one per sector.
        let usa_count = 1 + (SIZE / SECTOR) as u16;
        r[0x04..0x06].copy_from_slice(&usa_offset.to_le_bytes());
        r[0x06..0x08].copy_from_slice(&usa_count.to_le_bytes());
        let first_attr = 0x38u16;
        r[0x14..0x16].copy_from_slice(&first_attr.to_le_bytes());

        let mut off = usize::from(first_attr);

        if let Some(name) = label {
            let utf16: Vec<u8> = name
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<u8>>();
            let vlen = utf16.len();
            let alen = 0x18 + vlen.next_multiple_of(8);
            r[off..off + 4].copy_from_slice(&ATTR_VOLUME_NAME.to_le_bytes());
            r[off + 4..off + 8].copy_from_slice(&(alen as u32).to_le_bytes());
            r[off + 8] = 0; // resident
            r[off + 0x10..off + 0x14].copy_from_slice(&(vlen as u32).to_le_bytes());
            r[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
            r[off + 0x18..off + 0x18 + vlen].copy_from_slice(&utf16);
            off += alen;
        }

        // $VOLUME_INFORMATION: 12-byte value, flags at offset 10.
        let vlen = 12usize;
        let alen = 0x18 + 16;
        r[off..off + 4].copy_from_slice(&ATTR_VOLUME_INFORMATION.to_le_bytes());
        r[off + 4..off + 8].copy_from_slice(&(alen as u32).to_le_bytes());
        r[off + 8] = 0;
        r[off + 0x10..off + 0x14].copy_from_slice(&(vlen as u32).to_le_bytes());
        r[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        r[off + 0x18 + 8] = 3; // major
        r[off + 0x18 + 9] = 1; // minor
        r[off + 0x18 + 10..off + 0x18 + 12].copy_from_slice(&flags.to_le_bytes());
        off += alen;

        r[off..off + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        let used = (off + 8) as u32;
        r[0x18..0x1C].copy_from_slice(&used.to_le_bytes());

        // Now perform the substitution NTFS performs on write: stash each
        // sector's last two bytes in the USA and stamp the USN in their place.
        let usn = 0x1234u16;
        let uo = usize::from(usa_offset);
        r[uo..uo + 2].copy_from_slice(&usn.to_le_bytes());
        for i in 1..usize::from(usa_count) {
            let tail = i * SECTOR - 2;
            let real = [r[tail], r[tail + 1]];
            r[uo + i * 2..uo + i * 2 + 2].copy_from_slice(&real);
            r[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
        }
        r
    }

    #[test]
    fn boot_sector_yields_standard_geometry() {
        // 8 sectors/cluster = 4 KiB clusters; -10 => 1024-byte MFT records.
        let bs = parse_boot_sector(&boot_sector(8, 0xF6), path()).expect("parses");
        assert_eq!(bs.bytes_per_sector, 512);
        assert_eq!(bs.cluster_size, 4096);
        assert_eq!(bs.mft_record_size, 1024);
        assert_eq!(bs.mft_lcn, 4);
    }

    #[test]
    fn mft_record_size_can_be_a_cluster_count() {
        // Positive means "this many clusters", used when clusters are small.
        let bs = parse_boot_sector(&boot_sector(1, 2), path()).expect("parses");
        assert_eq!(bs.cluster_size, 512);
        assert_eq!(bs.mft_record_size, 1024);
    }

    #[test]
    fn large_clusters_use_the_shift_encoding() {
        // 0xF1 = -15 => 32 KiB clusters, beyond what a byte count could hold.
        let bs = parse_boot_sector(&boot_sector(0xF1, 0xF6), path()).expect("parses");
        assert_eq!(bs.cluster_size, 32768);
    }

    #[test]
    fn a_non_ntfs_volume_is_rejected() {
        let mut b = boot_sector(8, 0xF6);
        b[3..11].copy_from_slice(b"MSDOS5.0");
        let err = parse_boot_sector(&b, path()).expect_err("not NTFS");
        assert!(err.to_string().contains("no NTFS signature"), "{err}");
    }

    #[test]
    fn an_implausible_sector_size_is_rejected() {
        let mut b = boot_sector(8, 0xF6);
        b[0x0B..0x0D].copy_from_slice(&777u16.to_le_bytes());
        let err = parse_boot_sector(&b, path()).expect_err("not a power of two");
        assert!(err.to_string().contains("sector size"), "{err}");
    }

    #[test]
    fn a_truncated_boot_sector_is_rejected() {
        assert!(parse_boot_sector(&[0u8; 4], path()).is_err());
    }

    #[test]
    fn a_clean_volume_reads_as_clean() {
        let mut r = volume_record(0, Some("IODD"));
        let info = parse_volume_record(&mut r, 512, path()).expect("parses");
        assert!(!info.is_dirty());
        assert_eq!(info.label.as_deref(), Some("IODD"));
        assert_eq!((info.major, info.minor), (3, 1));
        assert!(info.flag_names().is_empty());
    }

    #[test]
    fn the_dirty_flag_is_read() {
        let mut r = volume_record(VOLUME_IS_DIRTY, Some("IODD"));
        let info = parse_volume_record(&mut r, 512, path()).expect("parses");
        assert!(info.is_dirty());
        assert_eq!(info.flag_names(), vec!["dirty"]);
    }

    #[test]
    fn several_flags_are_reported_together() {
        let mut r = volume_record(VOLUME_IS_DIRTY | VOLUME_MODIFIED_BY_CHKDSK, None);
        let info = parse_volume_record(&mut r, 512, path()).expect("parses");
        assert!(info.is_dirty());
        assert_eq!(info.flag_names(), vec!["dirty", "modified-by-chkdsk"]);
    }

    #[test]
    fn a_volume_without_a_label_is_not_an_error() {
        let mut r = volume_record(0, None);
        let info = parse_volume_record(&mut r, 512, path()).expect("parses");
        assert_eq!(info.label, None);
    }

    #[test]
    fn fixups_are_reversed_rather_than_read_literally() {
        // Put recognisable bytes at the end of the first sector; after the
        // substitution the record holds the USN there, and parsing must
        // restore the original. If fixups were skipped the value would be the
        // USN, which is exactly the class of bug this guards.
        let mut r = volume_record(VOLUME_IS_DIRTY, None);
        assert_eq!(
            u16_at(&r, 510),
            Some(0x1234),
            "the record as written carries the USN in the sector tail"
        );
        parse_volume_record(&mut r, 512, path()).expect("parses");
        assert_eq!(
            u16_at(&r, 510),
            Some(0),
            "after fixups the real bytes are back"
        );
    }

    #[test]
    fn a_torn_record_is_rejected() {
        let mut r = volume_record(0, None);
        // Corrupt one sector's stamped USN: that is precisely the signal that
        // the sector was written at a different time from its neighbours.
        r[510] = 0xFF;
        let err = parse_volume_record(&mut r, 512, path()).expect_err("torn");
        assert!(err.to_string().contains("torn"), "{err}");
    }

    #[test]
    fn a_record_that_is_not_a_file_record_is_rejected() {
        let mut r = volume_record(0, None);
        r[0..4].copy_from_slice(b"BAAD");
        let err = parse_volume_record(&mut r, 512, path()).expect_err("not FILE");
        assert!(err.to_string().contains("not a FILE record"), "{err}");
    }

    #[test]
    fn a_zero_length_attribute_does_not_hang() {
        let mut r = volume_record(0, None);
        let first = usize::from(u16_at(&r, 0x14).expect("offset"));
        // Undo fixups first so the write lands on the real record, then
        // re-stamp is unnecessary because the corruption is in sector 0.
        r[first + 4..first + 8].copy_from_slice(&0u32.to_le_bytes());
        let err = parse_volume_record(&mut r, 512, path()).expect_err("implausible length");
        assert!(err.to_string().contains("implausible length"), "{err}");
    }

    #[test]
    fn a_record_without_volume_information_is_rejected() {
        let mut r = volume_record(0, None);
        let first = usize::from(u16_at(&r, 0x14).expect("offset"));
        r[first..first + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        let err = parse_volume_record(&mut r, 512, path()).expect_err("no info");
        assert!(err.to_string().contains("$VOLUME_INFORMATION"), "{err}");
    }

    #[test]
    fn mft_record_offset_is_lcn_times_cluster_plus_index() {
        let bs = BootSector {
            bytes_per_sector: 512,
            cluster_size: 4096,
            mft_lcn: 786_432,
            mft_record_size: 1024,
            total_sectors: 1_000_000,
        };
        assert_eq!(
            bs.mft_record_offset(3),
            Some(786_432 * 4096 + 3 * 1024),
            "$Volume is the fourth record in the MFT"
        );
    }

    #[test]
    fn mft_record_offset_refuses_to_overflow() {
        let bs = BootSector {
            bytes_per_sector: 512,
            cluster_size: 4096,
            mft_lcn: u64::MAX,
            mft_record_size: 1024,
            total_sectors: 0,
        };
        assert_eq!(bs.mft_record_offset(3), None);
    }

    #[test]
    fn reading_a_non_ntfs_file_is_an_error_not_a_panic() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(f.path(), vec![0u8; 4096]).expect("write");
        assert!(read(f.path()).is_err());
    }

    #[test]
    fn backing_device_is_none_for_a_missing_path() {
        assert_eq!(backing_device(Path::new("/nonexistent-zzz")), None);
    }

    #[test]
    fn labels_round_trip_through_utf16() {
        let utf16: Vec<u8> = "Café".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_label(&utf16).as_deref(), Some("Café"));
        assert_eq!(decode_label(&[]), None);
        assert_eq!(decode_label(&[0x41]), None, "odd length is not UTF-16");
    }
}

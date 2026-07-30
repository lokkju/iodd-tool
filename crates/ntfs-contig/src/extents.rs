//! `FS_IOC_FIEMAP` and the extent-run merge.
//!
//! Ported from `spike/fiemap.py`, which was validated against a physical IODD
//! device. The ioctl and the merge stay separate on purpose: the merge is a
//! pure function over records, which makes it testable without a filesystem.
//!
//! # Two things measured the hard way
//!
//! **`fm_length` must be bounded to the file size.** Query past end-of-file, as
//! `filefrag` does, and ntfs3 returns an extra `UNWRITTEN` record covering the
//! slack in the final cluster — 3584 bytes for a file ending 512 bytes into a
//! 4096-byte cluster. That record is cluster tail, not file data. It would
//! corrupt diagnostics, and decisively it would make [`ExtentMap::any_unwritten`]
//! true forever, so the post-write check in the allocation sequence could never
//! pass. See `spike/FINDINGS.md`, F3.
//!
//! **Adjacent records must merge.** A contiguous file can be reported as
//! several physically adjacent records; NTFS run lists split for reasons
//! unrelated to fragmentation. Counting raw records would call such a file
//! fragmented.

use crate::error::{Error, Result, Run};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::Path;

// _IOWR('f', 11, struct fiemap), where sizeof(struct fiemap) == 32.
#[allow(clippy::cast_possible_wrap, clippy::unnecessary_cast)]
const FS_IOC_FIEMAP: crate::IoctlRequest = 0xC020_660B_u32 as crate::IoctlRequest;

const FIEMAP_FLAG_SYNC: u32 = 0x0001;

pub const FIEMAP_EXTENT_LAST: u32 = 0x0001;
pub const FIEMAP_EXTENT_UNKNOWN: u32 = 0x0002;
pub const FIEMAP_EXTENT_DELALLOC: u32 = 0x0004;
pub const FIEMAP_EXTENT_ENCODED: u32 = 0x0008;
pub const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x0200;
pub const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x0800;
pub const FIEMAP_EXTENT_MERGED: u32 = 0x1000;

/// Flags that mean the map cannot be trusted for a contiguity decision.
///
/// Design revision 1: treat these as unverifiable (exit 8) rather than
/// silently passing.
pub const UNTRUSTED: u32 = FIEMAP_EXTENT_UNKNOWN
    | FIEMAP_EXTENT_DELALLOC
    | FIEMAP_EXTENT_ENCODED
    | FIEMAP_EXTENT_DATA_INLINE;

/// How many extent records to request per ioctl round.
const BATCH: u32 = 1024;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct FiemapHeader {
    fm_start: u64,
    fm_length: u64,
    fm_flags: u32,
    fm_mapped_extents: u32,
    fm_extent_count: u32,
    fm_reserved: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct FiemapExtent {
    fe_logical: u64,
    fe_physical: u64,
    fe_length: u64,
    fe_reserved64: [u64; 2],
    fe_flags: u32,
    fe_reserved: [u32; 3],
}

// The ioctl request number encodes sizeof(struct fiemap) in its size field, so
// a layout drift would silently change which ioctl we invoke.
const _: () = assert!(size_of::<FiemapHeader>() == 32);
const _: () = assert!(size_of::<FiemapExtent>() == 56);
const _: () = assert!(align_of::<FiemapHeader>() == 8);

/// One record exactly as the kernel reported it, before merging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
    pub flags: u32,
}

/// The result of mapping a file: merged runs plus the facts a verdict needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtentMap {
    /// Physically contiguous runs, in logical order.
    pub runs: Vec<Run>,
    /// Raw record count, before merging. Diagnostic only.
    pub raw_records: usize,
    /// Any extent flagged `FIEMAP_EXTENT_UNWRITTEN`, i.e. allocated but never
    /// written. Such a region hands the device stale cluster contents.
    pub any_unwritten: bool,
}

impl ExtentMap {
    /// The tool's core question.
    #[must_use]
    pub fn is_contiguous(&self) -> bool {
        self.runs.len() == 1
    }
}

/// Fold raw records into physically contiguous runs.
///
/// Pure, so every interesting case is a unit test rather than a filesystem
/// fixture. Zero-length records are dropped: they are markers, not data. A
/// record joins the current run only when it continues it in **both** the
/// logical and physical dimensions — abutting physical offsets alone are not
/// enough, since a hole in the logical space is still a discontinuity.
#[must_use]
pub fn merge_runs(extents: &[Extent]) -> Vec<Run> {
    let mut real: Vec<&Extent> = extents.iter().filter(|e| e.length > 0).collect();
    real.sort_by_key(|e| e.logical);

    let mut runs: Vec<Run> = Vec::new();
    for e in real {
        match runs.last_mut() {
            Some(cur)
                if e.physical == cur.physical.saturating_add(cur.length)
                    && e.logical == cur.logical.saturating_add(cur.length) =>
            {
                cur.length = cur.length.saturating_add(e.length);
            }
            _ => runs.push(Run {
                logical: e.logical,
                physical: e.physical,
                length: e.length,
            }),
        }
    }
    runs
}

/// Bitwise-or of any untrusted flags present.
#[must_use]
pub fn untrusted_flags(extents: &[Extent]) -> u32 {
    extents.iter().fold(0, |acc, e| acc | (e.flags & UNTRUSTED))
}

/// True if any extent is allocated but uninitialized.
#[must_use]
pub fn any_unwritten(extents: &[Extent]) -> bool {
    extents
        .iter()
        .any(|e| e.flags & FIEMAP_EXTENT_UNWRITTEN != 0)
}

/// Issue `FS_IOC_FIEMAP` until the `LAST` flag appears, returning raw records.
///
/// `size` bounds the query. Passing anything larger than the file size picks up
/// past-EOF cluster slack; see the module docs.
pub fn read_extents(fd: BorrowedFd<'_>, size: u64, path: &Path) -> Result<Vec<Extent>> {
    if size == 0 {
        return Ok(Vec::new());
    }

    let mut out: Vec<Extent> = Vec::new();
    let mut start: u64 = 0;

    loop {
        // Allocate as u64 so the buffer is 8-aligned. A Vec<u8> is 1-aligned
        // and would produce unaligned reads of the u64 members.
        let words = (size_of::<FiemapHeader>() + BATCH as usize * size_of::<FiemapExtent>())
            .div_ceil(size_of::<u64>());
        let mut buf: Vec<u64> = vec![0; words];

        let header = FiemapHeader {
            fm_start: start,
            fm_length: size - start,
            fm_flags: FIEMAP_FLAG_SYNC,
            fm_mapped_extents: 0,
            fm_extent_count: BATCH,
            fm_reserved: 0,
        };

        // SAFETY: buf holds at least one header plus BATCH extents and is
        // 8-aligned by construction; FiemapHeader is a plain repr(C) struct of
        // integers with no padding requirements beyond that.
        unsafe {
            std::ptr::write(buf.as_mut_ptr().cast::<FiemapHeader>(), header);
        }

        // SAFETY: the buffer matches what FS_IOC_FIEMAP expects — a fiemap
        // header followed by fm_extent_count extent slots.
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), FS_IOC_FIEMAP, buf.as_mut_ptr()) };
        if rc != 0 {
            let errno = std::io::Error::last_os_error();
            return Err(Error::Unverifiable {
                path: path.to_path_buf(),
                reason: format!("FS_IOC_FIEMAP failed: {errno}"),
            });
        }

        // SAFETY: the ioctl succeeded, so the kernel filled in the header.
        let out_header = unsafe { std::ptr::read(buf.as_ptr().cast::<FiemapHeader>()) };
        let mapped = out_header.fm_mapped_extents as usize;

        if mapped == 0 {
            // No extents is a legitimate answer: an entirely sparse file is
            // all hole and has none. Distinguishing that from a driver that
            // failed to report needs `st_blocks`, which the caller has and we
            // do not, so report the empty map and let it decide.
            break;
        }

        let mut saw_last = false;
        for i in 0..mapped.min(BATCH as usize) {
            let byte_offset = size_of::<FiemapHeader>() + i * size_of::<FiemapExtent>();
            // SAFETY: i < min(mapped, BATCH), so this slot is inside the
            // buffer. read_unaligned because the offset is not a multiple of
            // the struct's alignment for odd i.
            let raw = unsafe {
                std::ptr::read_unaligned(
                    buf.as_ptr()
                        .cast::<u8>()
                        .add(byte_offset)
                        .cast::<FiemapExtent>(),
                )
            };
            out.push(Extent {
                logical: raw.fe_logical,
                physical: raw.fe_physical,
                length: raw.fe_length,
                flags: raw.fe_flags,
            });
            if raw.fe_flags & FIEMAP_EXTENT_LAST != 0 {
                saw_last = true;
            }
        }

        if saw_last {
            break;
        }

        // Advance. The guards mirror the prototype's: a driver that fails to
        // make progress must not spin forever.
        let Some(last) = out.last() else { break };
        let next = last.logical.saturating_add(last.length);
        if next <= start || next >= size {
            break;
        }
        start = next;
    }

    Ok(out)
}

/// Map a file and bundle everything a verdict needs.
///
/// Returns [`Error::Unverifiable`] rather than a verdict when the map carries
/// flags that make contiguity undecidable.
pub fn map(fd: BorrowedFd<'_>, size: u64, path: &Path) -> Result<ExtentMap> {
    let extents = read_extents(fd, size, path)?;

    let untrusted = untrusted_flags(&extents);
    if untrusted != 0 {
        return Err(Error::Unverifiable {
            path: path.to_path_buf(),
            reason: format!("extent map carries untrusted flags (0x{untrusted:04x})"),
        });
    }

    Ok(ExtentMap {
        runs: merge_runs(&extents),
        raw_records: extents.len(),
        any_unwritten: any_unwritten(&extents),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(logical: u64, physical: u64, length: u64, flags: u32) -> Extent {
        Extent {
            logical,
            physical,
            length,
            flags,
        }
    }

    #[test]
    fn empty_input_yields_no_runs() {
        assert!(merge_runs(&[]).is_empty());
    }

    #[test]
    fn adjacent_records_fold_into_one_run() {
        let e = [ext(0, 1000, 100, 0), ext(100, 1100, 100, 0)];
        let runs = merge_runs(&e);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].length, 200);
        assert_eq!(runs[0].physical, 1000);
    }

    #[test]
    fn a_physical_gap_splits_runs() {
        let e = [ext(0, 1000, 100, 0), ext(100, 5000, 100, 0)];
        assert_eq!(merge_runs(&e).len(), 2);
    }

    /// The case a physical-only merge gets wrong: the physical offsets abut,
    /// but there is a hole in the logical space, so this is not one run.
    #[test]
    fn a_logical_gap_splits_even_when_physical_abuts() {
        let e = [ext(0, 1000, 100, 0), ext(999_999, 1100, 100, 0)];
        assert_eq!(merge_runs(&e).len(), 2);
    }

    #[test]
    fn zero_length_records_are_dropped() {
        let e = [ext(0, 1000, 100, 0), ext(100, 1100, 0, FIEMAP_EXTENT_LAST)];
        let runs = merge_runs(&e);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].length, 100);
    }

    #[test]
    fn out_of_order_input_is_sorted_first() {
        let e = [ext(100, 1100, 100, 0), ext(0, 1000, 100, 0)];
        let runs = merge_runs(&e);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].logical, 0);
    }

    /// Measured on the physical device: the working 40 GiB .rmd, queried with
    /// fm_length bounded to the file size, returns exactly one record.
    /// FINDINGS F3.
    #[test]
    fn device_rmd_bounded_query_is_one_run() {
        let e = [ext(
            0,
            203_056_373_760,
            42_949_673_472,
            FIEMAP_EXTENT_LAST | FIEMAP_EXTENT_MERGED,
        )];
        let runs = merge_runs(&e);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].length, 42_949_673_472);
    }

    /// The same file queried past EOF, as filefrag does. The trailing record is
    /// cluster slack: 3584 bytes beyond end-of-file, flagged UNWRITTEN. It
    /// happens to abut physically, so merging alone still yields one run — but
    /// the run then overruns the file size, and any_unwritten goes true, which
    /// would make the post-write check in the allocation sequence fail forever.
    /// This is why the query is bounded. FINDINGS F3.
    #[test]
    fn unbounded_query_picks_up_cluster_slack() {
        let file_size = 42_949_673_472_u64;
        let e = [
            ext(0, 203_056_373_760, file_size, FIEMAP_EXTENT_MERGED),
            ext(
                file_size,
                203_056_373_760 + file_size,
                3584,
                FIEMAP_EXTENT_LAST | FIEMAP_EXTENT_UNWRITTEN | FIEMAP_EXTENT_MERGED,
            ),
        ];
        let runs = merge_runs(&e);
        assert_eq!(runs.len(), 1, "the slack abuts, so it merges");
        assert!(
            runs[0].length > file_size,
            "the merged run overruns the file, corrupting diagnostics"
        );
        assert!(
            any_unwritten(&e),
            "and any_unwritten goes true, which would fail every create"
        );
    }

    /// Measured: a 1 GiB fallocate on a filled-then-emptied 4 GiB volume came
    /// back in two runs, the first ending exactly on the MFT zone boundary.
    /// FINDINGS S2/S5. This is the refusal case.
    #[test]
    fn measured_two_run_fragmentation_is_detected() {
        let e = [
            ext(0, 1_867_776, 535_015_424, 0),
            ext(535_015_424, 2_168_954_880, 538_726_400, FIEMAP_EXTENT_LAST),
        ];
        let runs = merge_runs(&e);
        assert_eq!(runs.len(), 2);
        assert_eq!(
            runs[0].physical + runs[0].length,
            536_883_200,
            "run 0 ends on the MFT zone boundary: 131075 clusters * 4096"
        );
    }

    /// Measured on deliberately fragmented free space. FINDINGS S7.
    #[test]
    fn measured_five_run_fragmentation_is_detected() {
        let e = [
            ext(0, 190_513_152, 78_528_512, 0),
            ext(78_528_512, 694_775_808, 253_706_240, 0),
            ext(332_234_752, 1_202_188_288, 253_706_240, 0),
            ext(585_940_992, 1_709_600_768, 253_706_240, 0),
            ext(839_647_232, 2_422_661_120, 234_094_592, FIEMAP_EXTENT_LAST),
        ];
        assert_eq!(merge_runs(&e).len(), 5);
    }

    #[test]
    fn untrusted_flags_are_reported() {
        assert_eq!(untrusted_flags(&[ext(0, 0, 1, 0)]), 0);
        assert_eq!(
            untrusted_flags(&[ext(0, 0, 1, FIEMAP_EXTENT_UNKNOWN)]),
            FIEMAP_EXTENT_UNKNOWN
        );
        assert_eq!(
            untrusted_flags(&[ext(0, 0, 1, FIEMAP_EXTENT_DELALLOC)]),
            FIEMAP_EXTENT_DELALLOC
        );
        // UNWRITTEN is a fact about the data, not a reason to distrust the map.
        assert_eq!(untrusted_flags(&[ext(0, 0, 1, FIEMAP_EXTENT_UNWRITTEN)]), 0);
    }

    #[test]
    fn any_unwritten_detects_uninitialized_extents() {
        assert!(!any_unwritten(&[ext(0, 0, 1, 0)]));
        assert!(any_unwritten(&[
            ext(0, 0, 1, 0),
            ext(1, 1, 1, FIEMAP_EXTENT_UNWRITTEN)
        ]));
    }

    #[test]
    fn saturating_arithmetic_survives_absurd_records() {
        // A hostile or buggy driver must not panic us.
        let e = [ext(0, u64::MAX, u64::MAX, 0), ext(u64::MAX, 0, 1, 0)];
        let _ = merge_runs(&e);
    }
}

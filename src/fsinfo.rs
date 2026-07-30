//! Filesystem interrogation: ntfs3 detection, the NTFS compression attribute,
//! and free-space reporting.
//!
//! The engine itself is filesystem-agnostic. These checks are ntfs3-specific
//! and are gated on actually being on ntfs3, so running against ext4 during
//! development neither errors nor silently misreports.

use crate::error::Result;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::Path;

/// `statfs` `f_type` for the in-kernel ntfs3 driver. Measured on a mounted
/// IODD volume; see `spike/FINDINGS.md`, F6.
pub const NTFS3_MAGIC: i64 = 0x7366_746e;

/// The legacy read-mostly `ntfs` driver, for reporting only.
pub const NTFS_LEGACY_MAGIC: i64 = 0x5346_544e;

// _IOR('f', 1, long)
#[allow(clippy::cast_possible_wrap, clippy::unnecessary_cast)]
const FS_IOC_GETFLAGS: crate::IoctlRequest = 0x8008_6601_u32 as crate::IoctlRequest;

/// `FS_COMPR_FL` — the file is compressed.
const FS_COMPR_FL: libc::c_long = 0x0000_0004;

/// True when the file lives on a volume mounted by the ntfs3 driver.
///
/// A failure to interrogate is reported as "not ntfs3" rather than as an
/// error: every caller uses this only to decide whether an ntfs3-specific
/// check applies.
#[must_use]
pub fn is_ntfs3(fd: BorrowedFd<'_>) -> bool {
    fs_magic(fd).is_some_and(|m| m == NTFS3_MAGIC)
}

/// The raw `statfs` `f_type`, for diagnostics.
#[must_use]
pub fn fs_magic(fd: BorrowedFd<'_>) -> Option<i64> {
    // SAFETY: a zeroed statfs is a valid starting state; fstatfs fills it.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: fd is valid for the borrow, and buf is a live statfs.
    let rc = unsafe { libc::fstatfs(fd.as_raw_fd(), &raw mut buf) };
    if rc != 0 {
        return None;
    }
    // `f_type` is already i64 on glibc/x86_64 but is a different width on
    // musl and other targets, so cast rather than convert. The musl CI job is
    // what keeps this honest.
    #[allow(clippy::unnecessary_cast, clippy::cast_possible_wrap)]
    Some(buf.f_type as i64)
}

/// Whether the NTFS compression attribute is set.
///
/// `Ok(None)` means the filesystem does not expose the attribute, which is not
/// a failure — it is the normal answer on ext4 and on any driver without
/// `FS_IOC_GETFLAGS`.
///
/// A compressed target makes "fixed allocation" a fiction and reintroduces
/// fragmentation on write, so `create` refuses one. The check runs *after*
/// creation as well as on the parent directory, because a new file inherits
/// the attribute (design revision 6).
pub fn is_compressed(fd: BorrowedFd<'_>) -> Result<Option<bool>> {
    if !is_ntfs3(fd) {
        return Ok(None);
    }
    let mut flags: libc::c_long = 0;
    // SAFETY: FS_IOC_GETFLAGS writes a single long through the pointer.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), FS_IOC_GETFLAGS, &raw mut flags) };
    if rc != 0 {
        // ENOTTY simply means unsupported. Anything else is still not worth
        // failing a read-only audit over.
        return Ok(None);
    }
    Ok(Some(flags & FS_COMPR_FL != 0))
}

/// Free bytes on the volume holding `path`.
///
/// Best-effort; `None` when it cannot be determined.
#[must_use]
pub fn free_space(path: &Path) -> Option<u64> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: a zeroed statvfs is a valid starting state; statvfs fills it.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is NUL-terminated and buf is a live statvfs.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &raw mut buf) };
    if rc != 0 {
        return None;
    }
    Some((buf.f_bavail as u64).saturating_mul(buf.f_frsize as u64))
}

/// The largest contiguous free run on the volume.
///
/// Always `None` today. Determining it requires reading NTFS `$Bitmap`, which
/// ntfs3 hides unless the volume was mounted with `showmeta`. Design revision
/// 10 makes this best-effort and explicitly non-blocking; the call site exists
/// so the diagnostic slot is wired up if a source ever appears.
#[must_use]
pub const fn largest_free_run(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn magics_are_the_measured_constants() {
        // Guards a constant nobody would otherwise notice a typo in.
        assert_eq!(NTFS3_MAGIC, 0x7366_746e);
        assert_eq!(NTFS_LEGACY_MAGIC, 0x5346_544e);
    }

    #[test]
    fn a_dev_box_filesystem_is_not_reported_as_ntfs3() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        assert!(!is_ntfs3(f.as_file().as_fd()));
    }

    #[test]
    fn compression_is_unknown_off_ntfs3() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let got = is_compressed(f.as_file().as_fd()).expect("no error");
        assert_eq!(got, None, "off ntfs3 the attribute is not exposed");
    }

    #[test]
    fn fs_magic_is_readable() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        assert!(fs_magic(f.as_file().as_fd()).is_some());
    }

    #[test]
    fn free_space_is_reported_for_a_real_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(free_space(dir.path()).is_some());
    }

    #[test]
    fn free_space_is_none_for_a_missing_path() {
        assert_eq!(free_space(Path::new("/nonexistent-zzz")), None);
    }

    #[test]
    fn largest_free_run_is_honestly_unavailable() {
        assert_eq!(largest_free_run(Path::new("/")), None);
    }
}

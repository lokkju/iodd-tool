//! Allocate and verify contiguous, fully-allocated files on Linux.
//!
//! # What this is for
//!
//! Some consumers of a file do not read it through the filesystem. They map its
//! extents once and then stream sectors straight off the block device — IODD
//! hardware enclosures, certain hypervisor fast paths, and some database and
//! swap configurations. For those, two properties matter and neither is visible
//! through ordinary file APIs:
//!
//! * the file is **fully allocated**, not sparse and not merely reserved
//! * the file occupies a **single contiguous extent**
//!
//! This crate measures both, and allocates files intended to satisfy them.
//!
//! # What it cannot do
//!
//! It cannot *make* a file contiguous. Measurement on ntfs3 showed the
//! allocator places blocks identically whether the request arrives as
//! `fallocate(2)` or as sequential writes, so userspace has no lever on
//! placement. Where a volume's free space permits a single run you get one;
//! where it does not, no approach does, and this crate reports that rather than
//! pretending otherwise. Consolidating an already-fragmented file needs
//! filesystem-specific relocation that Linux does not expose for NTFS.
//!
//! # Two things measured the hard way
//!
//! **`fallocate(2)` does not zero.** Allocated-but-unwritten clusters keep
//! whatever was on them. Reads through the filesystem return zeros because the
//! filesystem synthesizes them past the valid data length; a consumer reading
//! the raw device sees the old bytes. [`extents::ExtentMap::any_unwritten`]
//! detects this in one ioctl.
//!
//! **FIEMAP must be bounded to the file size.** Querying past end-of-file, as
//! `filefrag` does, returns an extra `UNWRITTEN` record covering slack in the
//! final cluster. That record is cluster tail, not file data.

pub mod alloc;
pub mod error;
pub mod extents;
pub mod fsinfo;
pub mod volume;

pub use error::{Error, Result, Run};
pub use extents::{Extent, ExtentMap};

/// The request parameter of `ioctl(2)` is `c_int` on musl and `c_ulong` on
/// glibc. The constants themselves are the same bit pattern either way; only
/// the declared type differs, so they are written as `u32` literals and cast.
#[cfg(target_env = "musl")]
pub(crate) type IoctlRequest = libc::c_int;
#[cfg(not(target_env = "musl"))]
pub(crate) type IoctlRequest = libc::c_ulong;

/// Everything a caller needs to judge a file, gathered in one pass.
#[derive(Debug, Clone)]
pub struct FileFacts {
    pub size: u64,
    /// `st_blocks * 512`.
    pub allocated: u64,
    pub map: ExtentMap,
    /// `statfs` `f_type`, when obtainable.
    pub fs_magic: Option<i64>,
    /// `Some(true)` when NTFS compression is set, `None` when the filesystem
    /// does not expose the attribute.
    pub compressed: Option<bool>,
}

impl FileFacts {
    /// Gather facts about an already-open file. Read-only; nothing is written.
    pub fn gather(file: &std::fs::File, path: &std::path::Path) -> Result<Self> {
        use std::os::fd::AsFd;
        use std::os::unix::fs::MetadataExt;

        let meta = file.metadata().map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let size = meta.len();
        let fd = file.as_fd();
        Ok(Self {
            size,
            allocated: meta.blocks().saturating_mul(512),
            map: extents::map(fd, size, path)?,
            fs_magic: fsinfo::fs_magic(fd),
            compressed: fsinfo::is_compressed(fd)?,
        })
    }

    /// Open `path` read-only and gather facts about it.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::gather(&file, path)
    }

    /// True when the file is sparse: fewer bytes allocated than its length.
    #[must_use]
    pub fn is_sparse(&self) -> bool {
        self.size > 0 && self.allocated < self.size
    }

    /// True when the file occupies exactly one physically contiguous run.
    #[must_use]
    pub fn is_contiguous(&self) -> bool {
        self.map.is_contiguous()
    }
}

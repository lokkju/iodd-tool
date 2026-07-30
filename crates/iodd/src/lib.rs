//! `iodd` — contiguous fixed-size VHD/RMD virtual disks for IODD hardware.
//!
//! The library target exists so integration tests can exercise the pure pieces
//! (`merge_runs`, `chs`, `grade`, the size parser) directly rather than only
//! through the binary. The binary remains the deliverable.
//!
//! ## What this tool does and does not do
//!
//! IODD devices map a file's extents once at mount time and stream sectors
//! straight off the block device. Two properties are mandatory: the file must
//! be fully allocated, and it must occupy a single contiguous extent.
//!
//! This tool allocates, zeroes, and **verifies** those properties, refusing to
//! hand over a file that fails them. It does not *produce* contiguity —
//! measurement showed ntfs3 places blocks identically however the request
//! arrives, so userspace has no lever on placement. See `spike/FINDINGS.md`,
//! S7.

pub mod cli;
pub mod cmd;
pub mod error;
pub mod footer;
pub mod report;
pub mod size;
pub mod source;

pub use error::{Error, Result, Run};

// The engine, re-exported so `iodd` code and tests reach it by one path.
pub use ntfs_contig::{FileFacts, extents, fsinfo};

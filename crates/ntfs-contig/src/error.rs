//! Engine-level errors.
//!
//! Deliberately narrow: these describe properties of a *file on a filesystem*,
//! not of any particular file format. Consumers wrap them with their own
//! domain errors — `iodd` adds footer and CLI-usage variants, for instance.

use std::path::PathBuf;

/// A physically contiguous run of a file, as reported by the extent map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not allocate {requested} bytes for {path}: {reason}")]
    Allocation {
        path: PathBuf,
        requested: u64,
        reason: String,
    },

    #[error("{path} occupies {} extents, not 1", runs.len())]
    Fragmented {
        path: PathBuf,
        runs: Vec<Run>,
        free_bytes: Option<u64>,
        largest_free_run: Option<u64>,
    },

    #[error("{path} is not fully allocated: {allocated} bytes allocated, {expected} required")]
    Sparse {
        path: PathBuf,
        allocated: u64,
        expected: u64,
    },

    #[error("{path} contains uninitialized extents; the data region was never written")]
    Uninitialized { path: PathBuf },

    #[error("{path} is NTFS-compressed; fixed allocation cannot be guaranteed")]
    Compressed { path: PathBuf },

    #[error("could not verify contiguity of {path}: {reason}")]
    Unverifiable { path: PathBuf, reason: String },

    /// The device is not a legible NTFS volume. Distinct from [`Error::Io`]:
    /// the read succeeded, but what came back could not be interpreted.
    #[error("could not read NTFS volume state from {path}: {reason}")]
    VolumeUnreadable { path: PathBuf, reason: String },
}

pub type Result<T> = std::result::Result<T, Error>;

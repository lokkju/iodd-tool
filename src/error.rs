//! The one place the `SPEC.md` exit-code table is implemented.
//!
//! Every variant carries structured data rather than a pre-formatted string,
//! so diagnostics required by `SPEC.md` line 162 (the extent map, the file
//! size, free space) are rendered at the edge from real values.

use std::path::PathBuf;

/// A physically contiguous run of a file, as reported by the extent map.
///
/// Defined here rather than in `extents` so `Error` has no dependency on the
/// engine; `extents` re-exports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // ---- 1: usage / argument error -------------------------------------
    #[error("{0}")]
    Usage(String),

    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    // ---- 2: target exists without --force ------------------------------
    #[error("{path} already exists (use --force to overwrite)")]
    TargetExists { path: PathBuf },

    // ---- 3: allocation failed ------------------------------------------
    #[error("could not allocate {requested} bytes for {path}: {reason}")]
    Allocation {
        path: PathBuf,
        requested: u64,
        reason: String,
    },

    // ---- 4: contiguity check failed ------------------------------------
    #[error("{path} occupies {} extents, not 1", runs.len())]
    Fragmented {
        path: PathBuf,
        runs: Vec<Run>,
        free_bytes: Option<u64>,
        largest_free_run: Option<u64>,
    },

    // ---- 5: sparseness check failed ------------------------------------
    #[error("{path} is not fully allocated: {allocated} bytes allocated, {expected} required")]
    Sparse {
        path: PathBuf,
        allocated: u64,
        expected: u64,
    },

    #[error("{path} contains uninitialized extents; the data region was not written")]
    Uninitialized { path: PathBuf },

    // ---- 6: target on compressed NTFS ----------------------------------
    #[error("{path} is NTFS-compressed; fixed allocation cannot be guaranteed")]
    Compressed { path: PathBuf },

    // ---- 7: footer verification failed ---------------------------------
    #[error("{path}: {reason}")]
    Footer { path: PathBuf, reason: String },

    // ---- 8: contiguity unverifiable ------------------------------------
    #[error("could not verify contiguity of {path}: {reason}")]
    Unverifiable { path: PathBuf, reason: String },

    // ---- 9: external qemu-img required but absent or failed ------------
    #[error("qemu-img: {reason}")]
    QemuImg { reason: String },

    // ---- scaffolding, removed as phases land ---------------------------
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
}

impl Error {
    /// Map to the process exit code from the `SPEC.md` table.
    ///
    /// This match is exhaustive on purpose: adding a variant without assigning
    /// it a code will fail to compile.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) | Error::Io { .. } => 1,
            Error::TargetExists { .. } => 2,
            Error::Allocation { .. } => 3,
            Error::Fragmented { .. } => 4,
            Error::Sparse { .. } | Error::Uninitialized { .. } => 5,
            Error::Compressed { .. } => 6,
            Error::Footer { .. } => 7,
            Error::Unverifiable { .. } => 8,
            Error::QemuImg { .. } => 9,
            // Scaffolding only. Reported as a usage error so it never
            // masquerades as a real diagnostic.
            Error::NotImplemented(_) => 1,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    fn io_err() -> std::io::Error {
        std::io::Error::from(std::io::ErrorKind::NotFound)
    }

    /// Asserts every variant against the SPEC.md exit-code table. If a variant
    /// is added, `exit_code`'s match stops compiling before this test runs.
    #[test]
    fn exit_codes_match_the_spec_table() {
        let p = || PathBuf::from("/x");
        let cases: Vec<(Error, i32)> = vec![
            (Error::Usage("bad size".into()), 1),
            (
                Error::Io {
                    path: p(),
                    source: io_err(),
                },
                1,
            ),
            (Error::TargetExists { path: p() }, 2),
            (
                Error::Allocation {
                    path: p(),
                    requested: 512,
                    reason: "ENOSPC".into(),
                },
                3,
            ),
            (
                Error::Fragmented {
                    path: p(),
                    runs: vec![],
                    free_bytes: None,
                    largest_free_run: None,
                },
                4,
            ),
            (
                Error::Sparse {
                    path: p(),
                    allocated: 0,
                    expected: 512,
                },
                5,
            ),
            (Error::Uninitialized { path: p() }, 5),
            (Error::Compressed { path: p() }, 6),
            (
                Error::Footer {
                    path: p(),
                    reason: "bad cookie".into(),
                },
                7,
            ),
            (
                Error::Unverifiable {
                    path: p(),
                    reason: "ENOTTY".into(),
                },
                8,
            ),
            (
                Error::QemuImg {
                    reason: "not found".into(),
                },
                9,
            ),
            (Error::NotImplemented("create"), 1),
        ];
        for (err, code) in cases {
            assert_eq!(err.exit_code(), code, "wrong exit code for {err:?}");
        }
    }

    #[test]
    fn fragmented_message_counts_runs() {
        let err = Error::Fragmented {
            path: PathBuf::from("/t.vhd"),
            runs: vec![
                Run {
                    logical: 0,
                    physical: 1024,
                    length: 512,
                },
                Run {
                    logical: 512,
                    physical: 4096,
                    length: 512,
                },
            ],
            free_bytes: None,
            largest_free_run: None,
        };
        assert!(err.to_string().contains("2 extents"));
    }
}

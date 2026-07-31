//! Placing a file contiguously: `copy` and `fix`.
//!
//! Both do the same thing underneath — allocate a contiguous target, put the
//! bytes in it, rename it into place — and neither knows anything about disk
//! image formats. That is deliberate. A byte-for-byte copy has no opinion
//! about footers, virtual sizes, or geometry, so it cannot get them wrong.
//!
//! # Holes are written out by default
//!
//! A sparse source reads as zeros through the filesystem, so skipping its
//! holes would produce a target that *also* reads as zeros — through the
//! filesystem. Hardware that reads raw sectors sees something else entirely:
//! a freshly `fallocate`d region holds whatever was on those clusters before
//! (S1), and the IODD reads raw sectors.
//!
//! So the default writes the source in full, holes included, and the result
//! is faithful on the device as well as through the kernel. [`Options::sparse`]
//! opts out, which is worth it when the source is a freshly created image that
//! is nearly all hole — and dangerous when it is not, because the target then
//! differs from the source everywhere the source had a hole.
//!
//! # `fix` is not a defragmenter
//!
//! [`fix`] re-places one file by copying it somewhere contiguous. It cannot
//! move *other* files out of the way, so it needs a contiguous free run at
//! least as large as the file. When that does not exist it fails, and no
//! amount of retrying will help — that is the case an actual defragmenter
//! would be needed for.

use crate::alloc::{self, AllocOutcome, NoProgress, Progress, TempTarget};
use crate::error::{Error, Result};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Read/write chunk. Matches `convert`'s, which was measured adequate.
const CHUNK: usize = 4 * 1024 * 1024;

/// Defaults are the cautious ones: do not overwrite, do not skip holes, do
/// not leave debris.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Overwrite an existing target.
    pub force: bool,
    /// Skip the source's holes instead of writing them out. Fast, and only
    /// safe when the target's uninitialized contents do not matter.
    pub sparse: bool,
    /// Leave a partial target behind on failure instead of removing it.
    pub keep_on_fail: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub bytes: u64,
    /// Bytes actually written. Below `bytes` only when holes were skipped.
    pub written: u64,
    /// Regions left uninitialized because the source had holes there.
    pub holes_skipped: u64,
}

/// Byte-for-byte copy of `src` into a contiguous, fully-allocated `dst`.
///
/// # Errors
///
/// [`Error::TargetExists`](crate::Error) has no equivalent here — an existing
/// target without `force` is reported as [`Error::Allocation`]. Otherwise:
/// [`Error::Io`] for read/write failure, [`Error::Fragmented`] when the
/// filesystem cannot place the target in one run, and [`Error::Sparse`] if the
/// result is somehow not fully allocated.
pub fn copy(src: &Path, dst: &Path, opts: &Options, progress: &mut dyn Progress) -> Result<Report> {
    copy_impl(src, dst, opts, progress, false)
}

/// `replacing_self` is [`fix`]'s case: the source *is* the eventual target.
/// That is safe because the bytes go to a temporary beside it and only replace
/// the original at the rename, so the two guards below — which exist to stop a
/// caller destroying their own source — must not fire.
fn copy_impl(
    src: &Path,
    dst: &Path,
    opts: &Options,
    progress: &mut dyn Progress,
    replacing_self: bool,
) -> Result<Report> {
    let src_file = crate::fsinfo::open_readonly_nofollow(src)?;
    let len = src_file
        .metadata()
        .map_err(|source| Error::Io {
            path: src.to_path_buf(),
            source,
        })?
        .len();

    if !replacing_self {
        if same_file(src, dst) {
            return Err(Error::Allocation {
                path: dst.to_path_buf(),
                requested: len,
                reason: "the source and the target are the same file".into(),
            });
        }
        if dst.exists() && !opts.force {
            return Err(Error::Allocation {
                path: dst.to_path_buf(),
                requested: len,
                reason: "target exists; pass --force to replace it".into(),
            });
        }
    }

    let target = TempTarget::create(dst, opts.keep_on_fail)?;

    // A zero-length file has no extents at all, so asking whether it occupies
    // one run is meaningless — and `fallocate` rejects a length of 0 outright.
    if len == 0 {
        target.finalize(opts.force || replacing_self)?;
        return Ok(Report {
            bytes: 0,
            written: 0,
            holes_skipped: 0,
        });
    }

    let outcome = alloc::allocate(target.file(), len, target.path())?;
    if outcome == AllocOutcome::Reserved {
        // Refuse before writing anything: a fragmented target is the one
        // failure worth catching early, since the bytes take the longest.
        alloc::require_contiguous(target.file(), len, target.path())?;
    }

    let report = if opts.sparse {
        copy_data_extents(&src_file, src, &target, len, progress)?
    } else {
        copy_whole(&src_file, src, &target, len, progress)?
    };

    target.file().sync_data().map_err(|source| Error::Io {
        path: target.path().to_path_buf(),
        source,
    })?;
    // Only claim "fully written" when we actually wrote every byte.
    alloc::verify_written(target.file(), len, target.path(), !opts.sparse)?;
    target.finalize(opts.force || replacing_self)?;

    Ok(report)
}

/// Make an existing file contiguous, in place, keeping its name.
///
/// Copies to a contiguous temporary beside it and renames over the original,
/// so the file's *name* survives but its inode does not. Nothing else on the
/// volume moves; see the module docs on why this is not a defragmenter.
///
/// # Errors
///
/// As [`copy`]. [`Error::Fragmented`] here means no contiguous free run large
/// enough exists, which retrying will not fix.
pub fn fix(path: &Path, opts: &Options, progress: &mut dyn Progress) -> Result<Report> {
    // Already contiguous is a success, not a no-op worth failing over.
    let facts = crate::FileFacts::open(path)?;
    if facts.is_contiguous() && !facts.is_sparse() && !facts.map.any_unwritten {
        return Ok(Report {
            bytes: facts.size,
            written: 0,
            holes_skipped: 0,
        });
    }

    copy_impl(path, path, opts, progress, true)
}

/// Write every byte of the source, holes included. Holes read as zeros, so
/// this is what makes the target faithful to a raw reader.
fn copy_whole(
    src_file: &File,
    src: &Path,
    target: &TempTarget,
    len: u64,
    progress: &mut dyn Progress,
) -> Result<Report> {
    let mut buf = vec![0u8; CHUNK];
    let mut offset = 0u64;

    while offset < len {
        let want = usize::try_from((len - offset).min(CHUNK as u64)).unwrap_or(CHUNK);
        src_file
            .read_exact_at(&mut buf[..want], offset)
            .map_err(|source| Error::Io {
                path: src.to_path_buf(),
                source,
            })?;
        target.write_at(&buf[..want], offset)?;
        offset += want as u64;
        progress.update(offset, len);
    }
    progress.finish();

    Ok(Report {
        bytes: len,
        written: len,
        holes_skipped: 0,
    })
}

/// Write only the source's data extents, leaving the target's holes as
/// whatever `fallocate` reserved.
fn copy_data_extents(
    src_file: &File,
    src: &Path,
    target: &TempTarget,
    len: u64,
    progress: &mut dyn Progress,
) -> Result<Report> {
    let mut buf = vec![0u8; CHUNK];
    let mut written = 0u64;
    let mut skipped = 0u64;
    let mut offset = 0u64;

    while offset < len {
        let data = match seek_data(src_file, offset, len) {
            Some(d) => d,
            // No further data: the rest of the file is hole.
            None => {
                skipped += len - offset;
                break;
            }
        };
        skipped += data - offset;
        let end = seek_hole(src_file, data, len);

        let mut at = data;
        while at < end {
            let want = usize::try_from((end - at).min(CHUNK as u64)).unwrap_or(CHUNK);
            src_file
                .read_exact_at(&mut buf[..want], at)
                .map_err(|source| Error::Io {
                    path: src.to_path_buf(),
                    source,
                })?;
            target.write_at(&buf[..want], at)?;
            at += want as u64;
            written += want as u64;
            progress.update(at, len);
        }
        offset = end;
    }
    progress.finish();

    Ok(Report {
        bytes: len,
        written,
        holes_skipped: skipped,
    })
}

/// `lseek(SEEK_DATA)`. `None` when no data remains at or after `from`.
///
/// Filesystems without hole tracking report the whole file as data, which is
/// the safe answer: everything gets copied.
fn seek_data(file: &File, from: u64, len: u64) -> Option<u64> {
    let off = lseek(file, from, libc::SEEK_DATA)?;
    (off < len).then_some(off)
}

/// `lseek(SEEK_HOLE)`, clamped to the file length. Every file has an implicit
/// hole at EOF, so this always has an answer.
fn seek_hole(file: &File, from: u64, len: u64) -> u64 {
    lseek(file, from, libc::SEEK_HOLE).map_or(len, |o| o.min(len))
}

fn lseek(file: &File, offset: u64, whence: i32) -> Option<u64> {
    use std::os::fd::AsRawFd;
    let off = i64::try_from(offset).ok()?;
    // SAFETY: fd is valid for the borrow; lseek only moves the description's
    // offset, which nothing else here depends on (all I/O is positional).
    let rc = unsafe { libc::lseek(file.as_raw_fd(), off, whence) };
    u64::try_from(rc).ok()
}

fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Convenience wrapper for callers with no progress reporting.
///
/// # Errors
///
/// As [`copy`].
pub fn copy_quiet(src: &Path, dst: &Path, opts: &Options) -> Result<Report> {
    copy(src, dst, opts, &mut NoProgress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).expect("create");
        f.write_all(bytes).expect("write");
        f.sync_all().expect("sync");
        p
    }

    /// A file with a real hole in the middle: 1 MiB data, 4 MiB hole, 1 MiB
    /// data. `set_len` past a write leaves a hole on any filesystem that
    /// supports them.
    fn sparse_file(dir: &Path, name: &str) -> (PathBuf, u64) {
        let p = dir.join(name);
        let f = std::fs::File::create(&p).expect("create");
        let mb = 1024 * 1024u64;
        f.write_all_at(&vec![0xAB; mb as usize], 0).expect("head");
        f.write_all_at(&vec![0xCD; mb as usize], 5 * mb)
            .expect("tail");
        f.set_len(6 * mb).expect("len");
        f.sync_all().expect("sync");
        (p, 6 * mb)
    }

    #[test]
    fn a_copy_is_byte_for_byte() {
        let dir = tempfile::tempdir().expect("dir");
        let bytes: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let src = write_file(dir.path(), "src.bin", &bytes);
        let dst = dir.path().join("dst.bin");

        let r = copy_quiet(&src, &dst, &Options::default()).expect("copies");
        assert_eq!(r.bytes, bytes.len() as u64);
        assert_eq!(r.written, bytes.len() as u64);
        assert_eq!(r.holes_skipped, 0);
        assert_eq!(std::fs::read(&dst).expect("read"), bytes);
    }

    #[test]
    fn the_target_is_contiguous_and_fully_allocated() {
        let dir = tempfile::tempdir().expect("dir");
        let src = write_file(dir.path(), "src.bin", &vec![7u8; 3 * 1024 * 1024]);
        let dst = dir.path().join("dst.bin");

        copy_quiet(&src, &dst, &Options::default()).expect("copies");
        let facts = crate::FileFacts::open(&dst).expect("facts");
        assert!(facts.is_contiguous(), "runs: {:?}", facts.map.runs);
        assert!(!facts.is_sparse());
        assert!(!facts.map.any_unwritten);
    }

    /// The default has to write holes out, or the target only *looks* like the
    /// source to the kernel while differing on the device.
    #[test]
    fn holes_are_written_out_by_default() {
        let dir = tempfile::tempdir().expect("dir");
        let (src, len) = sparse_file(dir.path(), "sparse.bin");
        let dst = dir.path().join("dense.bin");

        let r = copy_quiet(&src, &dst, &Options::default()).expect("copies");
        assert_eq!(r.written, len, "every byte written");
        assert_eq!(r.holes_skipped, 0);

        assert_eq!(
            std::fs::read(&src).expect("src"),
            std::fs::read(&dst).expect("dst")
        );
        let facts = crate::FileFacts::open(&dst).expect("facts");
        assert!(!facts.map.any_unwritten, "nothing left uninitialized");
    }

    #[test]
    fn sparse_mode_skips_the_holes_it_finds() {
        let dir = tempfile::tempdir().expect("dir");
        let (src, len) = sparse_file(dir.path(), "sparse.bin");
        let dst = dir.path().join("sparse-copy.bin");

        let opts = Options {
            sparse: true,
            ..Options::default()
        };
        let r = copy_quiet(&src, &dst, &opts).expect("copies");
        assert_eq!(r.bytes, len);
        assert!(
            r.holes_skipped > 0,
            "the middle hole should have been skipped, report: {r:?}"
        );
        assert!(r.written < len, "less than the whole file written");
        // The data regions still have to be right.
        let got = std::fs::read(&dst).expect("dst");
        assert_eq!(&got[..1024], &[0xAB; 1024], "leading data copied");
        assert_eq!(
            &got[len as usize - 1024..],
            &[0xCD; 1024],
            "trailing data copied"
        );
    }

    #[test]
    fn refusing_to_overwrite_without_force() {
        let dir = tempfile::tempdir().expect("dir");
        let src = write_file(dir.path(), "src.bin", b"hello");
        let dst = write_file(dir.path(), "dst.bin", b"do not lose me");

        let err = copy_quiet(&src, &dst, &Options::default()).expect_err("refuses");
        assert!(err.to_string().contains("target exists"), "{err}");
        assert_eq!(
            std::fs::read(&dst).expect("read"),
            b"do not lose me",
            "the existing target must be untouched"
        );
    }

    #[test]
    fn force_replaces_the_target() {
        let dir = tempfile::tempdir().expect("dir");
        let src = write_file(dir.path(), "src.bin", b"new contents");
        let dst = write_file(dir.path(), "dst.bin", b"old contents");

        let opts = Options {
            force: true,
            ..Options::default()
        };
        copy_quiet(&src, &dst, &opts).expect("copies");
        assert_eq!(std::fs::read(&dst).expect("read"), b"new contents");
    }

    #[test]
    fn copying_a_file_onto_itself_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let src = write_file(dir.path(), "src.bin", b"data");
        let opts = Options {
            force: true,
            ..Options::default()
        };
        let err = copy_quiet(&src, &src, &opts).expect_err("refuses");
        assert!(err.to_string().contains("same file"), "{err}");
    }

    #[test]
    fn a_failed_copy_leaves_no_target_behind() {
        let dir = tempfile::tempdir().expect("dir");
        let missing = dir.path().join("does-not-exist.bin");
        let dst = dir.path().join("dst.bin");

        assert!(copy_quiet(&missing, &dst, &Options::default()).is_err());
        assert!(!dst.exists(), "no partial target");
    }

    #[test]
    fn an_empty_source_copies_to_an_empty_target() {
        let dir = tempfile::tempdir().expect("dir");
        let src = write_file(dir.path(), "empty.bin", b"");
        let dst = dir.path().join("dst.bin");

        let r = copy_quiet(&src, &dst, &Options::default()).expect("copies");
        assert_eq!(r.bytes, 0);
        assert_eq!(std::fs::metadata(&dst).expect("stat").len(), 0);
    }

    #[test]
    fn fix_on_an_already_contiguous_file_changes_nothing() {
        let dir = tempfile::tempdir().expect("dir");
        let bytes = vec![9u8; 2 * 1024 * 1024];
        let path = write_file(dir.path(), "fine.bin", &bytes);
        let before = std::fs::metadata(&path).expect("stat");

        let r = fix(&path, &Options::default(), &mut NoProgress).expect("fix");
        assert_eq!(r.written, 0, "nothing rewritten");

        use std::os::unix::fs::MetadataExt;
        let after = std::fs::metadata(&path).expect("stat");
        assert_eq!(before.ino(), after.ino(), "the file was left alone");
        assert_eq!(std::fs::read(&path).expect("read"), bytes);
    }

    #[test]
    fn fix_preserves_contents_and_the_name() {
        let dir = tempfile::tempdir().expect("dir");
        // A sparse file is "not contiguous enough" by our own definition
        // (unwritten extents), so fix has real work to do.
        let (path, len) = sparse_file(dir.path(), "sparse.bin");
        let expected = std::fs::read(&path).expect("read");

        let r = fix(&path, &Options::default(), &mut NoProgress).expect("fix");
        assert_eq!(r.bytes, len);

        assert!(path.exists(), "the name survives");
        assert_eq!(std::fs::read(&path).expect("read"), expected);
        let facts = crate::FileFacts::open(&path).expect("facts");
        assert!(facts.is_contiguous());
        assert!(!facts.map.any_unwritten);
    }

    /// `fix` writes through a temporary and renames, so an interrupted run
    /// must never leave the original replaced by a partial file.
    #[test]
    fn fix_leaves_no_stray_temporaries_behind() {
        let dir = tempfile::tempdir().expect("dir");
        let (path, _) = sparse_file(dir.path(), "sparse.bin");

        fix(&path, &Options::default(), &mut NoProgress).expect("fix");

        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "sparse.bin")
            .collect();
        assert!(strays.is_empty(), "left behind: {strays:?}");
    }
}

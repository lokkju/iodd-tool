//! Allocating a contiguous file, and publishing it only once it is good.
//!
//! # The sequence
//!
//! 1. `fallocate(2)` the full length. Instant, and fails fast on ENOSPC.
//! 2. Map the extents. **More than one run aborts here**, before any data is
//!    written, which on a large target saves an entire zeroing pass on the
//!    failure path.
//! 3. Optionally zero. See the warning below.
//! 4. Re-map: still one run, fully allocated, and `UNWRITTEN` cleared if we
//!    zeroed.
//! 5. Write any trailer (a VHD footer, for `iodd`) in place.
//! 6. `rename(2)` the temp file over the target.
//!
//! Step 1 is a raw `fallocate(2)`, not `posix_fallocate(3)`. glibc's wrapper
//! falls back to writing zeros when the kernel returns `EOPNOTSUPP`; musl's
//! returns the error. Since this ships as a static musl binary, the fallback
//! is handled here rather than left to libc.
//!
//! # Allocated is not zeroed
//!
//! `fallocate` reserves clusters without writing them. Reads *through the
//! filesystem* return zeros, because the filesystem synthesizes them past the
//! valid data length — but anything reading the raw device sees whatever was
//! on those clusters before. Measured on ntfs3: six of six sampled offsets
//! returned a previously-deleted pattern.
//!
//! Zeroing is therefore a caller decision, not something this module assumes.
//! Windows has the same property: `VhdTool.exe` creates fixed VHDs with
//! `SetFileValidData`, which is gated behind `SE_MANAGE_VOLUME` precisely
//! because it exposes deleted data.
//!
//! # Nothing is published until it is good
//!
//! Work happens in a temp file in the target's directory, renamed over the
//! target only on success. A failed run therefore cannot destroy an existing
//! good file, which a straight overwrite would.

use crate::error::{Error, Result};
use crate::extents;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Buffer size for the zero pass. Large enough to keep the device busy, small
/// enough not to matter for RSS.
const ZERO_CHUNK: usize = 4 * 1024 * 1024;

/// What `fallocate(2)` managed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocOutcome {
    /// Space is reserved.
    Reserved,
    /// The filesystem has no `fallocate`. The caller must write the bytes to
    /// allocate them, so a zero pass stops being optional.
    Unsupported,
}

/// A file being built, published by rename only when it is good.
///
/// Dropping without [`TempTarget::finalize`] unlinks the temp file, unless
/// `keep_on_fail` was set — in which case it stays at its temp path and is
/// still never renamed over the target. Failure never publishes.
#[derive(Debug)]
pub struct TempTarget {
    file: File,
    temp: PathBuf,
    target: PathBuf,
    keep_on_fail: bool,
    published: bool,
}

impl TempTarget {
    /// Create a temp file in the same directory as `target`.
    ///
    /// Same directory because `rename(2)` cannot cross filesystems, and
    /// because a rename within one directory preserves the extent layout we
    /// went to such trouble to verify.
    ///
    /// The name is dot-prefixed and `.tmp`-suffixed so directory scanners
    /// (`iodd doctor`, for one) skip it.
    pub fn create(target: &Path, keep_on_fail: bool) -> Result<Self> {
        let dir = parent_dir(target);
        let pid = std::process::id();

        // O_EXCL makes collisions self-correcting, so a counter suffices and
        // no RNG dependency is needed.
        for attempt in 0..64u32 {
            let temp = dir.join(format!(".iodd-{pid}-{attempt}.tmp"));
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)
            {
                Ok(file) => {
                    return Ok(Self {
                        file,
                        temp,
                        target: target.to_path_buf(),
                        keep_on_fail,
                        published: false,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(Error::Io { path: temp, source }),
            }
        }
        Err(Error::Io {
            path: dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not find an unused temporary filename",
            ),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.temp
    }

    #[must_use]
    pub fn target(&self) -> &Path {
        &self.target
    }

    #[must_use]
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Write bytes at an absolute offset without moving the file pointer or
    /// changing the length. Used for trailers such as a VHD footer.
    pub fn write_at(&self, buf: &[u8], offset: u64) -> Result<()> {
        self.file
            .write_all_at(buf, offset)
            .map_err(|source| Error::Io {
                path: self.temp.clone(),
                source,
            })
    }

    /// Publish: fsync the file, rename over the target, fsync the directory.
    ///
    /// With `replace = false` this refuses to clobber an existing target, using
    /// `renameat2(RENAME_NOREPLACE)` where the filesystem supports it and
    /// falling back to a pre-flight existence check where it does not — ntfs3
    /// is among those that may not.
    pub fn finalize(mut self, replace: bool) -> Result<()> {
        self.file.sync_all().map_err(|source| Error::Io {
            path: self.temp.clone(),
            source,
        })?;

        if replace {
            std::fs::rename(&self.temp, &self.target).map_err(|source| Error::Io {
                path: self.target.clone(),
                source,
            })?;
        } else {
            rename_noreplace(&self.temp, &self.target)?;
        }

        self.published = true;
        sync_parent_dir(&self.target);
        Ok(())
    }
}

impl Drop for TempTarget {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        if self.keep_on_fail {
            eprintln!(
                "iodd: --keep-on-fail: the partial file is at {}",
                self.temp.display()
            );
            eprintln!("iodd: it was NOT renamed over {}", self.target.display());
            return;
        }
        let _ = std::fs::remove_file(&self.temp);
    }
}

/// `rename(2)` that refuses to overwrite.
fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    use std::ffi::CString;

    let c_from = CString::new(from.as_os_str().as_encoded_bytes()).map_err(|_| Error::Io {
        path: from.to_path_buf(),
        source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
    })?;
    let c_to = CString::new(to.as_os_str().as_encoded_bytes()).map_err(|_| Error::Io {
        path: to.to_path_buf(),
        source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
    })?;

    const RENAME_NOREPLACE: libc::c_uint = 1;
    // Via syscall() rather than the libc wrapper, which musl does not expose.
    // SAFETY: both paths are NUL-terminated and outlive the call.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            c_from.as_ptr(),
            libc::AT_FDCWD,
            c_to.as_ptr(),
            RENAME_NOREPLACE,
        )
    };

    if rc == 0 {
        return Ok(());
    }

    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        // Filesystem or kernel does not support the flag. Fall back to
        // check-then-rename, which races in principle but is the best
        // available, and the caller has already checked for existence too.
        Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP) => {
            if to.exists() {
                return Err(Error::Io {
                    path: to.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "target appeared while the file was being built",
                    ),
                });
            }
            std::fs::rename(from, to).map_err(|source| Error::Io {
                path: to.to_path_buf(),
                source,
            })
        }
        _ => Err(Error::Io {
            path: to.to_path_buf(),
            source: err,
        }),
    }
}

/// The directory holding `path`.
///
/// `Path::parent` returns `Some("")` for a bare relative filename, not `None`,
/// so the obvious `unwrap_or(".")` silently yields an empty path that no
/// syscall accepts.
#[must_use]
pub fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// fsync the directory so the rename itself is durable. Best-effort: a failure
/// here does not invalidate a file that is already on disk.
fn sync_parent_dir(path: &Path) {
    if let Ok(d) = File::open(parent_dir(path)) {
        let _ = d.sync_all();
    }
}

/// Reserve `len` bytes with a raw `fallocate(2)`, mode 0.
///
/// Mode 0 extends the file length as well as reserving, which is what we want:
/// the file must end up exactly `len` bytes.
pub fn allocate(file: &File, len: u64, path: &Path) -> Result<AllocOutcome> {
    if len == 0 {
        return Ok(AllocOutcome::Reserved);
    }

    #[allow(clippy::cast_possible_wrap)]
    let off_len = len as libc::off_t;
    // SAFETY: fd is valid; mode 0 with offset 0 is always well-formed.
    let rc = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, off_len) };
    if rc == 0 {
        return Ok(AllocOutcome::Reserved);
    }

    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        // The filesystem cannot preallocate. Not fatal: writing the bytes
        // allocates them. glibc's posix_fallocate would silently do this for
        // us; musl's would not, so it is explicit here.
        Some(libc::EOPNOTSUPP | libc::ENOSYS) => Ok(AllocOutcome::Unsupported),
        _ => Err(Error::Allocation {
            path: path.to_path_buf(),
            requested: len,
            reason: err.to_string(),
        }),
    }
}

/// Map the file and refuse anything that is not a single run.
///
/// Called immediately after [`allocate`] so a doomed target fails before any
/// data is written.
pub fn require_contiguous(file: &File, len: u64, path: &Path) -> Result<()> {
    let map = extents::map(file.as_fd(), len, path)?;
    if map.runs.len() == 1 {
        return Ok(());
    }
    Err(Error::Fragmented {
        path: path.to_path_buf(),
        runs: map.runs,
        free_bytes: crate::fsinfo::free_space(parent_dir(path)),
        largest_free_run: crate::fsinfo::largest_free_run(parent_dir(path)),
    })
}

/// Measure the largest contiguous allocation this directory will currently
/// grant, by binary search, and report it in bytes.
///
/// This is the "largest linear space" figure that makes a refusal actionable:
/// telling an operator their 12 GiB image will not fit is far less use than
/// telling them 8 GiB would. `SPEC.md` line 162 asks for exactly this on
/// failure.
///
/// The design wrote it off as unobtainable on Linux, because ntfs3 hides
/// `$Bitmap` without the `showmeta` mount option and there is no `defrag /A` to
/// scrape as VHD Tool++ does. But an allocation that is not zeroed is only a
/// `fallocate` and an extent check, so the answer can simply be asked for
/// rather than derived. Each probe is created and immediately removed.
///
/// Returns `None` when even `granularity` bytes cannot be had contiguously.
pub fn probe_largest_contiguous(dir: &Path, upper: u64, granularity: u64) -> Option<u64> {
    let gran = granularity.max(512);
    let mut low = 0u64;
    let mut high = (upper / gran) * gran;
    let mut best = None;

    while low <= high && high >= gran {
        let mid = ((low + high) / 2 / gran) * gran;
        if mid < gran {
            break;
        }
        if fits_contiguously(dir, mid) {
            best = Some(mid);
            low = mid + gran;
        } else {
            if mid < gran * 2 {
                break;
            }
            high = mid - gran;
        }
    }
    best
}

/// One probe: can `len` bytes be had in a single run right now?
fn fits_contiguously(dir: &Path, len: u64) -> bool {
    let probe = dir.join(format!(".iodd-probe-{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&probe);

    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe)
    else {
        return false;
    };

    let ok = allocate(&file, len, &probe).is_ok_and(|outcome| {
        outcome == AllocOutcome::Reserved
            && extents::map(file.as_fd(), len, &probe).is_ok_and(|m| m.runs.len() == 1)
    });

    drop(file);
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Progress reporting for the zero pass, which can run for minutes.
pub trait Progress {
    fn update(&mut self, written: u64, total: u64);
    fn finish(&mut self) {}
}

/// Discards progress. Used by tests and non-interactive runs.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoProgress;

impl Progress for NoProgress {
    fn update(&mut self, _written: u64, _total: u64) {}
}

/// Write zeros over `[start, end)`.
///
/// This is what turns a reservation into bytes that are actually zero on the
/// device. Without it the region holds whatever was there before; see the
/// module docs.
pub fn zero_region(
    file: &File,
    start: u64,
    end: u64,
    path: &Path,
    progress: &mut dyn Progress,
) -> Result<()> {
    if end <= start {
        return Ok(());
    }
    let total = end - start;
    let buf = vec![0u8; ZERO_CHUNK];
    let mut offset = start;

    while offset < end {
        let remaining = end - offset;
        let n = usize::try_from(remaining.min(ZERO_CHUNK as u64)).unwrap_or(ZERO_CHUNK);
        file.write_all_at(&buf[..n], offset)
            .map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?;
        offset += n as u64;
        progress.update(offset - start, total);
    }
    progress.finish();

    // Durability before the post-write checks, so what we verify is what the
    // device will see.
    file.sync_data().map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Post-write checks: still one run, fully allocated, and — when the caller
/// zeroed — no extent left uninitialized.
pub fn verify_written(file: &File, len: u64, path: &Path, expect_written: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let map = extents::map(file.as_fd(), len, path)?;

    if map.runs.len() != 1 {
        return Err(Error::Fragmented {
            path: path.to_path_buf(),
            runs: map.runs,
            free_bytes: path.parent().and_then(crate::fsinfo::free_space),
            largest_free_run: path.parent().and_then(crate::fsinfo::largest_free_run),
        });
    }

    let meta = file.metadata().map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let allocated = meta.blocks().saturating_mul(512);
    if allocated < len {
        return Err(Error::Sparse {
            path: path.to_path_buf(),
            allocated,
            expected: len,
        });
    }

    // Only meaningful when we claimed to write the whole thing. Without a zero
    // pass the region is *expected* to be uninitialized, and saying so is the
    // caller's job rather than an error here.
    if expect_written && map.any_unwritten {
        return Err(Error::Uninitialized {
            path: path.to_path_buf(),
        });
    }

    Ok(())
}

/// A [`Progress`] that draws a throttled one-line meter on a terminal.
pub struct TtyProgress {
    label: String,
    last: std::time::Instant,
}

impl TtyProgress {
    /// `None` when stderr is not a terminal, so piped output stays clean.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Option<Self> {
        // SAFETY: isatty on a plain fd number has no preconditions.
        let is_tty = unsafe { libc::isatty(libc::STDERR_FILENO) } == 1;
        is_tty.then(|| Self {
            label: label.into(),
            last: std::time::Instant::now(),
        })
    }
}

impl Progress for TtyProgress {
    fn update(&mut self, written: u64, total: u64) {
        if self.last.elapsed() < std::time::Duration::from_millis(500) {
            return;
        }
        self.last = std::time::Instant::now();
        let pct = written
            .saturating_mul(100)
            .checked_div(total)
            .unwrap_or(100);
        eprint!("\r{}: {pct}% ({written}/{total} bytes)", self.label);
        let _ = std::io::stderr().flush();
    }

    fn finish(&mut self) {
        eprintln!("\r{}: done{}", self.label, " ".repeat(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_target_lives_beside_its_target_and_is_hidden() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("disk.vhd");
        let t = TempTarget::create(&target, false).expect("create");

        assert_eq!(t.path().parent(), target.parent(), "must share a directory");
        let name = t.path().file_name().and_then(|n| n.to_str()).expect("name");
        assert!(name.starts_with('.'), "dot-prefixed so scanners skip it");
        assert!(name.ends_with(".tmp"), "and .tmp suffixed");
        assert!(t.path().exists());
    }

    #[test]
    fn dropping_without_finalize_removes_the_temp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("disk.vhd");
        let temp_path = {
            let t = TempTarget::create(&target, false).expect("create");
            t.path().to_path_buf()
        };
        assert!(!temp_path.exists(), "the guard must unlink on drop");
        assert!(!target.exists(), "and must never publish");
    }

    #[test]
    fn keep_on_fail_leaves_the_temp_file_but_never_publishes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("disk.vhd");
        let temp_path = {
            let t = TempTarget::create(&target, true).expect("create");
            t.path().to_path_buf()
        };
        assert!(temp_path.exists(), "--keep-on-fail keeps the partial file");
        assert!(
            !target.exists(),
            "but failure must never publish to the target path"
        );
    }

    #[test]
    fn finalize_publishes_and_removes_the_temp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("disk.vhd");
        let t = TempTarget::create(&target, false).expect("create");
        let temp_path = t.path().to_path_buf();
        t.write_at(b"hello", 0).expect("write");
        t.finalize(false).expect("finalize");

        assert!(target.exists());
        assert!(!temp_path.exists());
        assert_eq!(std::fs::read(&target).expect("read"), b"hello");
    }

    #[test]
    fn finalize_without_replace_refuses_an_existing_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("disk.vhd");
        std::fs::write(&target, b"precious").expect("write");

        let t = TempTarget::create(&target, false).expect("create");
        t.write_at(b"new", 0).expect("write");
        assert!(t.finalize(false).is_err(), "must not clobber");

        assert_eq!(
            std::fs::read(&target).expect("read"),
            b"precious",
            "the existing file must survive"
        );
    }

    #[test]
    fn finalize_with_replace_overwrites() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("disk.vhd");
        std::fs::write(&target, b"old").expect("write");

        let t = TempTarget::create(&target, false).expect("create");
        t.write_at(b"new", 0).expect("write");
        t.finalize(true).expect("finalize");

        assert_eq!(std::fs::read(&target).expect("read"), b"new");
    }

    #[test]
    fn concurrent_temp_targets_do_not_collide() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("disk.vhd");
        let a = TempTarget::create(&target, false).expect("a");
        let b = TempTarget::create(&target, false).expect("b");
        assert_ne!(a.path(), b.path(), "O_EXCL must force distinct names");
    }

    #[test]
    fn allocate_reserves_and_sets_the_length() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("a.bin");
        let t = TempTarget::create(&target, false).expect("create");

        let outcome = allocate(t.file(), 1024 * 1024, t.path()).expect("allocate");
        assert_eq!(outcome, AllocOutcome::Reserved);
        assert_eq!(
            t.file().metadata().expect("stat").len(),
            1024 * 1024,
            "mode 0 must extend the length, not only reserve"
        );
    }

    #[test]
    fn zero_region_writes_actual_zeros() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("z.bin");
        let t = TempTarget::create(&target, false).expect("create");
        allocate(t.file(), 128 * 1024, t.path()).expect("allocate");

        // Dirty it first so "all zeros" cannot pass vacuously.
        t.write_at(&[0xAB; 4096], 0).expect("dirty");
        zero_region(t.file(), 0, 128 * 1024, t.path(), &mut NoProgress).expect("zero");

        let mut buf = vec![0xFFu8; 4096];
        t.file().read_exact_at(&mut buf, 0).expect("read");
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn zero_region_respects_its_bounds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("b.bin");
        let t = TempTarget::create(&target, false).expect("create");
        allocate(t.file(), 8192, t.path()).expect("allocate");
        t.write_at(&[0xAB; 8192], 0).expect("dirty");

        zero_region(t.file(), 4096, 8192, t.path(), &mut NoProgress).expect("zero");

        let mut head = vec![0u8; 4096];
        t.file().read_exact_at(&mut head, 0).expect("read head");
        assert!(head.iter().all(|&b| b == 0xAB), "before start must survive");

        let mut tail = vec![0xFFu8; 4096];
        t.file().read_exact_at(&mut tail, 4096).expect("read tail");
        assert!(tail.iter().all(|&b| b == 0), "the range must be zeroed");
    }

    #[test]
    fn zero_region_is_a_no_op_for_an_empty_range() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("c.bin");
        let t = TempTarget::create(&target, false).expect("create");
        allocate(t.file(), 4096, t.path()).expect("allocate");
        zero_region(t.file(), 100, 100, t.path(), &mut NoProgress).expect("no-op");
    }

    #[test]
    fn verify_written_accepts_a_freshly_zeroed_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("v.bin");
        let t = TempTarget::create(&target, false).expect("create");
        allocate(t.file(), 256 * 1024, t.path()).expect("allocate");
        zero_region(t.file(), 0, 256 * 1024, t.path(), &mut NoProgress).expect("zero");

        // Contiguity is a filesystem outcome we cannot force, so a fragmented
        // result here is the filesystem's answer, not a bug.
        match verify_written(t.file(), 256 * 1024, t.path(), true) {
            Ok(()) => {}
            Err(Error::Fragmented { .. }) => {
                eprintln!("skipping: the filesystem did not hand us one run");
            }
            Err(e) => panic!("unexpected: {e}"),
        }
    }

    #[test]
    fn probing_finds_something_on_a_normal_filesystem() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A dev box has room for a few MiB contiguously; the point is that the
        // search terminates and returns a plausible figure rather than looping.
        let got = probe_largest_contiguous(dir.path(), 16 * 1024 * 1024, 1024 * 1024);
        match got {
            Some(n) => {
                assert!(n >= 1024 * 1024, "should find at least the granularity");
                assert!(n <= 16 * 1024 * 1024, "must not exceed the upper bound");
                assert_eq!(n % (1024 * 1024), 0, "must be granularity-aligned");
            }
            None => eprintln!("filesystem granted nothing contiguously; acceptable"),
        }
    }

    #[test]
    fn probing_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _ = probe_largest_contiguous(dir.path(), 8 * 1024 * 1024, 1024 * 1024);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "probe files remain: {leftovers:?}");
    }

    #[test]
    fn probing_with_a_zero_bound_returns_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(probe_largest_contiguous(dir.path(), 0, 1024 * 1024), None);
    }

    #[test]
    fn progress_reports_monotonically_to_completion() {
        struct Rec(Vec<u64>);
        impl Progress for Rec {
            fn update(&mut self, written: u64, _total: u64) {
                self.0.push(written);
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("p.bin");
        let t = TempTarget::create(&target, false).expect("create");
        let len = ZERO_CHUNK as u64 * 2 + 7;
        allocate(t.file(), len, t.path()).expect("allocate");

        let mut rec = Rec(Vec::new());
        zero_region(t.file(), 0, len, t.path(), &mut rec).expect("zero");

        assert!(rec.0.windows(2).all(|w| w[0] < w[1]), "must be monotonic");
        assert_eq!(rec.0.last().copied(), Some(len), "must reach the total");
    }
}

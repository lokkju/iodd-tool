//! Integration tests against a real NTFS volume on a loopback device.
//!
//! **These need `sudo` and are off by default.** Run with:
//!
//! ```text
//! IODD_TEST_SUDO=1 cargo test -p ntfs-contig --features ntfs-integration -- --test-threads=1
//! ```
//!
//! Without both the feature and the environment variable they compile and skip,
//! so `cargo test` on a dev box and in CI stays clean.
//!
//! The volume is made with no partition table, so FIEMAP physical offsets index
//! straight into the backing image. That is what makes it possible to read what
//! is *actually on the device* rather than what the filesystem chooses to
//! report — the whole point of the S1 test below.
#![cfg(feature = "ntfs-integration")]
#![allow(clippy::expect_used, clippy::panic)]

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const PATTERN: &[u8; 16] = b"IODDSTALEPATTERN";

fn enabled() -> bool {
    if std::env::var("IODD_TEST_SUDO").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("skipping: set IODD_TEST_SUDO=1 to run loopback NTFS tests (needs sudo)");
    false
}

fn sh(cmd: &str) -> std::process::Output {
    Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .expect("spawn shell")
}

/// A loopback NTFS volume that unmounts itself.
struct Volume {
    dir: tempfile::TempDir,
    img: PathBuf,
    mnt: PathBuf,
    mounted: bool,
}

impl Volume {
    fn new(size: &str) -> Option<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        let img = dir.path().join("ntfs.img");
        let mnt = dir.path().join("mnt");
        std::fs::create_dir_all(&mnt).expect("mkdir");

        let out = sh(&format!("truncate -s {size} {}", img.display()));
        assert!(out.status.success(), "truncate failed");

        let out = sh(&format!(
            "mkntfs --quick --force --label iodd-test {} 2>&1",
            img.display()
        ));
        if !out.status.success() {
            eprintln!("skipping: mkntfs unavailable or failed");
            return None;
        }

        let out = sh(&format!(
            "sudo mount -t ntfs3 -o loop,uid=$(id -u),gid=$(id -g) {} {}",
            img.display(),
            mnt.display()
        ));
        if !out.status.success() {
            eprintln!(
                "skipping: could not mount ntfs3: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }

        Some(Self {
            dir,
            img,
            mnt,
            mounted: true,
        })
    }

    fn mount_point(&self) -> &Path {
        &self.mnt
    }

    fn unmount(&mut self) {
        if self.mounted {
            let _ = sh(&format!("sync; sudo umount {}", self.mnt.display()));
            self.mounted = false;
        }
    }

    /// Read `len` bytes from the backing image at a physical offset, i.e. what
    /// is really on the device rather than what the filesystem reports.
    fn read_raw(&self, offset: u64, len: usize) -> Vec<u8> {
        let mut f = std::fs::File::open(&self.img).expect("open image");
        f.seek(SeekFrom::Start(offset)).expect("seek");
        let mut buf = vec![0u8; len];
        let n = f.read(&mut buf).expect("read");
        buf.truncate(n);
        buf
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        self.unmount();
        let _ = &self.dir;
    }
}

/// Fill the volume with a recognizable pattern, then delete it, leaving those
/// clusters free but still holding the pattern on the device.
fn dirty_the_free_space(mnt: &Path) {
    let path = mnt.join("filler.bin");
    let block: Vec<u8> = PATTERN
        .iter()
        .copied()
        .cycle()
        .take(4 * 1024 * 1024)
        .collect();
    if let Ok(mut f) = std::fs::File::create(&path) {
        while f.write_all(&block).is_ok() {}
        let _ = f.sync_all();
    }
    let _ = sh("sync");
    let _ = std::fs::remove_file(&path);
    let _ = sh("sync");
}

fn physical_offsets(path: &Path) -> Vec<(u64, u64)> {
    let f = std::fs::File::open(path).expect("open");
    let size = f.metadata().expect("stat").len();
    let map =
        ntfs_contig::extents::map(std::os::fd::AsFd::as_fd(&f), size, path).expect("extent map");
    map.runs.iter().map(|r| (r.physical, r.length)).collect()
}

/// **The S1 regression.** Without a zero pass, allocated clusters keep whatever
/// was on them, and only a raw read can see it — the filesystem synthesizes
/// zeros past the valid data length.
///
/// This asserts the *stale data is really there*, which looks like an odd thing
/// to want. It is what makes `create`'s default-path warning truthful rather
/// than superstitious: if this test ever fails, the warning is wrong and D9
/// needs revisiting.
#[test]
fn without_zeroing_stale_data_survives_on_the_device() {
    if !enabled() {
        return;
    }
    let Some(mut vol) = Volume::new("512M") else {
        return;
    };

    dirty_the_free_space(vol.mount_point());

    let target = vol.mount_point().join("bare.bin");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&target)
        .expect("create");
    ntfs_contig::alloc::allocate(&file, 64 * 1024 * 1024, &target).expect("allocate");
    file.sync_all().expect("sync");

    let runs = physical_offsets(&target);
    assert!(!runs.is_empty(), "must have extents");

    // Through the filesystem: zeros, because it synthesizes them past the VDL.
    let mut via_fs = vec![0xFFu8; 4096];
    std::os::unix::fs::FileExt::read_exact_at(&file, &mut via_fs, 0).expect("read");
    assert!(
        via_fs.iter().all(|&b| b == 0),
        "the filesystem should report zeros"
    );

    drop(file);
    vol.unmount();

    // Off the device: whatever was there before.
    let mut found_pattern = false;
    for (physical, length) in &runs {
        for probe in [0u64, length / 2] {
            let buf = vol.read_raw(physical + probe, 65536);
            if buf.windows(PATTERN.len()).any(|w| w == PATTERN) {
                found_pattern = true;
            }
        }
    }

    assert!(
        found_pattern,
        "the default path's warning claims stale data survives; it did not here, \
         so either the warning or design decision D9 needs revisiting"
    );
}

/// With `--zero`, the same probe must come back clean. This is the half that
/// proves `--zero` actually reaches the device.
#[test]
fn zeroing_really_reaches_the_device() {
    if !enabled() {
        return;
    }
    let Some(mut vol) = Volume::new("512M") else {
        return;
    };

    dirty_the_free_space(vol.mount_point());

    let target = vol.mount_point().join("zeroed.bin");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&target)
        .expect("create");
    let len = 64 * 1024 * 1024;
    ntfs_contig::alloc::allocate(&file, len, &target).expect("allocate");
    ntfs_contig::alloc::zero_region(&file, 0, len, &target, &mut ntfs_contig::alloc::NoProgress)
        .expect("zero");
    file.sync_all().expect("sync");

    let runs = physical_offsets(&target);
    drop(file);
    vol.unmount();

    for (physical, length) in &runs {
        for probe in [0u64, length / 2, length.saturating_sub(65536)] {
            let buf = vol.read_raw(physical + probe, 65536);
            assert!(
                !buf.windows(PATTERN.len()).any(|w| w == PATTERN),
                "stale pattern survived --zero at physical {}",
                physical + probe
            );
        }
    }
}

/// `fallocate` must work on ntfs3 at all, and set the length as well as
/// reserving. S4 measured this; here it is as a regression.
#[test]
fn fallocate_is_supported_and_sets_the_length() {
    if !enabled() {
        return;
    }
    let Some(vol) = Volume::new("256M") else {
        return;
    };

    let target = vol.mount_point().join("a.bin");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&target)
        .expect("create");

    let outcome = ntfs_contig::alloc::allocate(&file, 32 * 1024 * 1024, &target).expect("allocate");
    assert_eq!(
        outcome,
        ntfs_contig::alloc::AllocOutcome::Reserved,
        "ntfs3 supports fallocate; an Unsupported result here contradicts S4"
    );
    assert_eq!(file.metadata().expect("stat").len(), 32 * 1024 * 1024);
}

/// A fresh allocation on ntfs3 carries `UNWRITTEN`, and zeroing clears it.
/// That is S3 and S6, and it is what makes the flag usable as a post-write
/// check rather than a guess.
#[test]
fn unwritten_is_set_by_allocation_and_cleared_by_zeroing() {
    if !enabled() {
        return;
    }
    let Some(vol) = Volume::new("256M") else {
        return;
    };

    let target = vol.mount_point().join("u.bin");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&target)
        .expect("create");
    let len = 32 * 1024 * 1024;
    ntfs_contig::alloc::allocate(&file, len, &target).expect("allocate");
    file.sync_all().expect("sync");

    let fd = std::os::fd::AsFd::as_fd(&file);
    let before = ntfs_contig::extents::map(fd, len, &target).expect("map");
    assert!(
        before.any_unwritten,
        "a fresh fallocate on ntfs3 should be UNWRITTEN (S3)"
    );

    ntfs_contig::alloc::zero_region(&file, 0, len, &target, &mut ntfs_contig::alloc::NoProgress)
        .expect("zero");

    let after = ntfs_contig::extents::map(fd, len, &target).expect("map");
    assert!(
        !after.any_unwritten,
        "writing zeros should clear UNWRITTEN (S6)"
    );
}

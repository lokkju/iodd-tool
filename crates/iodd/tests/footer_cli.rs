//! Phase 3 behaviour through the binary: the full `verify` checklist, the
//! `--strict` semantics from plan item C3, and `retype`.
//!
//! The crate denies unwrap/expect/panic so the write path can never abort on a
//! device holding real data; integration-test helpers are exempt.
#![allow(clippy::expect_used, clippy::panic)]

use assert_cmd::Command;
use iodd::footer::{self, Creator, Footer};
use predicates::str::contains;
use std::io::Write;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

const TS: u32 = 0x31FD_0E4B;
const ID: [u8; 16] = [0x5A; 16];

/// Whether the filesystem happened to give us one extent.
///
/// These tests are about footer logic, but `verify`'s exit code also reflects
/// the two hard gates, and nothing in userspace can *make* a file contiguous —
/// that is the whole finding behind design decision D8. On a busy filesystem a
/// multi-megabyte file may land in several runs, which trips exit 4 before the
/// footer assertions are reached.
///
/// So tests that assert a specific exit code check this first and skip loudly
/// rather than failing on a precondition they cannot control. Tests that only
/// care about reported *output* do not need it, since `verify` prints the footer
/// section before evaluating the gates.
fn is_contiguous(path: &Path) -> bool {
    use std::os::fd::AsFd;
    let Ok(f) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(meta) = f.metadata() else {
        return false;
    };
    iodd::extents::map(f.as_fd(), meta.len(), path).is_ok_and(|m| m.is_contiguous())
}

/// Skip the exit-code half of a test when the filesystem did not cooperate.
macro_rules! require_contiguous {
    ($path:expr) => {
        if !is_contiguous($path) {
            eprintln!(
                "skipping exit-code assertions: {} is fragmented, which is a \
                 filesystem outcome this test cannot control",
                $path.display()
            );
            return;
        }
    };
}

/// Write a real fixed VHD: `virtual_size` zero bytes then the footer. Written
/// with plain writes so it is fully allocated.
fn write_vhd(dir: &Path, name: &str, virtual_size: u64) -> PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).expect("create");
    let block = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    while written < virtual_size {
        let n = (block.len() as u64).min(virtual_size - written) as usize;
        f.write_all(&block[..n]).expect("write data");
        written += n as u64;
    }
    let footer = Footer::new(virtual_size, Creator::DEFAULT, TS, ID);
    f.write_all(&footer.to_bytes()).expect("write footer");
    f.sync_all().expect("sync");
    path
}

fn iodd() -> Command {
    Command::cargo_bin("iodd").expect("binary")
}

/// Patch bytes in place at `offset`.
///
/// Deliberately not `fs::write` of the whole file: rewriting reallocates, which
/// can fragment the file and trip the contiguity gate instead of whatever the
/// test is actually about. That made an earlier version of these tests
/// intermittently fail with exit 4. A positioned write also models what really
/// happens — a guest overwrites the last sector in place.
fn patch_at(path: &Path, offset: u64, bytes: &[u8]) {
    use std::os::unix::fs::FileExt;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for patch");
    f.write_all_at(bytes, offset).expect("patch");
    f.sync_all().expect("sync");
}

#[test]
fn verify_accepts_a_well_formed_fixed_vhd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_vhd(dir.path(), "good.vhd", 1024 * 1024);
    require_contiguous!(&path);

    iodd()
        .args(["verify", &path.to_string_lossy()])
        .assert()
        .success()
        .stdout(contains("footer        valid"))
        .stdout(contains("virtual size  1048576"));
}

#[test]
fn verify_reports_the_geometry_it_generated() {
    let dir = tempfile::tempdir().expect("tempdir");
    const SIZE: u64 = 8 * 1024 * 1024;
    let path = write_vhd(dir.path(), "geo.vhd", SIZE);

    // Computed, not hardcoded: the CHS branches are covered exhaustively
    // against the harvested qemu vectors in the unit tests. This only proves
    // verify reports what footer::chs produced.
    let g = iodd::footer::chs(SIZE / 512);
    let expected = format!(
        "geometry      {}/{}/{}",
        g.cylinders, g.heads, g.sectors_per_track
    );

    iodd()
        .args(["verify", &path.to_string_lossy()])
        .assert()
        .stdout(contains(expected));
}

/// Plan item C3. A footer problem is a warning by default and exit 7 under
/// `--strict`. This is the inverse of what `SPEC.md` line 100 describes, and it
/// follows from D2: the device mounts files with no valid footer at all.
#[test]
fn footer_problems_warn_by_default_and_fail_under_strict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_vhd(dir.path(), "clobbered.vhd", 1024 * 1024);

    // Overwrite the footer's cookie with a GPT backup header, which is exactly
    // what a guest does when it partitions the disk.
    let size = std::fs::metadata(&path).expect("stat").len();
    patch_at(&path, size - footer::FOOTER_SIZE as u64, b"EFI PART");
    require_contiguous!(&path);

    iodd()
        .args(["verify", &path.to_string_lossy()])
        .assert()
        .success()
        .stdout(contains("GPT backup header"))
        .stderr(contains("do not prevent mounting"));

    iodd()
        .args(["verify", &path.to_string_lossy(), "--strict"])
        .assert()
        .code(7);
}

/// The footerless case, which is what the one known-mounting file on the
/// physical device looks like. It must pass the hard gates.
#[test]
fn a_footerless_file_still_passes_the_hard_gates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("raw.rmd");
    let mut f = std::fs::File::create(&path).expect("create");
    f.write_all(&vec![0xAAu8; 512 * 1024]).expect("write");
    f.sync_all().expect("sync");
    drop(f);
    require_contiguous!(&path);

    iodd()
        .args(["verify", &path.to_string_lossy()])
        .assert()
        .success();

    iodd()
        .args(["verify", &path.to_string_lossy(), "--strict"])
        .assert()
        .code(7);
}

#[test]
fn verify_detects_a_dynamic_vhd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_vhd(dir.path(), "dynamic.vhd", 1024 * 1024);

    let size = std::fs::metadata(&path).expect("stat").len();
    let footer_at = size - footer::FOOTER_SIZE as u64;
    patch_at(&path, footer_at + 60, &3u32.to_be_bytes());
    require_contiguous!(&path);

    iodd()
        .args(["verify", &path.to_string_lossy()])
        .assert()
        .success()
        .stdout(contains("dynamic"));
}

#[test]
fn verify_detects_a_vhdx_under_a_vhd_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("container.vhd");
    let mut body = vec![0u8; 1024 * 1024];
    body[0..8].copy_from_slice(b"vhdxfile");
    std::fs::write(&path, &body).expect("write");
    require_contiguous!(&path);

    iodd()
        .args(["verify", &path.to_string_lossy(), "--strict"])
        .assert()
        .code(7);
}

#[test]
fn verify_handles_a_file_too_small_for_a_footer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tiny.vhd");
    std::fs::write(&path, b"short").expect("write");

    iodd()
        .args(["verify", &path.to_string_lossy()])
        .assert()
        .success()
        .stdout(contains("too small"));
}

// ---- retype -------------------------------------------------------------

#[test]
fn retype_renames_without_touching_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let vhd = write_vhd(dir.path(), "disk.vhd", 256 * 1024);

    let before = std::fs::read(&vhd).expect("read");
    let inode_before = std::fs::metadata(&vhd).expect("stat").ino();

    iodd()
        .args(["retype", &vhd.to_string_lossy(), "--to", "removable"])
        .assert()
        .success();

    let rmd = dir.path().join("disk.rmd");
    assert!(rmd.exists(), "target should exist");
    assert!(!vhd.exists(), "source should be gone");
    assert_eq!(std::fs::read(&rmd).expect("read"), before, "bytes changed");
    assert_eq!(
        std::fs::metadata(&rmd).expect("stat").ino(),
        inode_before,
        "a rename must preserve the inode, not copy"
    );
}

#[test]
fn retype_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let vhd = write_vhd(dir.path(), "rt.vhd", 128 * 1024);
    let digest = std::fs::read(&vhd).expect("read");

    iodd()
        .args(["retype", &vhd.to_string_lossy(), "--to", "removable"])
        .assert()
        .success();
    let rmd = dir.path().join("rt.rmd");
    iodd()
        .args(["retype", &rmd.to_string_lossy(), "--to", "fixed"])
        .assert()
        .success();

    assert!(vhd.exists());
    assert_eq!(std::fs::read(&vhd).expect("read"), digest);
}

#[test]
fn retype_is_a_no_op_when_already_the_target_type() {
    let dir = tempfile::tempdir().expect("tempdir");
    let vhd = write_vhd(dir.path(), "same.vhd", 64 * 1024);

    iodd()
        .args(["retype", &vhd.to_string_lossy(), "--to", "fixed"])
        .assert()
        .success()
        .stdout(contains("nothing to do"));
    assert!(vhd.exists());
}

/// Exit 2 is "target exists". retype has no --force, so it must never clobber.
#[test]
fn retype_refuses_to_clobber_an_existing_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let vhd = write_vhd(dir.path(), "clash.vhd", 64 * 1024);
    std::fs::write(dir.path().join("clash.rmd"), b"precious").expect("write");

    iodd()
        .args(["retype", &vhd.to_string_lossy(), "--to", "removable"])
        .assert()
        .code(2);

    assert_eq!(
        std::fs::read(dir.path().join("clash.rmd")).expect("read"),
        b"precious",
        "the existing file must be untouched"
    );
    assert!(vhd.exists(), "the source must be untouched");
}

#[test]
fn retype_rejects_other_extensions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let iso = dir.path().join("image.iso");
    std::fs::write(&iso, b"x").expect("write");

    iodd()
        .args(["retype", &iso.to_string_lossy(), "--to", "removable"])
        .assert()
        .code(1);
}

#[test]
fn retype_reports_a_missing_file() {
    iodd()
        .args(["retype", "/nonexistent-zzz.vhd", "--to", "removable"])
        .assert()
        .code(1);
}

/// Design D3: `--strict` governs *footer* findings only. The hard gates fire
/// with or without it, because the device rejects such a file regardless of
/// which flags we were invoked with. The engine-side sparseness test lives in
/// the ntfs-contig crate; this asserts iodd's flag does not soften it.
#[test]
fn strict_does_not_change_the_hard_gates() {
    use std::os::unix::fs::MetadataExt as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sparse.vhd");

    let f = std::fs::File::create(&path).expect("create");
    f.set_len(8 * 1024 * 1024).expect("truncate");
    f.sync_all().expect("sync");
    drop(f);

    let meta = std::fs::metadata(&path).expect("stat");
    if meta.blocks() * 512 >= meta.len() {
        eprintln!("filesystem does not create sparse files here; skipping");
        return;
    }

    for args in [
        vec!["verify", &*path.to_string_lossy()],
        vec!["verify", &*path.to_string_lossy(), "--strict"],
    ] {
        iodd().args(&args).assert().code(5);
    }
}

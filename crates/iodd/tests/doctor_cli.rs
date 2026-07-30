//! Phase 5: `doctor` through the binary.
//!
//! The verdict logic lives in unit tests, where every fixture is a struct
//! literal. What is worth testing here is the parts that need a real
//! filesystem: traversal, type filtering, the aggregate exit code, and the
//! read-only guarantee.
//!
//! The crate denies unwrap/expect/panic so the write path can never abort on a
//! device holding real data; integration-test helpers are exempt.
#![allow(clippy::expect_used, clippy::panic)]

use assert_cmd::Command;
use iodd::footer::{Creator, Footer};
use predicates::str::contains;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

fn iodd() -> Command {
    Command::cargo_bin("iodd").expect("binary")
}

/// A real fixed VHD: data then footer, fully allocated.
fn write_vhd(path: &Path, virtual_size: u64) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    let mut f = std::fs::File::create(path).expect("create");
    let block = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    while written < virtual_size {
        let n = (block.len() as u64).min(virtual_size - written) as usize;
        f.write_all(&block[..n]).expect("write");
        written += n as u64;
    }
    let footer = Footer::new(virtual_size, Creator::DEFAULT, 0x3000_0000, [9u8; 16]);
    f.write_all(&footer.to_bytes()).expect("footer");
    f.sync_all().expect("sync");
}

fn write_raw(path: &Path, len: usize, fill: u8) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, vec![fill; len]).expect("write");
}

/// Snapshot of everything a read-only tool must not disturb.
fn snapshot(root: &Path) -> Vec<(PathBuf, i64, i64, u64, u64)> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if entry.file_type().is_file() {
            let m = entry.metadata().expect("stat");
            out.push((
                entry.path().to_path_buf(),
                m.mtime(),
                m.ctime(),
                m.blocks(),
                m.len(),
            ));
        }
    }
    out.sort();
    out
}

#[test]
fn an_empty_tree_reports_nothing_and_succeeds() {
    let dir = tempfile::tempdir().expect("tempdir");
    iodd()
        .args(["doctor", &dir.path().to_string_lossy()])
        .assert()
        .success()
        .stdout(contains("no IODD-mountable files"));
}

#[test]
fn a_missing_root_is_a_usage_error() {
    iodd().args(["doctor", "/nonexistent-zzz"]).assert().code(1);
}

#[test]
fn an_unknown_type_filter_is_a_usage_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    iodd()
        .args(["doctor", &dir.path().to_string_lossy(), "--type", "exe"])
        .assert()
        .code(1);
}

/// Design revision 8 / plan OPEN-1. The real device keeps everything in
/// `_ISO/`, `VHDs/` and `tools/`, so a top-level-only default would report
/// nothing on a device full of images.
#[test]
fn nested_files_are_found_by_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_vhd(&dir.path().join("VHDs/nested.vhd"), 1024 * 1024);
    write_raw(&dir.path().join("_ISO/deep/img.iso"), 64 * 1024, 0x11);

    iodd()
        .args(["doctor", &dir.path().to_string_lossy()])
        .assert()
        .success()
        .stdout(contains("nested.vhd"))
        .stdout(contains("img.iso"));
}

#[test]
fn no_recursive_scans_only_the_top_level() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_vhd(&dir.path().join("top.vhd"), 1024 * 1024);
    write_vhd(&dir.path().join("VHDs/nested.vhd"), 1024 * 1024);

    iodd()
        .args(["doctor", &dir.path().to_string_lossy(), "--no-recursive"])
        .assert()
        .success()
        .stdout(contains("top.vhd"));

    let out = iodd()
        .args(["doctor", &dir.path().to_string_lossy(), "--no-recursive"])
        .output()
        .expect("run");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("nested.vhd"),
        "--no-recursive must not descend"
    );
}

#[test]
fn type_filtering_restricts_what_is_scanned() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_vhd(&dir.path().join("a.vhd"), 1024 * 1024);
    write_raw(&dir.path().join("b.iso"), 64 * 1024, 0x22);

    let out = iodd()
        .args(["doctor", &dir.path().to_string_lossy(), "--type", "vhd"])
        .output()
        .expect("run");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("a.vhd"));
    assert!(!text.contains("b.iso"), "iso should be filtered out");
}

#[test]
fn unrecognized_extensions_are_ignored_entirely() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_raw(&dir.path().join("notes.txt"), 1024, 0x33);
    write_raw(&dir.path().join("backup.img"), 1024, 0x44);

    iodd()
        .args(["doctor", &dir.path().to_string_lossy()])
        .assert()
        .success()
        .stdout(contains("no IODD-mountable files"));
}

/// Temp files from an interrupted `create` are dot-prefixed precisely so an
/// audit skips them.
#[test]
fn temp_files_from_create_are_not_scanned() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_raw(&dir.path().join(".iodd-1234-0.tmp"), 4096, 0x55);

    iodd()
        .args(["doctor", &dir.path().to_string_lossy()])
        .assert()
        .success()
        .stdout(contains("no IODD-mountable files"));
}

#[test]
fn a_fragmented_iso_over_the_ceiling_fails_the_aggregate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let iso = dir.path().join("frag.iso");
    write_raw(&iso, 64 * 1024, 0x66);

    // Forcing real fragmentation is not possible, so drive the ceiling to zero:
    // any file then exceeds it, which exercises the same code path.
    iodd()
        .args([
            "doctor",
            &dir.path().to_string_lossy(),
            "--iso-max-fragments",
            "0",
        ])
        .assert()
        .code(4)
        .stdout(contains("WILL-NOT-MOUNT"));
}

#[test]
fn strict_promotes_warnings_to_a_failing_exit() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A footerless file under .rmd: mountable, but with a footer warning.
    write_raw(&dir.path().join("raw.rmd"), 512 * 1024, 0x77);

    iodd()
        .args(["doctor", &dir.path().to_string_lossy()])
        .assert()
        .success()
        .stdout(contains("WARN"));

    iodd()
        .args(["doctor", &dir.path().to_string_lossy(), "--strict"])
        .assert()
        .code(4);
}

#[test]
fn json_output_is_parseable_and_has_stable_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_vhd(&dir.path().join("a.vhd"), 1024 * 1024);

    let out = iodd()
        .args(["doctor", &dir.path().to_string_lossy(), "--format", "json"])
        .output()
        .expect("run");
    let text = String::from_utf8_lossy(&out.stdout);

    for field in [
        "\"path\"",
        "\"type\"",
        "\"size\"",
        "\"extents\"",
        "\"verdict\"",
        "\"reasons\"",
    ] {
        assert!(text.contains(field), "missing {field} in {text}");
    }
    assert!(text.trim_start().starts_with('{'), "must be a JSON object");
    assert!(text.contains("\"files\""), "files array must be present");
}

/// `SPEC.md` line 150: doctor never modifies, renames, or deletes anything.
/// Asserted against mtime, ctime, block count and length, so a write of any
/// kind would show up.
#[test]
fn doctor_writes_absolutely_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_vhd(&dir.path().join("VHDs/a.vhd"), 1024 * 1024);
    write_raw(&dir.path().join("_ISO/b.iso"), 128 * 1024, 0x88);
    write_raw(&dir.path().join("c.rmd"), 64 * 1024, 0x99);
    write_raw(&dir.path().join("d.ima"), 32 * 1024, 0xAA);

    let before = snapshot(dir.path());

    for extra in [
        vec![],
        vec!["--strict"],
        vec!["--format", "json"],
        vec!["--no-recursive"],
    ] {
        let mut cmd = iodd();
        cmd.args(["doctor", &dir.path().to_string_lossy()]);
        cmd.args(&extra);
        let _ = cmd.output().expect("run");
    }

    assert_eq!(
        snapshot(dir.path()),
        before,
        "doctor must not alter mtime, ctime, block count, or length"
    );
}

/// A symlink must not be followed out of the tree; the audit reports on what is
/// there, not on what it points at.
#[test]
fn symlinks_are_not_followed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside");
    write_vhd(&outside.path().join("secret.vhd"), 1024 * 1024);

    std::os::unix::fs::symlink(
        outside.path().join("secret.vhd"),
        dir.path().join("link.vhd"),
    )
    .expect("symlink");

    let out = iodd()
        .args(["doctor", &dir.path().to_string_lossy()])
        .output()
        .expect("run");
    let text = String::from_utf8_lossy(&out.stdout);
    // It may appear as unreadable, but must never be graded as a real file.
    assert!(
        !text.contains("MOUNTABLE"),
        "a symlink must not be followed and graded: {text}"
    );
}

#[test]
fn the_summary_counts_by_verdict() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_vhd(&dir.path().join("good.vhd"), 1024 * 1024);
    write_raw(&dir.path().join("warn.rmd"), 512 * 1024, 0xBB);

    iodd()
        .args(["doctor", &dir.path().to_string_lossy()])
        .assert()
        .success()
        .stdout(contains("2 file(s):"))
        .stdout(contains("mountable"))
        .stdout(contains("warn"));
}

/// The vendor documents a 32-item limit per directory containing images.
/// Exceeding it does not break the files — the device simply stops listing
/// them, so a file that copied perfectly is missing from the menu.
#[test]
fn an_over_full_directory_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sub = dir.path().join("_ISO");
    std::fs::create_dir_all(&sub).expect("mkdir");
    for i in 0..40 {
        write_raw(&sub.join(format!("i{i:03}.iso")), 4096, 0x10);
    }

    iodd()
        .args(["doctor", &dir.path().to_string_lossy()])
        .assert()
        .success()
        .stdout(contains("32-item limit"))
        .stdout(contains("40 items"));
}

#[test]
fn a_directory_at_the_limit_is_not_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sub = dir.path().join("_ISO");
    std::fs::create_dir_all(&sub).expect("mkdir");
    for i in 0..32 {
        write_raw(&sub.join(format!("i{i:03}.iso")), 4096, 0x10);
    }

    let out = iodd()
        .args(["doctor", &dir.path().to_string_lossy()])
        .output()
        .expect("run");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("32-item limit"),
        "exactly 32 is within the limit"
    );
}

#[test]
fn strict_fails_on_an_over_full_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sub = dir.path().join("_ISO");
    std::fs::create_dir_all(&sub).expect("mkdir");
    for i in 0..40 {
        write_raw(&sub.join(format!("i{i:03}.iso")), 4096, 0x10);
    }

    iodd()
        .args(["doctor", &dir.path().to_string_lossy(), "--strict"])
        .assert()
        .code(4);
}

#[test]
fn directory_findings_appear_in_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sub = dir.path().join("_ISO");
    std::fs::create_dir_all(&sub).expect("mkdir");
    for i in 0..40 {
        write_raw(&sub.join(format!("i{i:03}.iso")), 4096, 0x10);
    }

    let out = iodd()
        .args(["doctor", &dir.path().to_string_lossy(), "--format", "json"])
        .output()
        .expect("run");
    let text = String::from_utf8_lossy(&out.stdout);
    for field in ["\"files\"", "\"directories\"", "\"items\"", "\"limit\""] {
        assert!(text.contains(field), "missing {field}");
    }
}

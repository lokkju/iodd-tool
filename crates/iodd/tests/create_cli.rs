//! Phase 4: the write path, through the binary.
//!
//! The crate denies unwrap/expect/panic so the write path can never abort on a
//! device holding real data; integration-test helpers are exempt.
#![allow(clippy::expect_used, clippy::panic)]

use assert_cmd::Command;
use iodd::footer;
use predicates::str::contains;
use std::path::Path;

fn iodd() -> Command {
    Command::cargo_bin("iodd").expect("binary")
}

/// No `.iodd-*.tmp` may survive a run, successful or not.
fn assert_no_temp_files(dir: &Path) {
    let leftovers: Vec<_> = std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".iodd-") && n.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

#[test]
fn create_makes_a_file_of_virtual_size_plus_512() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("disk");

    iodd()
        .args(["create", "--size", "16M", "--out", &out.to_string_lossy()])
        .assert()
        .success();

    let made = dir.path().join("disk.vhd");
    assert!(made.exists(), "the .vhd extension must be applied");
    assert_eq!(
        std::fs::metadata(&made).expect("stat").len(),
        16 * 1024 * 1024 + 512,
        "file must be virtual_size + 512"
    );
    assert_no_temp_files(dir.path());
}

#[test]
fn the_footer_it_writes_parses_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("f.vhd");

    iodd()
        .args(["create", "--size", "16M", "--out", &out.to_string_lossy()])
        .assert()
        .success();

    let bytes = std::fs::read(&out).expect("read");
    let mut raw = [0u8; footer::FOOTER_SIZE];
    raw.copy_from_slice(&bytes[bytes.len() - footer::FOOTER_SIZE..]);

    let (parsed, problems) = footer::parse(&raw, Some(16 * 1024 * 1024));
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(parsed.virtual_size, 16 * 1024 * 1024);
    assert_eq!(parsed.disk_type, footer::DISK_TYPE_FIXED);
    assert_eq!(parsed.data_offset, footer::DATA_OFFSET_FIXED);
    assert_eq!(parsed.geometry, footer::chs(16 * 1024 * 1024 / 512));
}

/// The important round-trip: what `create` produces by default, `verify` must
/// accept. An earlier version failed this, because `verify` treated
/// uninitialized extents as a hard gate while D9 makes them the default state.
#[test]
fn what_create_makes_by_default_verify_accepts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("d.vhd");

    iodd()
        .args(["create", "--size", "16M", "--out", &out.to_string_lossy()])
        .assert()
        .success()
        .stdout(contains("zeroed        no"))
        .stderr(contains("NOT zeroed"));

    iodd()
        .args(["verify", &out.to_string_lossy()])
        .assert()
        .success()
        .stderr(contains("uninitialized extents"));

    // The finding is real, so --strict must still surface it.
    iodd()
        .args(["verify", &out.to_string_lossy(), "--strict"])
        .assert()
        .code(7);
}

#[test]
fn zero_produces_a_file_that_passes_strict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("z.vhd");

    iodd()
        .args([
            "create",
            "--size",
            "16M",
            "--out",
            &out.to_string_lossy(),
            "--zero",
        ])
        .assert()
        .success()
        .stdout(contains("zeroed        yes"));

    iodd()
        .args(["verify", &out.to_string_lossy(), "--strict"])
        .assert()
        .success();

    // And the bytes really are zero, not merely reported so.
    let bytes = std::fs::read(&out).expect("read");
    assert!(
        bytes[..bytes.len() - footer::FOOTER_SIZE]
            .iter()
            .all(|&b| b == 0),
        "the data region must be zeros"
    );
}

#[test]
fn removable_defaults_to_rmd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("r");

    iodd()
        .args([
            "create",
            "--size",
            "16M",
            "--out",
            &out.to_string_lossy(),
            "--removable",
        ])
        .assert()
        .success();

    assert!(dir.path().join("r.rmd").exists());
    assert!(!dir.path().join("r.vhd").exists());
}

/// `.vhd` and `.rmd` are **not** the same format, which hardware measurement
/// established after `SPEC.md` lines 26-28 claimed otherwise.
///
/// The device presents a `.vhd` as file size minus 512, treating the last
/// sector as a footer, and presents a `.rmd` whole. So for a guest to see the
/// size that was asked for, a `.rmd` must be exactly `virtual_size` with no
/// footer, while a `.vhd` is `virtual_size + 512` with one.
#[test]
fn removable_is_a_raw_image_and_fixed_is_a_vhd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let vhd = dir.path().join("a.vhd");
    let rmd = dir.path().join("b.rmd");

    iodd()
        .args(["create", "--size", "16M", "--out", &vhd.to_string_lossy()])
        .assert()
        .success();
    iodd()
        .args([
            "create",
            "--size",
            "16M",
            "--out",
            &rmd.to_string_lossy(),
            "--removable",
        ])
        .assert()
        .success()
        .stdout(contains("no footer"));

    let vhd_len = std::fs::metadata(&vhd).expect("stat").len();
    let rmd_len = std::fs::metadata(&rmd).expect("stat").len();

    assert_eq!(vhd_len, 16 * 1024 * 1024 + 512, "a .vhd carries a footer");
    assert_eq!(rmd_len, 16 * 1024 * 1024, "a .rmd does not");

    // The point of the difference: both give the guest exactly 16 MiB.
    assert_eq!(vhd_len - 512, rmd_len, "guest-visible sizes must match");

    // And the .rmd really has no footer where one would be.
    let tail = std::fs::read(&rmd).expect("read");
    assert_ne!(
        &tail[tail.len() - 512..tail.len() - 504],
        b"conectix",
        "a raw removable image must not end in a VHD footer"
    );
}

/// An explicit `.rmd` name gets raw treatment even without `--removable`, since
/// the device decides by extension and nothing else.
#[test]
fn an_explicit_rmd_extension_also_means_raw() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("explicit.rmd");

    iodd()
        .args(["create", "--size", "16M", "--out", &out.to_string_lossy()])
        .assert()
        .success();

    assert_eq!(
        std::fs::metadata(&out).expect("stat").len(),
        16 * 1024 * 1024,
        "an .rmd is raw however it was requested"
    );
}

#[test]
fn an_existing_target_is_refused_without_force() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("e.vhd");
    std::fs::write(&out, b"precious").expect("write");

    iodd()
        .args(["create", "--size", "16M", "--out", &out.to_string_lossy()])
        .assert()
        .code(2);

    assert_eq!(
        std::fs::read(&out).expect("read"),
        b"precious",
        "the existing file must be untouched"
    );
    assert_no_temp_files(dir.path());
}

#[test]
fn force_overwrites() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("o.vhd");
    std::fs::write(&out, b"old").expect("write");

    iodd()
        .args([
            "create",
            "--size",
            "16M",
            "--out",
            &out.to_string_lossy(),
            "--force",
        ])
        .assert()
        .success();

    assert_eq!(
        std::fs::metadata(&out).expect("stat").len(),
        16 * 1024 * 1024 + 512
    );
    assert_no_temp_files(dir.path());
}

/// The reason the temp-file design exists. A failure after `--force` was given
/// must leave the original intact, where a straight overwrite would have
/// destroyed it before discovering it could not succeed.
#[test]
fn a_failed_force_leaves_the_original_intact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("keep.vhd");
    std::fs::write(&out, b"irreplaceable").expect("write");

    // Far larger than any plausible free space, so allocation fails.
    iodd()
        .args([
            "create",
            "--size",
            "900T",
            "--out",
            &out.to_string_lossy(),
            "--force",
        ])
        .assert()
        .failure();

    assert_eq!(
        std::fs::read(&out).expect("read"),
        b"irreplaceable",
        "a failed run must never destroy the original"
    );
    assert_no_temp_files(dir.path());
}

#[test]
fn keep_on_fail_keeps_the_partial_but_does_not_publish() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("kf.vhd");

    iodd()
        .args([
            "create",
            "--size",
            "900T",
            "--out",
            &out.to_string_lossy(),
            "--keep-on-fail",
        ])
        .assert()
        .failure();

    assert!(!out.exists(), "failure must never publish to the target");

    let temps: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read_dir")
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".iodd-"))
        .collect();
    assert_eq!(temps.len(), 1, "the partial file should be kept: {temps:?}");
}

#[test]
fn a_misaligned_size_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("m.vhd");

    iodd()
        .args([
            "create",
            "--size",
            "1000001",
            "--out",
            &out.to_string_lossy(),
        ])
        .assert()
        .code(1);
    assert!(!out.exists());
}

#[test]
fn a_tiny_size_warns_about_the_vendor_minimum() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("t.vhd");

    iodd()
        .args(["create", "--size", "1M", "--out", &out.to_string_lossy()])
        .assert()
        .success()
        .stderr(contains("12 MiB minimum"));
}

#[test]
fn a_bad_creator_tag_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("c.vhd");

    for bad in ["toolong", ""] {
        iodd()
            .args([
                "create",
                "--size",
                "16M",
                "--out",
                &out.to_string_lossy(),
                "--creator",
                bad,
            ])
            .assert()
            .code(1);
    }
    assert!(!out.exists());
}

#[test]
fn the_creator_tag_reaches_the_footer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("ct.vhd");

    iodd()
        .args([
            "create",
            "--size",
            "16M",
            "--out",
            &out.to_string_lossy(),
            "--creator",
            "abcd",
        ])
        .assert()
        .success();

    let bytes = std::fs::read(&out).expect("read");
    let base = bytes.len() - footer::FOOTER_SIZE;
    assert_eq!(&bytes[base + 28..base + 32], b"abcd");
}

#[test]
fn a_nonexistent_directory_is_a_usage_error() {
    iodd()
        .args(["create", "--size", "16M", "--out", "/nonexistent-zzz/x.vhd"])
        .assert()
        .code(1);
}

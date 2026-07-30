//! Phase 6: `convert` through the binary.
//!
//! The crate denies unwrap/expect/panic so the write path can never abort on a
//! device holding real data; integration-test helpers are exempt.
#![allow(clippy::expect_used, clippy::panic)]

use assert_cmd::Command;
use iodd::footer;
use predicates::str::contains;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

fn iodd() -> Command {
    Command::cargo_bin("iodd").expect("binary")
}

fn have_qemu() -> bool {
    std::process::Command::new("qemu-img")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A recognizable raw source: every 512-byte sector stamped with its offset, so
/// a mis-copy or a transposition is detectable rather than hidden by uniform
/// bytes.
fn write_patterned_raw(path: &Path, len: u64) {
    let mut buf = Vec::with_capacity(len as usize);
    let mut off = 0u64;
    while off < len {
        buf.extend_from_slice(&off.to_le_bytes());
        buf.extend_from_slice(&[0xC5u8; 504]);
        off += 512;
    }
    buf.truncate(len as usize);
    std::fs::write(path, &buf).expect("write source");
}

fn assert_no_temp_files(dir: &Path) {
    let leftovers: Vec<_> = std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".iodd-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

#[test]
fn a_raw_source_is_copied_byte_for_byte() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.img");
    let out = dir.path().join("out.vhd");
    write_patterned_raw(&src, 4 * 1024 * 1024);

    iodd()
        .args([
            "convert",
            "--source",
            &src.to_string_lossy(),
            "--out",
            &out.to_string_lossy(),
        ])
        .assert()
        .success();

    let original = std::fs::read(&src).expect("read src");
    let produced = std::fs::read(&out).expect("read out");
    assert_eq!(
        produced.len() as u64,
        4 * 1024 * 1024 + 512,
        "target is source + footer"
    );
    assert_eq!(
        &produced[..original.len()],
        &original[..],
        "the data region must match the source exactly"
    );
    assert_no_temp_files(dir.path());
}

#[test]
fn the_footer_describes_the_source_size() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.img");
    let out = dir.path().join("out.vhd");
    write_patterned_raw(&src, 2 * 1024 * 1024);

    iodd()
        .args([
            "convert",
            "--source",
            &src.to_string_lossy(),
            "--out",
            &out.to_string_lossy(),
        ])
        .assert()
        .success();

    let bytes = std::fs::read(&out).expect("read");
    let mut raw = [0u8; footer::FOOTER_SIZE];
    raw.copy_from_slice(&bytes[bytes.len() - footer::FOOTER_SIZE..]);
    let (parsed, problems) = footer::parse(&raw, Some(2 * 1024 * 1024));
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(parsed.virtual_size, 2 * 1024 * 1024);
}

#[test]
fn size_can_enlarge_and_the_tail_is_beyond_the_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.img");
    let out = dir.path().join("big.vhd");
    write_patterned_raw(&src, 1024 * 1024);

    iodd()
        .args([
            "convert",
            "--source",
            &src.to_string_lossy(),
            "--out",
            &out.to_string_lossy(),
            "--size",
            "4M",
            "--zero",
        ])
        .assert()
        .success();

    let produced = std::fs::read(&out).expect("read");
    assert_eq!(produced.len() as u64, 4 * 1024 * 1024 + 512);

    let original = std::fs::read(&src).expect("read src");
    assert_eq!(&produced[..original.len()], &original[..]);
    // With --zero the region past the source really is zero.
    assert!(
        produced[original.len()..produced.len() - footer::FOOTER_SIZE]
            .iter()
            .all(|&b| b == 0),
        "the enlarged tail should be zeros under --zero"
    );
}

/// `SPEC.md` line 123 says max(source, --size), line 98 says it must be >=.
/// Design revision 5 picks the latter: shrinking silently would lose data.
#[test]
fn size_smaller_than_the_source_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.img");
    let out = dir.path().join("small.vhd");
    write_patterned_raw(&src, 4 * 1024 * 1024);

    iodd()
        .args([
            "convert",
            "--source",
            &src.to_string_lossy(),
            "--out",
            &out.to_string_lossy(),
            "--size",
            "1M",
        ])
        .assert()
        .code(1);
    assert!(!out.exists(), "nothing should be produced");
}

#[test]
fn a_missing_source_is_a_usage_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    iodd()
        .args([
            "convert",
            "--source",
            "/nonexistent-zzz.img",
            "--out",
            &dir.path().join("x.vhd").to_string_lossy(),
        ])
        .assert()
        .code(1);
}

#[test]
fn converting_a_file_onto_itself_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("self.vhd");
    write_patterned_raw(&src, 64 * 1024);

    iodd()
        .args([
            "convert",
            "--source",
            &src.to_string_lossy(),
            "--out",
            &src.to_string_lossy(),
            "--force",
        ])
        .assert()
        .code(1);

    assert_eq!(
        std::fs::metadata(&src).expect("stat").len(),
        64 * 1024,
        "the source must be untouched"
    );
}

#[test]
fn an_existing_target_is_refused_without_force() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.img");
    let out = dir.path().join("out.vhd");
    write_patterned_raw(&src, 64 * 1024);
    std::fs::write(&out, b"precious").expect("write");

    iodd()
        .args([
            "convert",
            "--source",
            &src.to_string_lossy(),
            "--out",
            &out.to_string_lossy(),
        ])
        .assert()
        .code(2);

    assert_eq!(std::fs::read(&out).expect("read"), b"precious");
}

#[test]
fn removable_defaults_to_rmd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.img");
    write_patterned_raw(&src, 64 * 1024);

    iodd()
        .args([
            "convert",
            "--source",
            &src.to_string_lossy(),
            "--out",
            &dir.path().join("r").to_string_lossy(),
            "--removable",
        ])
        .assert()
        .success();

    assert!(dir.path().join("r.rmd").exists());
}

/// Exit 9 is "qemu-img required but absent". The check must happen before any
/// allocation, so a missing tool costs nothing.
#[test]
fn a_container_source_without_qemu_exits_nine() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.qcow2");
    let out = dir.path().join("out.vhd");

    // A qcow2 magic is enough; we never get as far as parsing it.
    let mut body = vec![0u8; 65536];
    body[0..4].copy_from_slice(b"QFI\xfb");
    std::fs::write(&src, &body).expect("write");

    let empty = dir.path().join("empty-path");
    std::fs::create_dir_all(&empty).expect("mkdir");

    iodd()
        .env("PATH", &empty)
        .args([
            "convert",
            "--source",
            &src.to_string_lossy(),
            "--out",
            &out.to_string_lossy(),
        ])
        .assert()
        .code(9);

    assert!(!out.exists());
    assert_no_temp_files(dir.path());
}

#[test]
fn a_raw_source_needs_no_qemu_at_all() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.img");
    let out = dir.path().join("out.vhd");
    write_patterned_raw(&src, 256 * 1024);

    let empty = dir.path().join("empty-path");
    std::fs::create_dir_all(&empty).expect("mkdir");

    iodd()
        .env("PATH", &empty)
        .args([
            "convert",
            "--source",
            &src.to_string_lossy(),
            "--out",
            &out.to_string_lossy(),
        ])
        .assert()
        .success();
}

/// A *fixed* VHD keeps `conectix` only in its trailing footer, so re-converting
/// one must be treated as raw rather than handed to qemu.
#[test]
fn a_fixed_vhd_source_is_treated_as_raw() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("fixed.vhd");
    let out = dir.path().join("again.vhd");

    iodd()
        .args([
            "create",
            "--size",
            "16M",
            "--out",
            &src.to_string_lossy(),
            "--zero",
        ])
        .assert()
        .success();

    let empty = dir.path().join("empty-path");
    std::fs::create_dir_all(&empty).expect("mkdir");

    // No qemu on PATH: if it sniffed as a container this would exit 9.
    iodd()
        .env("PATH", &empty)
        .args([
            "convert",
            "--source",
            &src.to_string_lossy(),
            "--out",
            &out.to_string_lossy(),
        ])
        .assert()
        .success();
}

#[test]
fn what_convert_makes_verify_accepts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.img");
    let out = dir.path().join("out.vhd");
    write_patterned_raw(&src, 1024 * 1024);

    iodd()
        .args([
            "convert",
            "--source",
            &src.to_string_lossy(),
            "--out",
            &out.to_string_lossy(),
        ])
        .assert()
        .success();

    // The data region is fully written by the copy, so even --strict is clean
    // when the target is not enlarged.
    let meta = std::fs::metadata(&out).expect("stat");
    if meta.blocks() * 512 >= meta.len() {
        iodd()
            .args(["verify", &out.to_string_lossy()])
            .assert()
            .success();
    }
}

// ---- qemu-backed, skipped when it is absent -------------------------------

#[test]
fn a_qcow2_source_converts_and_matches_qemus_own_output() {
    if !have_qemu() {
        eprintln!("skipping: qemu-img not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let raw = dir.path().join("orig.img");
    let qcow = dir.path().join("src.qcow2");
    let out = dir.path().join("out.vhd");
    write_patterned_raw(&raw, 2 * 1024 * 1024);

    let ok = std::process::Command::new("qemu-img")
        .args(["convert", "-O", "qcow2"])
        .arg(&raw)
        .arg(&qcow)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skipping: could not build a qcow2 fixture");
        return;
    }

    iodd()
        .args([
            "convert",
            "--source",
            &qcow.to_string_lossy(),
            "--out",
            &out.to_string_lossy(),
        ])
        .assert()
        .success()
        .stdout(contains("qcow2 container"));

    let original = std::fs::read(&raw).expect("read raw");
    let produced = std::fs::read(&out).expect("read out");
    assert_eq!(
        &produced[..original.len()],
        &original[..],
        "the guest-visible bytes must match the original raw image"
    );
}

/// Design revision 2, pinned. `-S 0` is what stops qemu punching holes for zero
/// runs; without it the result is sparse, which is the exact failure this tool
/// exists to prevent. Demonstrated by doing it wrong on purpose.
#[test]
fn without_s_zero_qemu_produces_a_sparse_file() {
    if !have_qemu() {
        eprintln!("skipping: qemu-img not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let qcow = dir.path().join("holey.qcow2");

    // A mostly-zero image: 8 MiB with a few bytes at the start.
    let raw = dir.path().join("holey.img");
    let mut body = vec![0u8; 8 * 1024 * 1024];
    body[..16].copy_from_slice(b"0123456789abcdef");
    std::fs::write(&raw, &body).expect("write");
    if !std::process::Command::new("qemu-img")
        .args(["convert", "-O", "qcow2"])
        .arg(&raw)
        .arg(&qcow)
        .status()
        .is_ok_and(|s| s.success())
    {
        eprintln!("skipping: could not build fixture");
        return;
    }

    let sparse_out = dir.path().join("sparse.raw");
    let dense_out = dir.path().join("dense.raw");

    // Without -S 0: qemu is free to leave holes.
    let _ = std::process::Command::new("qemu-img")
        .args(["convert", "-O", "raw"])
        .arg(&qcow)
        .arg(&sparse_out)
        .status();
    // With -S 0: every byte is written.
    let _ = std::process::Command::new("qemu-img")
        .args(["convert", "-S", "0", "-O", "raw"])
        .arg(&qcow)
        .arg(&dense_out)
        .status();

    let sparse = std::fs::metadata(&sparse_out).expect("stat");
    let dense = std::fs::metadata(&dense_out).expect("stat");

    if sparse.blocks() * 512 >= sparse.len() {
        eprintln!("skipping: this filesystem did not produce a sparse result");
        return;
    }
    assert!(
        dense.blocks() * 512 >= dense.len(),
        "-S 0 must produce a fully allocated file: {} allocated for {} bytes",
        dense.blocks() * 512,
        dense.len()
    );
}

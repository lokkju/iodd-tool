//! `ntfs-contig copy` and `fix` at the command line.

#![allow(clippy::expect_used, clippy::panic)]

use assert_cmd::Command;
use std::path::Path;

fn bin() -> Command {
    Command::cargo_bin("ntfs-contig").expect("binary")
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).expect("write");
    p
}

#[test]
fn copy_produces_an_identical_target() {
    let dir = tempfile::tempdir().expect("dir");
    let bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 253) as u8).collect();
    let src = write(dir.path(), "src.bin", &bytes);
    let dst = dir.path().join("dst.bin");

    bin().arg("copy").arg(&src).arg(&dst).assert().success();
    assert_eq!(std::fs::read(&dst).expect("read"), bytes);
}

#[test]
fn copy_refuses_an_existing_target_without_force() {
    let dir = tempfile::tempdir().expect("dir");
    let src = write(dir.path(), "src.bin", b"new");
    let dst = write(dir.path(), "dst.bin", b"keep me");

    bin()
        .arg("copy")
        .arg(&src)
        .arg(&dst)
        .assert()
        .failure()
        .stderr(predicates::str::contains("target exists"));
    assert_eq!(std::fs::read(&dst).expect("read"), b"keep me");

    bin()
        .arg("copy")
        .arg(&src)
        .arg(&dst)
        .arg("--force")
        .assert()
        .success();
    assert_eq!(std::fs::read(&dst).expect("read"), b"new");
}

/// Skipping holes has to be visible in the output, not silent: the target
/// differs from the source anywhere a raw reader looks.
#[test]
fn sparse_mode_says_what_it_skipped() {
    let dir = tempfile::tempdir().expect("dir");
    let src = dir.path().join("sparse.bin");
    let f = std::fs::File::create(&src).expect("create");
    use std::os::unix::fs::FileExt;
    f.write_all_at(b"head", 0).expect("head");
    f.set_len(8 * 1024 * 1024).expect("len");
    f.sync_all().expect("sync");
    drop(f);

    bin()
        .arg("copy")
        .arg(&src)
        .arg(dir.path().join("dst.bin"))
        .arg("--sparse")
        .assert()
        .success()
        .stdout(predicates::str::contains("holes skipped"))
        .stdout(predicates::str::contains("not zeros"));
}

#[test]
fn fix_keeps_the_name_and_the_contents() {
    let dir = tempfile::tempdir().expect("dir");
    let bytes = vec![0x5Au8; 2 * 1024 * 1024];
    let path = write(dir.path(), "image.vhd", &bytes);

    bin().arg("fix").arg(&path).assert().success();
    assert!(path.exists());
    assert_eq!(std::fs::read(&path).expect("read"), bytes);
}

#[test]
fn fix_reports_a_file_that_needed_no_work() {
    let dir = tempfile::tempdir().expect("dir");
    let path = write(dir.path(), "fine.bin", &vec![1u8; 1024 * 1024]);

    bin()
        .arg("fix")
        .arg(&path)
        .assert()
        .success()
        .stdout(predicates::str::contains("already contiguous"));
}

#[test]
fn copying_a_missing_source_fails_without_creating_the_target() {
    let dir = tempfile::tempdir().expect("dir");
    let dst = dir.path().join("dst.bin");

    bin()
        .arg("copy")
        .arg(dir.path().join("nope.bin"))
        .arg(&dst)
        .assert()
        .failure();
    assert!(!dst.exists(), "no partial target left behind");
}

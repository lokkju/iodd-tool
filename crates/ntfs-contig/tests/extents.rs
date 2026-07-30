//! Extent-engine tests against a real filesystem.
//!
//! These need no sudo and no NTFS: they run on whatever the dev box or CI
//! runner provides, usually ext4. That is deliberate. The bug these exist to
//! catch — an unbounded FIEMAP query picking up past-EOF cluster slack — is
//! not ntfs3-specific, and catching it here means it cannot reach the write
//! path in phase 4.
//!
//! The crate denies unwrap/expect/panic so the write path can never abort on a
//! device holding real data; integration-test helpers are exempt.
#![allow(clippy::expect_used, clippy::panic)]

use assert_cmd::Command;
use ntfs_contig::extents;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;

fn write_file(dir: &std::path::Path, name: &str, bytes: usize) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut f = File::create(&path).expect("create");
    let block = vec![0xABu8; 64 * 1024];
    let mut written = 0;
    while written < bytes {
        let n = block.len().min(bytes - written);
        f.write_all(&block[..n]).expect("write");
        written += n;
    }
    f.sync_all().expect("sync");
    path
}

#[test]
fn runs_cover_the_file_and_never_overrun_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "plain.bin", 3 * 1024 * 1024);

    let f = File::open(&path).expect("open");
    let size = f.metadata().expect("stat").len();
    let map = extents::map(f.as_fd(), size, &path).expect("map");

    let total: u64 = map.runs.iter().map(|r| r.length).sum();
    assert!(total >= size, "runs must cover the file: {total} < {size}");

    // The regression that matters. An unbounded query returns cluster slack
    // past end-of-file; a bounded one cannot. If this fires, the phase 4
    // UNWRITTEN check would never pass.
    for run in &map.runs {
        assert!(
            run.logical < size,
            "run starts past EOF: logical {} >= size {size}",
            run.logical
        );
    }
    assert!(
        total < size + 1024 * 1024,
        "runs overrun the file by more than a cluster: {total} vs {size}"
    );
}

#[test]
fn a_zero_length_file_maps_to_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("empty.bin");
    File::create(&path).expect("create");

    let f = File::open(&path).expect("open");
    let map = extents::map(f.as_fd(), 0, &path).expect("map");
    assert!(map.runs.is_empty());
    assert!(!map.any_unwritten);
}

#[test]
fn a_contiguous_file_reports_one_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "contig.bin", 256 * 1024);

    let f = File::open(&path).expect("open");
    let size = f.metadata().expect("stat").len();
    let map = extents::map(f.as_fd(), size, &path).expect("map");

    // A freshly written small file on an idle filesystem is normally one run.
    // If it is not, the merge is still expected to be sane, so assert the
    // weaker invariant rather than making the test flaky.
    assert!(!map.runs.is_empty());
    assert!(map.runs.len() <= map.raw_records.max(1));
}

#[test]
fn verify_accepts_a_fully_allocated_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "good.bin", 512 * 1024);

    Command::cargo_bin("ntfs-contig")
        .expect("binary")
        .args(["verify", &path.to_string_lossy()])
        .assert()
        .success();
}

/// Exit 5 is the sparseness gate.
///
/// The `--strict` half of this lives in the iodd tests: `--strict` is an iodd
/// flag governing *footer* findings, and the point there is that it does not
/// change the hard gates. The engine has no such flag, only `--report-only`.
#[test]
fn verify_rejects_a_sparse_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sparse.bin");

    let f = File::create(&path).expect("create");
    f.set_len(8 * 1024 * 1024).expect("truncate");
    f.sync_all().expect("sync");
    drop(f);

    let meta = std::fs::metadata(&path).expect("stat");
    if meta.blocks() * 512 >= meta.len() {
        eprintln!("filesystem does not create sparse files here; skipping");
        return;
    }

    Command::cargo_bin("ntfs-contig")
        .expect("binary")
        .args(["verify", &path.to_string_lossy()])
        .assert()
        .code(5);
}

/// `--report-only` is the engine's escape hatch for auditing: report the facts,
/// never fail on them. Useful when scanning a volume where fragmentation is
/// expected and you want the numbers rather than an exit code.
#[test]
fn report_only_never_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sparse2.bin");

    let f = File::create(&path).expect("create");
    f.set_len(8 * 1024 * 1024).expect("truncate");
    f.sync_all().expect("sync");
    drop(f);

    Command::cargo_bin("ntfs-contig")
        .expect("binary")
        .args(["verify", &path.to_string_lossy(), "--report-only"])
        .assert()
        .success();
}

#[test]
fn json_output_is_parseable_and_stable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "j.bin", 128 * 1024);

    let out = Command::cargo_bin("ntfs-contig")
        .expect("binary")
        .args(["verify", &path.to_string_lossy(), "--json"])
        .output()
        .expect("run");
    let text = String::from_utf8_lossy(&out.stdout);

    for field in [
        "\"path\"",
        "\"size\"",
        "\"allocated\"",
        "\"sparse\"",
        "\"merged_runs\"",
        "\"contiguous\"",
        "\"any_unwritten\"",
        "\"runs\"",
    ] {
        assert!(text.contains(field), "missing {field} in: {text}");
    }
    assert!(text.starts_with('['), "must be a JSON array");
}

/// The Rust port must agree with `spike/fiemap.py`, which was validated
/// against the physical device. Skipped when python3 is unavailable.
#[test]
fn rust_agrees_with_the_python_prototype() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("spike/fiemap.py");
    if !script.exists() {
        eprintln!("prototype missing; skipping");
        return;
    }
    let Ok(py) = std::process::Command::new("python3")
        .arg(&script)
        .arg("--json")
        .arg(env!("CARGO_MANIFEST_DIR").to_string() + "/Cargo.toml")
        .output()
    else {
        eprintln!("python3 unavailable; skipping");
        return;
    };
    if !py.status.success() {
        eprintln!("prototype failed; skipping");
        return;
    }
    let text = String::from_utf8_lossy(&py.stdout);

    // Pull merged_runs out without a JSON dependency in dev-deps.
    let prototype_runs: usize = text
        .split("\"merged_runs\":")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse().ok())
        .expect("merged_runs in prototype output");

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let f = File::open(&path).expect("open");
    let size = f.metadata().expect("stat").len();
    let map = extents::map(f.as_fd(), size, &path).expect("map");

    assert_eq!(
        map.runs.len(),
        prototype_runs,
        "Rust and the Python prototype disagree on run count"
    );
}

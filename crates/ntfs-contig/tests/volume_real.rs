//! Volume-state parsing against a *real* NTFS filesystem.
//!
//! The unit tests in `volume.rs` build synthetic records, so they only ever
//! prove the parser agrees with the fixture generator sitting next to it. These
//! tests run against bytes written by `mkntfs`, and cross-check the result
//! against `ntfsinfo` — an independent implementation of the same read. That is
//! what makes a passing result mean something.
//!
//! No root required: `mkntfs` writes into a plain image file, and the parser
//! reads a file the same way it reads a block device. Skipped with a notice
//! when `ntfsprogs` is not installed.

#![allow(clippy::expect_used, clippy::panic)]

use ntfs_contig::volume::{self, VOLUME_IS_DIRTY};

/// `$VOLUME_INFORMATION`, needed here to locate the flags field for patching.
const ATTR_VOLUME_INFORMATION: u32 = 0x0000_0070;

use std::path::{Path, PathBuf};
use std::process::Command;

fn have(tool: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .output()
        .is_ok_and(|o| o.status.success())
}

fn requires(tools: &[&str]) -> bool {
    for t in tools {
        if !have(t) {
            eprintln!("skipping: {t} not installed (install ntfsprogs / ntfs-3g)");
            return false;
        }
    }
    true
}

/// A 64 MiB NTFS filesystem in a plain file. `-F` forces past the "this is not
/// a block device" refusal; nothing here needs a real device.
fn make_volume(label: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let img = dir.path().join("volume.ntfs");
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "truncate -s 64M {img} && mkntfs -F -q -L {label} {img}",
            img = img.display()
        ))
        .output()
        .expect("spawn mkntfs");
    assert!(
        out.status.success(),
        "mkntfs failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (dir, img)
}

/// Run `ntfsinfo -m`, returning whether it succeeded and what it printed.
///
/// `force` maps to `-f`. It matters: ntfsprogs refuses to open a volume whose
/// dirty flag is set unless forced, which is the same protective reflex the
/// kernel drivers have and the reason a dirty IODD volume simply will not
/// mount.
fn ntfsinfo(img: &Path, force: bool) -> (bool, String) {
    let mut cmd = Command::new("ntfsinfo");
    if force {
        cmd.arg("-f");
    }
    let out = cmd.arg("-m").arg(img).output().expect("spawn ntfsinfo");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Pull a trailing integer out of an `ntfsinfo -m` line.
fn info_field(info: &str, name: &str) -> Option<u64> {
    info.lines()
        .find(|l| l.trim().starts_with(name))?
        .rsplit(':')
        .next()?
        .trim()
        .parse()
        .ok()
}

#[test]
fn geometry_matches_what_ntfsinfo_reports() {
    if !requires(&["mkntfs", "ntfsinfo"]) {
        return;
    }
    let (_dir, img) = make_volume("GEOMTEST");

    let boot = std::fs::read(&img).expect("read image");
    let bs = volume::parse_boot_sector(&boot[..512], &img).expect("parses");

    let (ok, info) = ntfsinfo(&img, false);
    assert!(ok, "ntfsinfo should read a fresh volume");
    let field = |name: &str| info_field(&info, name);

    assert_eq!(
        Some(u64::from(bs.bytes_per_sector)),
        field("Sector Size:"),
        "sector size disagrees with ntfsinfo"
    );
    assert_eq!(
        Some(bs.cluster_size),
        field("Cluster Size:"),
        "cluster size disagrees with ntfsinfo"
    );
    assert_eq!(
        Some(bs.mft_record_size),
        field("MFT Record Size:"),
        "MFT record size disagrees with ntfsinfo"
    );
    assert_eq!(
        Some(bs.mft_lcn),
        field("LCN of Data Attribute for FILE_MFT:"),
        "MFT location disagrees with ntfsinfo"
    );
}

#[test]
fn a_freshly_made_volume_is_clean_and_labelled() {
    if !requires(&["mkntfs", "ntfsinfo"]) {
        return;
    }
    let (_dir, img) = make_volume("IODDTEST");

    let got = volume::read(&img).expect("reads");
    assert_eq!(got.label.as_deref(), Some("IODDTEST"));
    assert_eq!((got.major, got.minor), (3, 1), "NTFS 3.1");
    assert!(!got.is_dirty(), "mkntfs does not leave a volume dirty");
    assert!(got.flag_names().is_empty(), "no flags set: {:?}", got.flags);

    let (ok, info) = ntfsinfo(&img, false);
    assert!(ok, "ntfsinfo opens a clean volume without --force");
    assert!(
        info.contains("Volume Flags: 0x0000"),
        "ntfsinfo should agree the volume is clean"
    );
}

/// Set the dirty bit in the image, then confirm *both* implementations see it.
///
/// Patching the byte ourselves and re-reading it would only prove the parser is
/// self-consistent. Having `ntfsinfo` independently report `0x0001` is what
/// shows the bit was written where NTFS actually keeps it.
#[test]
fn the_dirty_flag_is_read_the_same_way_ntfsinfo_reads_it() {
    if !requires(&["mkntfs", "ntfsinfo"]) {
        return;
    }
    let (_dir, img) = make_volume("DIRTYTEST");

    assert!(!volume::read(&img).expect("reads").is_dirty());

    let mut bytes = std::fs::read(&img).expect("read image");
    let bs = volume::parse_boot_sector(&bytes[..512], &img).expect("boot sector");

    // $Volume must be patched in *both* copies. NTFS mirrors the first four
    // MFT records into $MFTMirr, and record 3 is $Volume, so touching only
    // $MFT leaves the two disagreeing — ntfsprogs then rejects the volume as
    // inconsistent ("$MFTMirr does not match $MFT (record 3)") rather than
    // reporting it dirty, and the test would be measuring the wrong thing.
    //
    // Reading is unaffected: $MFT is authoritative, so the parser under test
    // correctly consults it alone. Only a writer has to maintain the mirror.
    let (_, info) = ntfsinfo(&img, false);
    let mirror_lcn = info_field(&info, "LCN of Data Attribute for File_MFTMirr:")
        .expect("ntfsinfo reports the $MFTMirr location");

    let rec_len = usize::try_from(bs.mft_record_size).expect("fits");
    for base_lcn in [bs.mft_lcn, mirror_lcn] {
        let start = usize::try_from(base_lcn * bs.cluster_size + 3 * bs.mft_record_size)
            .expect("offset fits");
        let record = &mut bytes[start..start + rec_len];
        // Find the $VOLUME_INFORMATION header, then reach its flags: 0x18 to
        // the resident value, then 10 bytes into it.
        let attr = record
            .windows(4)
            .position(|w| w == ATTR_VOLUME_INFORMATION.to_le_bytes())
            .expect("$VOLUME_INFORMATION present");
        let flags_at = attr + 0x18 + 10;
        assert_eq!(
            u16::from_le_bytes([record[flags_at], record[flags_at + 1]]),
            0,
            "flags start clear at LCN {base_lcn}"
        );
        record[flags_at..flags_at + 2].copy_from_slice(&VOLUME_IS_DIRTY.to_le_bytes());
    }
    std::fs::write(&img, &bytes).expect("write image");

    let got = volume::read(&img).expect("reads");
    assert!(got.is_dirty(), "our parser should see the dirty bit");
    assert_eq!(got.flag_names(), vec!["dirty"]);
    assert_eq!(
        got.label.as_deref(),
        Some("DIRTYTEST"),
        "the rest of the record still parses"
    );

    // ntfsprogs now refuses the volume outright ("Volume is scheduled for
    // check") — the same reflex that makes a dirty IODD volume unmountable,
    // and the reason this check has to read the device rather than ask a
    // mounted filesystem: by the time it matters, there is nothing to ask.
    let (ok, _) = ntfsinfo(&img, false);
    assert!(!ok, "ntfsinfo should refuse a dirty volume without --force");

    let (ok, info) = ntfsinfo(&img, true);
    assert!(ok, "ntfsinfo --force should still read it");
    assert!(
        info.contains("Volume Flags: 0x0001"),
        "ntfsinfo should independently report the volume dirty:\n{info}"
    );
}

#[test]
fn a_non_ntfs_image_is_rejected_cleanly() {
    let dir = tempfile::tempdir().expect("temp dir");
    let img = dir.path().join("not-ntfs.img");
    std::fs::write(&img, vec![0u8; 1 << 20]).expect("write");

    let err = volume::read(&img).expect_err("not NTFS");
    assert!(
        err.to_string().contains("no NTFS signature"),
        "unexpected error: {err}"
    );
}

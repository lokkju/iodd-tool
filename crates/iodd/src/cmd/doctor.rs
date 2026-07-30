//! `iodd doctor` — "will everything on this device mount?"
//!
//! Strictly read-only. Files are opened `O_RDONLY | O_NOFOLLOW`; only the first
//! 512 bytes, the last 512, `fstat`, and the extent map are read. Nothing is
//! written, renamed, or deleted, and remediation is reported rather than
//! performed.
//!
//! # The verdict function is pure
//!
//! [`grade`] takes [`FileFacts`] and a [`Policy`] and returns an
//! [`Assessment`]. No I/O. That is what lets nearly every case in the design's
//! fixture list — a dynamic VHD, a VHDX under a `.vhd` name, a real VMDK, a
//! footerless `.rmd`, an ISO over the fragment ceiling — be a struct literal in
//! a unit test rather than a committed binary needing a filesystem.
//!
//! # What counts as fatal
//!
//! Only what the device enforces: one extent, and fully allocated. Footer
//! problems are warnings (design D2), because measurement showed the device
//! mounts files with no valid footer at all, and because a guest partitioning
//! the disk overwrites the footer with its GPT backup header. Grading that as
//! broken would condemn a file that demonstrably works.

use crate::cli::DoctorArgs;
use crate::error::{Error, Result};
use crate::footer;
use crate::report::{self, Record};
use ntfs_contig::extents;
use std::path::{Path, PathBuf};

/// Default fragment ceiling for ISO images. Device families differ; the value
/// is overridable because it was never documented, only inferred.
pub const DEFAULT_ISO_MAX_FRAGMENTS: u32 = 24;

/// Extensions IODD will attempt to mount.
///
/// `.ima` only, deliberately. `SPEC.md` line 146 mentions `.img`, `.bif` and
/// `.vfd`, but as formats that get *renamed to* `.ima`, not as extensions the
/// device scans (design revision 9).
pub const RECOGNIZED: [&str; 5] = ["vhd", "rmd", "vmdk", "iso", "ima"];

#[derive(Debug, Clone)]
pub struct Policy {
    pub iso_max_fragments: u32,
    pub strict: bool,
    /// Empty means every recognized type.
    pub types: Vec<String>,
    pub recursive: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            iso_max_fragments: DEFAULT_ISO_MAX_FRAGMENTS,
            strict: false,
            types: Vec::new(),
            recursive: true,
        }
    }
}

impl Policy {
    fn accepts(&self, ext: &str) -> bool {
        if !RECOGNIZED.contains(&ext) {
            return false;
        }
        self.types.is_empty() || self.types.iter().any(|t| t.eq_ignore_ascii_case(ext))
    }
}

/// Everything [`grade`] needs. Gathered once per file, read-only.
#[derive(Debug, Clone)]
pub struct FileFacts {
    pub path: PathBuf,
    /// Lowercased, without the dot.
    pub ext: String,
    pub size: u64,
    /// `st_blocks * 512`.
    pub allocated: u64,
    pub run_count: usize,
    pub any_unwritten: bool,
    /// First 512 bytes, for container magics. Short files are zero-padded.
    pub head: [u8; 512],
    /// Last 512 bytes, where a fixed VHD keeps its footer.
    pub tail: [u8; 512],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Mountable,
    Warn,
    WillNotMount,
    /// Structurally broken enough that the device may hide it from its
    /// on-screen list entirely, so the operator sees a file they copied simply
    /// missing rather than reported as bad (`SPEC.md` line 148).
    WillNotMountMaybeUnlisted,
}

impl Verdict {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Mountable => "MOUNTABLE",
            Self::Warn => "WARN",
            Self::WillNotMount => "WILL-NOT-MOUNT",
            Self::WillNotMountMaybeUnlisted => "WILL-NOT-MOUNT (may not be listed)",
        }
    }

    #[must_use]
    pub fn is_failure(self) -> bool {
        matches!(self, Self::WillNotMount | Self::WillNotMountMaybeUnlisted)
    }
}

/// A graded file.
#[derive(Debug, Clone)]
pub struct Assessment {
    pub verdict: Verdict,
    pub reasons: Vec<String>,
}

/// Grade a file. Pure: no I/O, no clock, no filesystem.
#[must_use]
pub fn grade(f: &FileFacts, policy: &Policy) -> Assessment {
    match f.ext.as_str() {
        "iso" => grade_iso(f, policy),
        "ima" => grade_ima(f),
        // vhd, rmd, vmdk: all expected to be fixed-VHD bytes.
        _ => grade_vhd_family(f),
    }
}

/// `.iso` — the one type with a fragment tolerance.
///
/// No footer or allocation check applies (`SPEC.md` line 145). A 3-extent ISO
/// was measured working on a real device, so the tolerance is real rather than
/// theoretical.
fn grade_iso(f: &FileFacts, policy: &Policy) -> Assessment {
    let runs = u32::try_from(f.run_count).unwrap_or(u32::MAX);
    if runs > policy.iso_max_fragments {
        return Assessment {
            verdict: Verdict::WillNotMount,
            reasons: vec![format!(
                "{runs} fragments exceeds the ceiling of {} (override with --iso-max-fragments)",
                policy.iso_max_fragments
            )],
        };
    }
    Assessment {
        verdict: Verdict::Mountable,
        reasons: Vec::new(),
    }
}

/// `.ima` — floppy images. The device's tolerance here is undocumented, so
/// fragmentation warns rather than failing, and the reason says why.
fn grade_ima(f: &FileFacts) -> Assessment {
    if f.run_count > 1 {
        return Assessment {
            verdict: Verdict::Warn,
            reasons: vec![format!(
                "{} fragments; the device's tolerance for floppy images is \
                 undocumented, so this is unverified rather than known-bad",
                f.run_count
            )],
        };
    }
    Assessment {
        verdict: Verdict::Mountable,
        reasons: Vec::new(),
    }
}

fn grade_vhd_family(f: &FileFacts) -> Assessment {
    let mut reasons = Vec::new();

    // ---- container misdetection: reported; severity decided below ----------
    let container = detect_container(f);
    if let Some(ref why) = container {
        reasons.push(why.clone());
    }

    // ---- the two hard gates ------------------------------------------------
    let mut fatal = false;

    if f.size > 0 && f.allocated < f.size {
        reasons.push(format!(
            "sparse: {} bytes allocated for a {} byte file; the device streams \
             sectors off the extent map and cannot read a hole",
            f.allocated, f.size
        ));
        fatal = true;
    }

    if f.run_count > 1 {
        reasons.push(format!(
            "{} fragments; the device requires exactly 1",
            f.run_count
        ));
        fatal = true;
    }

    if fatal {
        // A file that is both the wrong container *and* fails a hard gate is
        // the case most likely to vanish from the device's menu rather than be
        // reported as bad.
        let verdict = if container.is_some() {
            Verdict::WillNotMountMaybeUnlisted
        } else {
            Verdict::WillNotMount
        };
        return Assessment { verdict, reasons };
    }

    // ---- soft findings -----------------------------------------------------
    let (_, problems) = footer::parse(&f.tail, Some(f.size.saturating_sub(512)));
    for p in &problems {
        reasons.push(p.to_string());
    }

    if f.any_unwritten {
        reasons.push(
            "contains uninitialized extents; a guest will see whatever was \
             previously on those clusters, not zeros"
                .to_string(),
        );
    }

    let verdict = if reasons.is_empty() {
        Verdict::Mountable
    } else {
        Verdict::Warn
    };
    Assessment { verdict, reasons }
}

/// Spot files carrying a fixed-VHD extension that are not fixed-VHD bytes.
fn detect_container(f: &FileFacts) -> Option<String> {
    if f.head.starts_with(b"vhdxfile") {
        return Some("VHDX container; the device mounts fixed VHD only".into());
    }
    // A dynamic or differencing VHD keeps a footer *copy* at offset 0.
    if f.head.starts_with(footer::COOKIE) {
        let disk_type = u32::from_be_bytes([f.head[60], f.head[61], f.head[62], f.head[63]]);
        let kind = match disk_type {
            footer::DISK_TYPE_DYNAMIC => Some("dynamic"),
            footer::DISK_TYPE_DIFFERENCING => Some("differencing"),
            _ => None,
        };
        if let Some(kind) = kind {
            return Some(format!(
                "{kind} VHD (footer copy at offset 0); the device mounts fixed VHD only"
            ));
        }
    }
    if f.ext == "vmdk" {
        if f.head.starts_with(b"KDMV") {
            return Some(
                "genuine VMDK sparse extent; IODD expects fixed-VHD bytes under a .vmdk name"
                    .into(),
            );
        }
        if f.head.starts_with(b"# Disk DescriptorFile") {
            return Some(
                "genuine VMDK descriptor; IODD expects fixed-VHD bytes under a .vmdk name".into(),
            );
        }
    }
    None
}

// ---------------------------------------------------------------------------
// I/O layer
// ---------------------------------------------------------------------------

pub fn run(args: &DoctorArgs) -> Result<u8> {
    let policy = Policy {
        iso_max_fragments: args.iso_max_fragments,
        strict: args.strict,
        types: args
            .r#type
            .iter()
            .map(|t| t.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .collect(),
        recursive: args.is_recursive(),
    };

    for t in &policy.types {
        if !RECOGNIZED.contains(&t.as_str()) {
            return Err(Error::Usage(format!(
                "unknown type {t:?}; recognized types are {}",
                RECOGNIZED.join(", ")
            )));
        }
    }

    let root = args.root.as_path();
    if !root.is_dir() {
        return Err(Error::Usage(format!(
            "{} is not a directory",
            root.display()
        )));
    }

    let mut records: Vec<Record> = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .max_depth(if policy.recursive { usize::MAX } else { 1 })
        .follow_links(false);

    for entry in walker.into_iter().filter_map(std::result::Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(ext) = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
        else {
            continue;
        };
        if !policy.accepts(&ext) {
            continue;
        }

        match gather(path, ext) {
            Ok(facts) => {
                let assessment = grade(&facts, &policy);
                records.push(Record::new(&facts, &assessment));
            }
            Err(e) => records.push(Record::unreadable(path, &e.to_string())),
        }
    }

    records.sort_by(|a, b| a.path.cmp(&b.path));

    match args.format {
        crate::cli::Format::Json => report::render_json(&records),
        crate::cli::Format::Table => report::render_table(&records),
    }

    Ok(exit_code(&records, policy.strict))
}

/// Aggregate exit: 0 when everything mounts, 4 when anything will not, and
/// under `--strict` 4 when anything warns.
///
/// Reuses the contiguity code rather than inventing one, per design revision
/// 11.
fn exit_code(records: &[Record], strict: bool) -> u8 {
    if records.iter().any(|r| r.verdict.is_none()) {
        // Unreadable files are a usage-level problem, not a device verdict.
        return 1;
    }
    match records.iter().filter_map(|r| r.verdict).max() {
        Some(v) if v.is_failure() => 4,
        Some(Verdict::Warn) if strict => 4,
        _ => 0,
    }
}

/// Read exactly what a verdict needs, and nothing else.
fn gather(path: &Path, ext: String) -> ntfs_contig::Result<FileFacts> {
    use ntfs_contig::Error as E;
    use std::os::fd::AsFd;
    use std::os::unix::fs::{FileExt, MetadataExt};

    let file = ntfs_contig::fsinfo::open_readonly_nofollow(path)?;

    let meta = file.metadata().map_err(|source| E::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let size = meta.len();

    let mut head = [0u8; 512];
    let mut tail = [0u8; 512];
    // Short reads are fine: a file too small to hold a magic or a footer simply
    // leaves the buffer zeroed, which no detector matches.
    let _ = file.read_at(&mut head, 0);
    if size >= 512 {
        let _ = file.read_at(&mut tail, size - 512);
    }

    let map = extents::map(file.as_fd(), size, path)?;

    Ok(FileFacts {
        path: path.to_path_buf(),
        ext,
        size,
        allocated: meta.blocks().saturating_mul(512),
        run_count: map.runs.len(),
        any_unwritten: map.any_unwritten,
        head,
        tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footer::{Creator, Footer};

    fn facts(ext: &str) -> FileFacts {
        let footer = Footer::new(1_048_576, Creator::DEFAULT, 0x3000_0000, [7u8; 16]);
        FileFacts {
            path: PathBuf::from(format!("/x/f.{ext}")),
            ext: ext.to_string(),
            size: 1_048_576 + 512,
            allocated: 1_048_576 + 512,
            run_count: 1,
            any_unwritten: false,
            head: [0u8; 512],
            tail: footer.to_bytes(),
        }
    }

    fn grade_default(f: &FileFacts) -> Assessment {
        grade(f, &Policy::default())
    }

    // ---- the happy case ---------------------------------------------------

    #[test]
    fn a_clean_fixed_vhd_is_mountable() {
        let a = grade_default(&facts("vhd"));
        assert_eq!(a.verdict, Verdict::Mountable, "{:?}", a.reasons);
        assert!(a.reasons.is_empty());
    }

    #[test]
    fn rmd_and_vmdk_use_the_same_ruleset_as_vhd() {
        for ext in ["rmd", "vmdk"] {
            assert_eq!(grade_default(&facts(ext)).verdict, Verdict::Mountable);
        }
    }

    // ---- the two hard gates -----------------------------------------------

    #[test]
    fn a_fragmented_vhd_will_not_mount() {
        let mut f = facts("vhd");
        f.run_count = 3;
        let a = grade_default(&f);
        assert_eq!(a.verdict, Verdict::WillNotMount);
        assert!(a.reasons.iter().any(|r| r.contains("3 fragments")));
    }

    #[test]
    fn a_sparse_vhd_will_not_mount() {
        let mut f = facts("vhd");
        f.allocated = 4096;
        let a = grade_default(&f);
        assert_eq!(a.verdict, Verdict::WillNotMount);
        assert!(a.reasons.iter().any(|r| r.contains("sparse")));
    }

    // ---- D2: footer problems are warnings ---------------------------------

    /// The design's named fixture, and the shape of the one file measured
    /// mounting on real hardware: no footer at all, yet it works.
    #[test]
    fn a_footerless_raw_file_under_rmd_is_mountable_with_a_warning() {
        let mut f = facts("rmd");
        f.tail = [0xAAu8; 512];
        let a = grade_default(&f);
        assert_eq!(a.verdict, Verdict::Warn, "{:?}", a.reasons);
        assert!(!a.verdict.is_failure(), "a working file must not be failed");
    }

    #[test]
    fn a_footer_clobbered_by_gpt_is_a_warning_with_an_explanation() {
        let mut f = facts("rmd");
        f.tail[0..8].copy_from_slice(b"EFI PART");
        let a = grade_default(&f);
        assert_eq!(a.verdict, Verdict::Warn);
        assert!(
            a.reasons.iter().any(|r| r.contains("GPT backup header")),
            "the operator should be told why: {:?}",
            a.reasons
        );
    }

    #[test]
    fn uninitialized_extents_warn_rather_than_fail() {
        let mut f = facts("vhd");
        f.any_unwritten = true;
        let a = grade_default(&f);
        assert_eq!(a.verdict, Verdict::Warn);
        assert!(a.reasons.iter().any(|r| r.contains("uninitialized")));
    }

    // ---- container misdetection -------------------------------------------

    #[test]
    fn a_vhdx_under_a_vhd_name_is_reported() {
        let mut f = facts("vhd");
        f.head[0..8].copy_from_slice(b"vhdxfile");
        let a = grade_default(&f);
        assert!(a.reasons.iter().any(|r| r.contains("VHDX")));
    }

    #[test]
    fn a_dynamic_vhd_is_reported() {
        let mut f = facts("vhd");
        f.head[0..8].copy_from_slice(footer::COOKIE);
        f.head[60..64].copy_from_slice(&footer::DISK_TYPE_DYNAMIC.to_be_bytes());
        let a = grade_default(&f);
        assert!(a.reasons.iter().any(|r| r.contains("dynamic")));
    }

    #[test]
    fn a_differencing_vhd_is_reported() {
        let mut f = facts("vhd");
        f.head[0..8].copy_from_slice(footer::COOKIE);
        f.head[60..64].copy_from_slice(&footer::DISK_TYPE_DIFFERENCING.to_be_bytes());
        let a = grade_default(&f);
        assert!(a.reasons.iter().any(|r| r.contains("differencing")));
    }

    #[test]
    fn a_real_vmdk_under_a_vmdk_name_is_reported() {
        for magic in [&b"KDMV"[..], &b"# Disk DescriptorFile"[..]] {
            let mut f = facts("vmdk");
            f.head[..magic.len()].copy_from_slice(magic);
            let a = grade_default(&f);
            assert!(
                a.reasons.iter().any(|r| r.contains("VMDK")),
                "{:?}",
                a.reasons
            );
        }
    }

    /// A fixed VHD renamed to .vmdk is exactly what IODD expects, so the
    /// detector must not fire on it.
    #[test]
    fn fixed_vhd_bytes_under_a_vmdk_name_are_fine() {
        assert_eq!(grade_default(&facts("vmdk")).verdict, Verdict::Mountable);
    }

    /// Only when a hard gate also fails does a container problem become the
    /// "may not be listed" case.
    #[test]
    fn a_broken_container_that_also_fails_a_gate_may_be_unlisted() {
        let mut f = facts("vhd");
        f.head[0..8].copy_from_slice(b"vhdxfile");
        f.run_count = 5;
        assert_eq!(
            grade_default(&f).verdict,
            Verdict::WillNotMountMaybeUnlisted
        );
    }

    // ---- ISO ---------------------------------------------------------------

    #[test]
    fn an_iso_under_the_ceiling_is_mountable() {
        let mut f = facts("iso");
        // Measured on real hardware: a 3-extent ISO works.
        f.run_count = 3;
        assert_eq!(grade_default(&f).verdict, Verdict::Mountable);
    }

    #[test]
    fn an_iso_over_the_ceiling_will_not_mount() {
        let mut f = facts("iso");
        f.run_count = 25;
        assert_eq!(grade_default(&f).verdict, Verdict::WillNotMount);
    }

    #[test]
    fn the_iso_ceiling_is_overridable() {
        let mut f = facts("iso");
        f.run_count = 25;
        let policy = Policy {
            iso_max_fragments: 32,
            ..Policy::default()
        };
        assert_eq!(grade(&f, &policy).verdict, Verdict::Mountable);
    }

    /// No footer or allocation check applies to ISOs, so a sparse one with a
    /// junk tail is still graded on fragments alone.
    #[test]
    fn an_iso_is_not_graded_on_footer_or_allocation() {
        let mut f = facts("iso");
        f.tail = [0xFFu8; 512];
        f.allocated = 0;
        assert_eq!(grade_default(&f).verdict, Verdict::Mountable);
    }

    // ---- IMA ---------------------------------------------------------------

    #[test]
    fn a_contiguous_ima_is_mountable() {
        assert_eq!(grade_default(&facts("ima")).verdict, Verdict::Mountable);
    }

    #[test]
    fn a_fragmented_ima_warns_and_says_the_threshold_is_unknown() {
        let mut f = facts("ima");
        f.run_count = 4;
        let a = grade_default(&f);
        assert_eq!(a.verdict, Verdict::Warn);
        assert!(a.reasons.iter().any(|r| r.contains("undocumented")));
    }

    // ---- policy ------------------------------------------------------------

    #[test]
    fn type_filtering_restricts_the_scan_set() {
        let policy = Policy {
            types: vec!["vhd".into(), "rmd".into()],
            ..Policy::default()
        };
        assert!(policy.accepts("vhd"));
        assert!(policy.accepts("rmd"));
        assert!(!policy.accepts("iso"));
        assert!(!policy.accepts("exe"));
    }

    #[test]
    fn an_empty_type_list_accepts_every_recognized_type() {
        let policy = Policy::default();
        for ext in RECOGNIZED {
            assert!(policy.accepts(ext));
        }
        assert!(!policy.accepts("exe"));
        assert!(!policy.accepts("img"), "revision 9: .img is not scanned");
    }

    // ---- verdict ordering --------------------------------------------------

    /// The aggregate exit code takes the worst verdict, so the ordering has to
    /// be right.
    #[test]
    fn verdicts_order_by_severity() {
        assert!(Verdict::Mountable < Verdict::Warn);
        assert!(Verdict::Warn < Verdict::WillNotMount);
        assert!(Verdict::WillNotMount < Verdict::WillNotMountMaybeUnlisted);
    }
}

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
use ntfs_contig::{extents, volume};
use std::path::{Path, PathBuf};

/// Default fragment ceiling for ISO images. Device families differ; the value
/// is overridable because it was never documented, only inferred.
pub const DEFAULT_ISO_MAX_FRAGMENTS: u32 = 24;

/// Most items the device will show in a single directory.
///
/// Vendor documentation: "the number of files or subfolders within any single
/// directory (that contains ISO/VHD/IMA files) must not exceed 32 items", and
/// "you can bypass the 32-item limit by organizing your files into subfolders".
///
/// Exceeding it does not corrupt anything — the device simply stops listing,
/// so a file that copied perfectly is missing from the menu. That is exactly
/// the confusion `doctor` exists to explain.
pub const MAX_ITEMS_PER_DIRECTORY: usize = 32;

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

/// A directory holding mountable files, and whether the device can list all of
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryFinding {
    pub path: PathBuf,
    pub items: usize,
}

/// Check a directory's item count. Pure, like [`grade`].
#[must_use]
pub fn check_directory(path: &Path, items: usize) -> Option<DirectoryFinding> {
    (items > MAX_ITEMS_PER_DIRECTORY).then(|| DirectoryFinding {
        path: path.to_path_buf(),
        items,
    })
}

/// Volume-level state, reported once for the whole scan.
///
/// Every file on the device can be flawless and still be unreachable, because
/// the volume itself will not mount. That outranks any per-file verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeFinding {
    pub device: PathBuf,
    pub label: Option<String>,
    pub flags: Vec<String>,
    pub dirty: bool,
}

/// Grade volume state. Pure, like [`grade`] and [`check_directory`].
///
/// `None` when nothing is set, which is the normal case.
#[must_use]
pub fn check_volume(device: &Path, info: &volume::VolumeInfo) -> Option<VolumeFinding> {
    let flags = info.flag_names();
    (!flags.is_empty()).then(|| VolumeFinding {
        device: device.to_path_buf(),
        label: info.label.clone(),
        flags: flags.into_iter().map(str::to_owned).collect(),
        dirty: info.is_dirty(),
    })
}

/// Best-effort volume state for the tree being scanned.
///
/// `None` for every failure: no backing block device, no read access (the
/// usual case, since this needs root or the `disk` group), or not NTFS. An
/// audit that cannot open `/dev/sdb1` is still a useful audit, so this must
/// never turn a successful scan into an error.
///
/// Reads the device rather than asking the mounted filesystem because there
/// is nothing to ask: ntfs3 publishes no volume flags, and by the time the
/// flag matters the volume will not mount at all.
fn gather_volume(root: &Path) -> Option<VolumeFinding> {
    let device = volume::resolve(root).ok()?;
    check_volume(&device, &volume::read(&device).ok()?)
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
        // Raw images: the whole file is the guest's disk, and no footer belongs.
        "ima" | "rmd" => grade_raw(f),
        // vhd and vmdk: fixed-VHD bytes, last sector is a footer.
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

/// `.rmd` and `.ima` — raw removable images.
///
/// Measured on hardware: the device presents these **whole**, unlike a `.vhd`
/// which has its final sector treated as a footer and excluded. So a footer
/// does not belong here, and its absence is correct rather than a finding.
///
/// A footer that *is* present is the anomaly worth reporting: the guest will
/// see those 512 bytes as trailing disk, and the file is most likely a `.vhd`
/// that was renamed.
fn grade_raw(f: &FileFacts) -> Assessment {
    let mut reasons = Vec::new();
    let mut fatal = false;

    if f.size > 0 && f.allocated < f.size {
        reasons.push(format!(
            "sparse: {} bytes allocated for a {} byte file; the device streams \
             sectors off the extent map and cannot read a hole",
            f.allocated, f.size
        ));
        fatal = true;
    }

    if f.ext == "rmd" {
        // .rmd is presented whole and the device requires one extent.
        if f.run_count > 1 {
            reasons.push(format!(
                "{} fragments; the device requires exactly 1",
                f.run_count
            ));
            fatal = true;
        }
    } else if f.run_count > 1 {
        // .ima: the tolerance is undocumented, so this is unverified rather
        // than known-bad.
        reasons.push(format!(
            "{} fragments; the device's tolerance for floppy images is \
             undocumented, so this is unverified rather than known-bad",
            f.run_count
        ));
    }

    if fatal {
        return Assessment {
            verdict: Verdict::WillNotMount,
            reasons,
        };
    }

    if f.tail.starts_with(footer::COOKIE) {
        reasons.push(format!(
            "the last 512 bytes are a VHD footer, which a .{} does not use. The \
             device presents this file whole, so the guest sees the footer as \
             trailing disk. It was probably a .vhd that was renamed; \
             `iodd retype --to removable` removes the footer properly.",
            f.ext
        ));
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

    // A block device is the one case where there is no tree to walk: the
    // volume will not mount, which is exactly why someone is asking. Report
    // what can be read about the volume itself and stop.
    if is_block_device(root) {
        return volume_only(root, args.format);
    }

    if !root.is_dir() {
        return Err(Error::Usage(format!(
            "{} is not a directory",
            root.display()
        )));
    }

    let mut records: Vec<Record> = Vec::new();
    // Directories holding at least one mountable file, and how many entries
    // each holds in total. The device's limit counts subfolders too.
    let mut dirs_with_images: std::collections::BTreeSet<PathBuf> =
        std::collections::BTreeSet::new();

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

        if let Some(parent) = path.parent() {
            dirs_with_images.insert(parent.to_path_buf());
        }

        match gather(path, ext) {
            Ok(facts) => {
                let assessment = grade(&facts, &policy);
                records.push(Record::new(&facts, &assessment));
            }
            Err(e) => records.push(Record::unreadable(path, &e.to_string())),
        }
    }

    let dir_findings: Vec<DirectoryFinding> = dirs_with_images
        .iter()
        .filter_map(|d| {
            let items = std::fs::read_dir(d).ok()?.count();
            check_directory(d, items)
        })
        .collect();

    records.sort_by(|a, b| a.path.cmp(&b.path));

    let vol = gather_volume(root);

    match args.format {
        crate::cli::Format::Json => report::render_json(&records, &dir_findings, vol.as_ref()),
        crate::cli::Format::Table => report::render_table(&records, &dir_findings, vol.as_ref()),
    }

    Ok(exit_code(
        &records,
        &dir_findings,
        vol.as_ref(),
        policy.strict,
    ))
}

#[must_use]
fn is_block_device(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path).is_ok_and(|m| m.file_type().is_block_device())
}

/// `iodd doctor /dev/sdb1` — volume state only, for a volume with no
/// mountpoint to point at.
///
/// Errors propagate here rather than being swallowed as they are during a
/// tree scan: the caller named this device explicitly, so "could not read it"
/// is the answer to their question, not an incidental gap in an audit.
fn volume_only(device: &Path, format: crate::cli::Format) -> Result<u8> {
    let info = volume::read(device)?;
    let finding = check_volume(device, &info);

    match format {
        crate::cli::Format::Json => report::render_json(&[], &[], finding.as_ref()),
        crate::cli::Format::Table => report::render_volume_only(device, &info, finding.as_ref()),
    }

    Ok(u8::from(info.is_dirty()) * 4)
}

/// Aggregate exit: 0 when everything mounts, 4 when anything will not, and
/// under `--strict` 4 when anything warns.
///
/// Reuses the contiguity code rather than inventing one, per design revision
/// 11.
fn exit_code(
    records: &[Record],
    dirs: &[DirectoryFinding],
    vol: Option<&VolumeFinding>,
    strict: bool,
) -> u8 {
    // A dirty volume outranks everything below it. It is not a warning about
    // what might go wrong: the volume will not mount next time it is
    // attached, which makes every file on it unreachable however sound.
    if vol.is_some_and(|v| v.dirty) {
        return 4;
    }
    if records.iter().any(|r| r.verdict.is_none()) {
        // Unreadable files are a usage-level problem, not a device verdict.
        return 1;
    }
    // An over-full directory hides files rather than breaking them, so it is a
    // warning: everything present still mounts, but not everything is reachable.
    if strict && !dirs.is_empty() {
        return 4;
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

    /// A raw image: no footer, because raw types do not use one.
    fn raw_facts(ext: &str) -> FileFacts {
        let mut f = facts(ext);
        f.tail = [0u8; 512];
        f
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
    fn vmdk_uses_the_same_ruleset_as_vhd() {
        // IODD expects fixed-VHD bytes under a .vmdk name.
        assert_eq!(grade_default(&facts("vmdk")).verdict, Verdict::Mountable);
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

    /// A footerless `.rmd` is **correct**, not a finding.
    ///
    /// Hardware showed the device presents a `.rmd` whole, so a footer would be
    /// visible to the guest as trailing disk. Its absence is the right state.
    /// An earlier version of this graded it WARN, from the mistaken belief that
    /// every type wanted a footer.
    #[test]
    fn a_footerless_rmd_is_simply_mountable() {
        let a = grade_default(&raw_facts("rmd"));
        assert_eq!(a.verdict, Verdict::Mountable, "{:?}", a.reasons);
        assert!(
            a.reasons.is_empty(),
            "no findings expected: {:?}",
            a.reasons
        );
    }

    /// The file that began the investigation: a 40 GiB `.rmd` whose last sector
    /// is a GPT backup header. Under the corrected model that is exactly what a
    /// partitioned raw removable image looks like, so it grades clean.
    #[test]
    fn the_device_rmd_grades_clean_under_the_corrected_model() {
        let mut f = raw_facts("rmd");
        f.size = 42_949_673_472;
        f.allocated = 42_949_677_056;
        f.tail[0..8].copy_from_slice(b"EFI PART");
        let a = grade_default(&f);
        assert_eq!(a.verdict, Verdict::Mountable, "{:?}", a.reasons);
    }

    /// A `.rmd` that still carries a VHD footer is the anomaly: the guest will
    /// see it as trailing disk, and the file was probably a renamed `.vhd`.
    #[test]
    fn an_rmd_with_a_footer_is_reported() {
        let a = grade_default(&facts("rmd"));
        assert_eq!(a.verdict, Verdict::Warn, "{:?}", a.reasons);
        assert!(
            a.reasons.iter().any(|r| r.contains("does not use")),
            "{:?}",
            a.reasons
        );
        assert!(
            a.reasons.iter().any(|r| r.contains("retype")),
            "should point at the fix: {:?}",
            a.reasons
        );
    }

    /// The GPT explanation still applies to a `.vhd`, where the last sector
    /// really is supposed to be a footer.
    #[test]
    fn a_vhd_footer_clobbered_by_gpt_is_a_warning_with_an_explanation() {
        let mut f = facts("vhd");
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
    fn a_fragmented_rmd_will_not_mount() {
        let mut f = raw_facts("rmd");
        f.run_count = 3;
        assert_eq!(grade_default(&f).verdict, Verdict::WillNotMount);
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
        assert_eq!(grade_default(&raw_facts("ima")).verdict, Verdict::Mountable);
    }

    #[test]
    fn a_fragmented_ima_warns_and_says_the_threshold_is_unknown() {
        let mut f = raw_facts("ima");
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

    // ---- directory item limit ----------------------------------------------

    #[test]
    fn a_directory_at_the_limit_is_fine() {
        assert_eq!(check_directory(Path::new("/x/_ISO"), 32), None);
        assert_eq!(check_directory(Path::new("/x/_ISO"), 1), None);
    }

    #[test]
    fn a_directory_over_the_limit_is_reported() {
        let f = check_directory(Path::new("/x/_ISO"), 33).expect("should fire");
        assert_eq!(f.items, 33);
        assert_eq!(f.path, Path::new("/x/_ISO"));
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

    // ---- volume state ------------------------------------------------------

    fn vol_info(flags: u16) -> volume::VolumeInfo {
        volume::VolumeInfo {
            label: Some("IODD".to_owned()),
            major: 3,
            minor: 1,
            flags,
        }
    }

    fn dev() -> &'static Path {
        Path::new("/dev/sdb1")
    }

    #[test]
    fn a_clean_volume_produces_no_finding() {
        assert_eq!(check_volume(dev(), &vol_info(0)), None);
    }

    #[test]
    fn a_dirty_volume_is_reported() {
        let f = check_volume(dev(), &vol_info(volume::VOLUME_IS_DIRTY)).expect("should fire");
        assert!(f.dirty);
        assert_eq!(f.flags, vec!["dirty"]);
        assert_eq!(f.device, dev());
        assert_eq!(f.label.as_deref(), Some("IODD"));
    }

    /// Flags other than dirty are worth surfacing but do not stop a mount, so
    /// they must not be reported as dirty.
    #[test]
    fn a_non_dirty_flag_is_reported_without_claiming_dirty() {
        let f =
            check_volume(dev(), &vol_info(volume::VOLUME_MODIFIED_BY_CHKDSK)).expect("should fire");
        assert!(!f.dirty);
        assert_eq!(f.flags, vec!["modified-by-chkdsk"]);
    }

    // ---- exit codes --------------------------------------------------------

    fn clean_record() -> Record {
        let f = facts("vhd");
        Record::new(&f, &grade_default(&f))
    }

    #[test]
    fn a_clean_scan_exits_zero() {
        assert_eq!(exit_code(&[clean_record()], &[], None, false), 0);
    }

    /// The point of the whole volume check: every file can be perfect and the
    /// device still unusable, because the volume will not mount.
    #[test]
    fn a_dirty_volume_fails_the_scan_even_when_every_file_is_sound() {
        let vol = check_volume(dev(), &vol_info(volume::VOLUME_IS_DIRTY)).expect("dirty");
        assert_eq!(exit_code(&[clean_record()], &[], Some(&vol), false), 4);
        assert_eq!(
            exit_code(&[clean_record()], &[], Some(&vol), true),
            4,
            "and under --strict too"
        );
    }

    #[test]
    fn a_non_dirty_volume_flag_does_not_fail_the_scan() {
        let vol = check_volume(dev(), &vol_info(volume::VOLUME_MODIFIED_BY_CHKDSK)).expect("flag");
        assert_eq!(
            exit_code(&[clean_record()], &[], Some(&vol), false),
            0,
            "chkdsk having touched the volume does not stop it mounting"
        );
    }

    /// Not being able to read the device is the common case without root. It
    /// must not change the verdict of an otherwise successful audit.
    #[test]
    fn an_unreadable_volume_leaves_the_verdict_alone() {
        assert_eq!(exit_code(&[clean_record()], &[], None, false), 0);
        assert_eq!(exit_code(&[clean_record()], &[], None, true), 0);
    }

    #[test]
    fn gathering_volume_state_from_a_plain_directory_is_not_an_error() {
        // A tempdir is not on an NTFS volume, so this exercises the path that
        // has to stay quiet rather than fail the scan.
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(gather_volume(dir.path()), None);
    }
}

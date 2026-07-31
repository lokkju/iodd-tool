//! Table and JSON rendering for `doctor`.
//!
//! No table crate: `SPEC.md` line 185 keeps the dependency list deliberately
//! short, and column widths are a dozen lines.

use crate::cmd::doctor::{
    Assessment, DirectoryFinding, FileFacts, MAX_ITEMS_PER_DIRECTORY, Verdict, VolumeFinding,
};
use crate::size;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// One graded file, and the unit of `--format json`.
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub path: PathBuf,
    #[serde(rename = "type")]
    pub file_type: String,
    pub size: u64,
    pub extents: usize,
    /// `None` when the file could not be read, which is not a device verdict.
    #[serde(skip)]
    pub verdict: Option<Verdict>,
    #[serde(rename = "verdict")]
    pub verdict_label: String,
    pub reasons: Vec<String>,
}

impl Record {
    #[must_use]
    pub fn new(facts: &FileFacts, assessment: &Assessment) -> Self {
        Self {
            path: facts.path.clone(),
            file_type: facts.ext.clone(),
            size: facts.size,
            extents: facts.run_count,
            verdict: Some(assessment.verdict),
            verdict_label: assessment.verdict.label().to_string(),
            reasons: assessment.reasons.clone(),
        }
    }

    /// A file we could not read. Distinct from any verdict: the device might
    /// be perfectly happy with it, we simply cannot say.
    #[must_use]
    pub fn unreadable(path: &Path, why: &str) -> Self {
        Self {
            path: path.to_path_buf(),
            file_type: path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase(),
            size: 0,
            extents: 0,
            verdict: None,
            verdict_label: "UNREADABLE".to_string(),
            reasons: vec![why.to_string()],
        }
    }
}

/// A directory the device cannot fully list.
#[derive(Debug, Clone, Serialize)]
pub struct DirRecord {
    pub path: PathBuf,
    pub items: usize,
    pub limit: usize,
    pub reason: String,
}

impl From<&DirectoryFinding> for DirRecord {
    fn from(f: &DirectoryFinding) -> Self {
        Self {
            path: f.path.clone(),
            items: f.items,
            limit: MAX_ITEMS_PER_DIRECTORY,
            reason: format!(
                "{} items exceeds the device's limit of {MAX_ITEMS_PER_DIRECTORY} per \
                 directory; files beyond it will not appear in the on-screen list. \
                 Move some into subfolders, which the limit does not count across.",
                f.items
            ),
        }
    }
}

/// Volume state as it appears in `--format json`.
#[derive(Debug, Serialize)]
struct VolRecord {
    device: PathBuf,
    label: Option<String>,
    flags: Vec<String>,
    dirty: bool,
}

impl From<&VolumeFinding> for VolRecord {
    fn from(v: &VolumeFinding) -> Self {
        Self {
            device: v.device.clone(),
            label: v.label.clone(),
            flags: v.flags.clone(),
            dirty: v.dirty,
        }
    }
}

#[derive(Debug, Serialize)]
struct Report<'a> {
    files: &'a [Record],
    directories: Vec<DirRecord>,
    /// Absent when the volume could not be read, which is the usual case
    /// without root — distinct from a volume read and found clean.
    #[serde(skip_serializing_if = "Option::is_none")]
    volume: Option<VolRecord>,
}

pub fn render_json(records: &[Record], dirs: &[DirectoryFinding], vol: Option<&VolumeFinding>) {
    let report = Report {
        files: records,
        directories: dirs.iter().map(DirRecord::from).collect(),
        volume: vol.map(VolRecord::from),
    };
    match serde_json::to_string_pretty(&report) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("iodd: could not render JSON: {e}"),
    }
}

pub fn render_table(records: &[Record], dirs: &[DirectoryFinding], vol: Option<&VolumeFinding>) {
    if records.is_empty() {
        println!("no IODD-mountable files found");
        print_directory_findings(dirs);
        print_volume_finding(vol);
        return;
    }

    let name_width = records
        .iter()
        .map(|r| display_name(&r.path).chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 60);

    println!(
        "{:<name_width$}  TYPE          SIZE  EXTENTS  VERDICT",
        "FILE",
        name_width = name_width
    );

    for r in records {
        let name = truncate(&display_name(&r.path), name_width);
        println!(
            "{:<name_width$}  {:>4}  {:>12}  {:>7}  {}",
            name,
            r.file_type,
            size::humanize(r.size),
            r.extents,
            r.verdict_label,
            name_width = name_width
        );
        for reason in &r.reasons {
            println!("{:<name_width$}    - {reason}", "", name_width = name_width);
        }
    }

    print_summary(records);
    print_directory_findings(dirs);
    print_volume_finding(vol);
}

/// `doctor` pointed straight at a block device: no tree, just the volume.
pub fn render_volume_only(
    device: &Path,
    info: &ntfs_contig::volume::VolumeInfo,
    finding: Option<&VolumeFinding>,
) {
    println!("{}", device.display());
    println!(
        "  label         {}",
        info.label.as_deref().unwrap_or("(none)")
    );
    println!("  ntfs version  {}.{}", info.major, info.minor);
    println!("  flags         0x{:04x}", info.flags);
    if finding.is_none() {
        println!();
        println!("Volume state is clean. No files were examined: pass a mounted path to");
        println!("grade what is on it.");
    }
    print_volume_finding(finding);
}

/// Volume state is printed last and unindented because it outranks everything
/// above it: a dirty volume makes every file listed unreachable.
fn print_volume_finding(vol: Option<&VolumeFinding>) {
    let Some(v) = vol else {
        return;
    };
    println!();
    let name = v.label.as_deref().map_or_else(
        || v.device.display().to_string(),
        |l| format!("{l} ({})", v.device.display()),
    );

    if v.dirty {
        println!("VOLUME DIRTY: {name}");
        println!();
        println!("This volume will not mount again until the flag is cleared, however sound");
        println!("the files above are. The IODD sets it by dropping the volume off the USB");
        println!("bus mid-write when it switches modes, so unmount before using the device's");
        println!("own controls.");
        println!();
        println!(
            "  ntfsfix -d {}        # quick clear on Linux",
            v.device.display()
        );
        println!("  chkdsk /f            # thorough, on Windows");
    } else {
        println!("Volume flags on {name}: {}", v.flags.join(", "));
    }
}

/// Over-full directories hide files rather than breaking them, so they are
/// reported apart from the per-file verdicts: every file listed above may be
/// perfectly good and still be invisible on the device.
fn print_directory_findings(dirs: &[DirectoryFinding]) {
    if dirs.is_empty() {
        return;
    }
    println!();
    println!(
        "{} director{} over the device's {MAX_ITEMS_PER_DIRECTORY}-item limit:",
        dirs.len(),
        if dirs.len() == 1 { "y" } else { "ies" }
    );
    for d in dirs {
        println!("  {} — {} items", d.path.display(), d.items);
    }
    println!();
    println!(
        "Files past the first {MAX_ITEMS_PER_DIRECTORY} will not appear in the on-screen list, \
         however sound they are."
    );
    println!("Move some into subfolders; the limit counts per directory, not per drive.");
}

fn print_summary(records: &[Record]) {
    let mut mountable = 0usize;
    let mut warn = 0usize;
    let mut failing = 0usize;
    let mut unreadable = 0usize;

    for r in records {
        match r.verdict {
            Some(Verdict::Mountable) => mountable += 1,
            Some(Verdict::Warn) => warn += 1,
            Some(v) if v.is_failure() => failing += 1,
            Some(_) => {}
            None => unreadable += 1,
        }
    }

    println!();
    // A warned file still mounts, so counting it apart from "mountable" would
    // read as though it did not. The device's own verdict is the parenthetical.
    println!(
        "{} file(s): {} will mount ({warn} with warnings), {failing} will not{}",
        records.len(),
        mountable + warn,
        if unreadable > 0 {
            format!(", {unreadable} unreadable")
        } else {
            String::new()
        }
    );

    if failing > 0 {
        println!();
        println!(
            "Files that will not mount cannot be repaired from Linux: nothing here can \
             relocate NTFS clusters."
        );
        println!(
            "Consolidate free space on Windows (built-in defrag, or Sysinternals Contig on \
             the single file),"
        );
        println!("or re-lay the file on a volume with a large enough contiguous free run.");
    }
}

/// Keep the leading directory for context, since IODD layouts nest.
fn display_name(path: &Path) -> String {
    let file = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let joined = match path.parent().and_then(Path::file_name) {
        Some(dir) => format!("{}/{file}", dir.to_string_lossy()),
        None => file,
    };
    escape_control(&joined)
}

/// Render control characters visibly instead of letting them out.
///
/// Not hypothetical: a real IODD carries an ISO whose filename contains an
/// embedded newline, which split one row across two lines and misaligned every
/// column after it. Filenames are attacker-adjacent data on a device that gets
/// passed around, so a terminal escape sequence is worth neutering too.
fn escape_control(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32 & 0xff)),
            c => out.push(c),
        }
    }
    out
}

/// Truncate from the left: the tail of an IODD filename carries the version and
/// the mode tags, which is the part worth keeping.
fn truncate(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        return s.to_string();
    }
    let keep = width.saturating_sub(1);
    let skip = count - keep;
    format!("…{}", s.chars().skip(skip).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_keeps_one_directory_of_context() {
        assert_eq!(display_name(Path::new("/mnt/_ISO/a.iso")), "_ISO/a.iso");
        assert_eq!(display_name(Path::new("a.iso")), "a.iso");
    }

    /// Taken from a real device: an ISO whose filename contains an embedded
    /// newline, which split its table row in two and misaligned everything
    /// after it.
    #[test]
    fn a_newline_in_a_filename_does_not_break_the_table() {
        let name = "/mnt/_ISO/Windows Server 2012 R2 with Update 1\n 9600.ISO";
        let shown = display_name(Path::new(name));
        assert!(
            !shown.contains('\n'),
            "must not emit a raw newline: {shown}"
        );
        assert!(shown.contains("\\n"), "and should show it escaped: {shown}");
    }

    #[test]
    fn control_characters_are_neutered() {
        assert_eq!(escape_control("a\tb"), "a\\tb");
        assert_eq!(escape_control("a\rb"), "a\\rb");
        assert_eq!(escape_control("a\u{1b}[31mred"), "a\\x1b[31mred");
        assert_eq!(escape_control("plain"), "plain");
    }

    #[test]
    fn truncate_keeps_the_tail_within_the_width() {
        // Mode tags and versions live at the end of IODD filenames, so the tail
        // is the part worth keeping. The result is exactly `width` chars,
        // ellipsis included, or the column alignment breaks.
        assert_eq!(truncate("abcdefghij", 5), "…ghij");
        assert_eq!(truncate("abcdefghij", 5).chars().count(), 5);
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        // Multi-byte names must not be split mid-character or over-truncated.
        let s = "日本語のファイル名";
        let out = truncate(s, 4);
        assert_eq!(out.chars().count(), 4);
        assert!(out.starts_with('…'));
    }

    #[test]
    fn unreadable_records_carry_no_verdict() {
        let r = Record::unreadable(Path::new("/x/a.vhd"), "permission denied");
        assert!(r.verdict.is_none());
        assert_eq!(r.verdict_label, "UNREADABLE");
        assert_eq!(r.file_type, "vhd");
    }

    #[test]
    fn json_field_names_are_stable() {
        // This is the scripting interface; renaming a field breaks callers.
        let r = Record {
            path: PathBuf::from("/x/a.vhd"),
            file_type: "vhd".into(),
            size: 1024,
            extents: 1,
            verdict: Some(Verdict::Mountable),
            verdict_label: "MOUNTABLE".into(),
            reasons: vec!["note".into()],
        };
        let text = serde_json::to_string(&[r].as_slice()).expect("serialize");
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
        assert!(text.contains("MOUNTABLE"));
    }
}

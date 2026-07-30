//! Table and JSON rendering for `doctor`.
//!
//! No table crate: `SPEC.md` line 185 keeps the dependency list deliberately
//! short, and column widths are a dozen lines.

use crate::cmd::doctor::{Assessment, FileFacts, Verdict};
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

pub fn render_json(records: &[Record]) {
    match serde_json::to_string_pretty(records) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("iodd: could not render JSON: {e}"),
    }
}

pub fn render_table(records: &[Record]) {
    if records.is_empty() {
        println!("no IODD-mountable files found");
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
    println!(
        "{} file(s): {mountable} mountable, {warn} warn, {failing} will not mount{}",
        records.len(),
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
    match path.parent().and_then(Path::file_name) {
        Some(dir) => format!("{}/{file}", dir.to_string_lossy()),
        None => file,
    }
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

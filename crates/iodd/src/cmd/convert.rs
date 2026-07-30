//! `iodd convert` — write an existing image into a contiguous fixed VHD/RMD.
//!
//! Raw sources are copied natively. Anything else is handed to `qemu-img`,
//! which is an optional runtime dependency: absent it, non-raw sources exit 9
//! *before* anything is allocated.
//!
//! # Two flags that are not optional
//!
//! `qemu-img convert -n` skips target creation, so qemu writes into the file we
//! already allocated instead of truncating and recreating it. Without `-n` the
//! careful allocation is discarded before the first byte lands.
//!
//! `-S 0` disables qemu's zero-run sparsification. Without it qemu punches
//! holes for runs of zeros, reintroducing exactly the sparseness this tool
//! exists to prevent. Design revision 2 makes it mandatory rather than
//! advisory.

use crate::cli::ConvertArgs;
use crate::error::{Error, Result};
use crate::footer::{Creator, Footer};
use crate::size;
use crate::source::{self, SourceKind};
use ntfs_contig::alloc::{self, AllocOutcome, NoProgress, Progress, TempTarget, TtyProgress};
use ntfs_contig::fsinfo;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Chunk size for the native raw copy.
const COPY_CHUNK: usize = 4 * 1024 * 1024;

pub fn run(args: &ConvertArgs) -> Result<u8> {
    let src = args.source.as_path();

    let src_file = std::fs::File::open(src).map_err(|source| Error::Io {
        path: src.to_path_buf(),
        source,
    })?;
    let src_meta = src_file.metadata().map_err(|source| Error::Io {
        path: src.to_path_buf(),
        source,
    })?;
    if src_meta.is_dir() {
        return Err(Error::Usage(format!("{} is a directory", src.display())));
    }

    // ---- what is it? -------------------------------------------------------
    let mut head = [0u8; 512];
    let _ = std::os::unix::fs::FileExt::read_at(&src_file, &mut head, 0);
    let kind = source::sniff(&head);

    let source_virtual_size = match &kind {
        SourceKind::Raw => src_meta.len(),
        SourceKind::Container(name) => {
            println!("source is a {name} container; using qemu-img");
            qemu_virtual_size(src)?
        }
    };

    if source_virtual_size == 0 {
        return Err(Error::Usage(format!("{} is empty", src.display())));
    }

    // ---- target size -------------------------------------------------------
    // `SPEC.md` line 123 says max(source, --size); line 98 says --size must be
    // >= source. Design revision 5 picks the latter: silently enlarging past
    // what the operator asked for hides a mistake rather than reporting it.
    let virtual_size = match &args.size {
        None => source_virtual_size,
        Some(text) => {
            let requested = size::parse(text)?;
            if requested < source_virtual_size {
                return Err(Error::Usage(format!(
                    "--size {requested} is smaller than the source's virtual size \
                     {source_virtual_size}; convert can enlarge but never truncate"
                )));
            }
            requested
        }
    };
    size::require_512_aligned(virtual_size)?;
    if let Some(warning) = size::mib_alignment_warning(virtual_size) {
        eprintln!("iodd: warning: {warning}");
    }

    let creator = Creator::parse(&args.creator).map_err(Error::Usage)?;
    let out = crate::cmd::create::resolve_extension(&args.out, args.removable);
    let total_size = virtual_size
        .checked_add(512)
        .ok_or_else(|| Error::Usage("size + 512 overflows a 64-bit byte count".into()))?;

    if out.exists() && !args.force {
        return Err(Error::TargetExists { path: out });
    }
    if same_file(src, &out) {
        return Err(Error::Usage(
            "the source and the target are the same file".into(),
        ));
    }

    let dir = alloc::parent_dir(&out);
    if !dir.is_dir() {
        return Err(Error::Usage(format!(
            "{} is not a directory",
            dir.display()
        )));
    }

    // Fail before allocating anything if the tool we need is missing.
    if kind.needs_qemu() {
        require_qemu()?;
    }

    println!(
        "converting {} ({}) to {} ({} virtual + 512 footer)",
        src.display(),
        size::humanize(source_virtual_size),
        out.display(),
        size::humanize(virtual_size)
    );

    // ---- allocate and check before writing anything ------------------------
    let target = TempTarget::create(&out, args.keep_on_fail)?;
    let outcome = alloc::allocate(target.file(), total_size, target.path())?;
    let must_write = outcome == AllocOutcome::Unsupported;
    if must_write {
        eprintln!(
            "iodd: warning: this filesystem does not support fallocate; the file \
             will be allocated by writing it"
        );
    }
    if outcome == AllocOutcome::Reserved {
        alloc::require_contiguous(target.file(), total_size, target.path())?;
    }

    // ---- zero only what the source will not cover --------------------------
    // The source overwrites [0, source_virtual_size), so zeroing that range too
    // would be wasted work. The post-write check over the whole file is what
    // makes the narrowing safe rather than optimistic.
    let zeroed = args.zero || must_write;
    if zeroed {
        let start = if must_write { 0 } else { source_virtual_size };
        let mut tty = TtyProgress::new("zeroing tail");
        let mut none = NoProgress;
        let progress: &mut dyn Progress = match tty.as_mut() {
            Some(p) => p,
            None => &mut none,
        };
        alloc::zero_region(target.file(), start, total_size, target.path(), progress)?;
    }

    // ---- populate ----------------------------------------------------------
    match kind {
        SourceKind::Raw => copy_raw(&src_file, &target, source_virtual_size, src)?,
        SourceKind::Container(_) => run_qemu_convert(src, target.path())?,
    }

    target.file().sync_data().map_err(|source| Error::Io {
        path: target.path().to_path_buf(),
        source,
    })?;

    // ---- verify, footer, publish -------------------------------------------
    alloc::verify_written(target.file(), total_size, target.path(), zeroed)?;

    let footer = Footer::now(virtual_size, creator);
    target.write_at(&footer.to_bytes(), virtual_size)?;

    alloc::require_contiguous(target.file(), total_size, target.path())?;
    if matches!(
        fsinfo::is_compressed(std::os::fd::AsFd::as_fd(target.file())),
        Ok(Some(true))
    ) {
        return Err(Error::Compressed { path: out.clone() });
    }

    target.finalize(args.force)?;

    println!(
        "  geometry      {}/{}/{}",
        footer.geometry.cylinders, footer.geometry.heads, footer.geometry.sectors_per_track
    );
    println!("  contiguous    yes (1 extent)");
    println!("  zeroed        {}", if zeroed { "yes" } else { "no" });
    println!("{} created", out.display());

    if !zeroed && virtual_size > source_virtual_size {
        eprintln!();
        eprintln!(
            "iodd: warning: {} beyond the source was NOT zeroed and holds whatever \
             was previously on those clusters.",
            size::humanize(virtual_size - source_virtual_size)
        );
        eprintln!("iodd: pass --zero if the disk may be seen by anyone else.");
    }

    Ok(0)
}

/// Copy a raw source into the already-allocated target.
///
/// Plain positioned writes. Deliberately not `copy_file_range`, which can share
/// extents between the two files on some filesystems — the exact opposite of
/// what this tool is for.
fn copy_raw(
    src_file: &std::fs::File,
    target: &TempTarget,
    len: u64,
    src_path: &Path,
) -> Result<()> {
    let mut tty = TtyProgress::new("copying");
    let mut none = NoProgress;
    let progress: &mut dyn Progress = match tty.as_mut() {
        Some(p) => p,
        None => &mut none,
    };

    let mut buf = vec![0u8; COPY_CHUNK];
    let mut offset = 0u64;
    let mut reader = src_file;

    while offset < len {
        let want = usize::try_from((len - offset).min(COPY_CHUNK as u64)).unwrap_or(COPY_CHUNK);
        let mut filled = 0usize;
        while filled < want {
            match reader.read(&mut buf[filled..want]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(source) => {
                    return Err(Error::Io {
                        path: src_path.to_path_buf(),
                        source,
                    });
                }
            }
        }
        if filled == 0 {
            break;
        }
        target.write_at(&buf[..filled], offset)?;
        offset += filled as u64;
        progress.update(offset, len);
    }
    progress.finish();
    Ok(())
}

fn require_qemu() -> Result<()> {
    which_qemu().map(|_| ()).ok_or_else(|| Error::QemuImg {
        reason: "qemu-img is not on PATH; it is required for non-raw sources".into(),
    })
}

fn which_qemu() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("qemu-img"))
        .find(|candidate| candidate.is_file())
}

fn qemu_virtual_size(src: &Path) -> Result<u64> {
    require_qemu()?;
    let out = std::process::Command::new("qemu-img")
        .args(["info", "--output=json"])
        .arg(src)
        .output()
        .map_err(|e| Error::QemuImg {
            reason: format!("could not run qemu-img info: {e}"),
        })?;

    if !out.status.success() {
        return Err(Error::QemuImg {
            reason: format!(
                "qemu-img info failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| Error::QemuImg {
        reason: format!("could not parse qemu-img info output: {e}"),
    })?;
    value
        .get("virtual-size")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| Error::QemuImg {
            reason: "qemu-img info did not report a virtual-size".into(),
        })
}

fn run_qemu_convert(src: &Path, target: &Path) -> Result<()> {
    // -n so qemu writes into our pre-allocated file rather than recreating it.
    // -S 0 so it never punches holes for zero runs. Both are load-bearing.
    let out = std::process::Command::new("qemu-img")
        .args(["convert", "-n", "-S", "0", "-O", "raw"])
        .arg(src)
        .arg(target)
        .output()
        .map_err(|e| Error::QemuImg {
            reason: format!("could not run qemu-img convert: {e}"),
        })?;

    if !out.status.success() {
        return Err(Error::QemuImg {
            reason: format!(
                "qemu-img convert failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_file_detects_identity_through_different_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.img");
        std::fs::write(&a, b"x").expect("write");
        let via_dot = dir.path().join(".").join("a.img");
        assert!(same_file(&a, &via_dot));

        let b = dir.path().join("b.img");
        std::fs::write(&b, b"x").expect("write");
        assert!(!same_file(&a, &b), "distinct files must not compare equal");
    }

    #[test]
    fn same_file_is_false_when_either_is_missing() {
        assert!(!same_file(
            Path::new("/nonexistent-a"),
            Path::new("/nonexistent-b")
        ));
    }
}

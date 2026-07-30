//! `iodd create` — a blank fixed VHD/RMD of a given size.
//!
//! # Zeroing is off by default (design D9)
//!
//! `VhdTool.exe`, which is what VHD Tool++ actually shells out to, allocates
//! with `SetFileValidData` and never writes the data region. That is what
//! "instantly creates fixed-size VHD files" means, and it is why the tool needs
//! `SE_MANAGE_VOLUME` — the privilege exists precisely because the API exposes
//! deleted data.
//!
//! We match that, because instant creation is the point. `--zero` opts into a
//! full pass, and without it `create` prints a warning that says plainly what
//! the file contains.

use crate::cli::CreateArgs;
use crate::error::{Error, Result};
use crate::footer::{Creator, Footer};
use crate::size;
use ntfs_contig::alloc::{self, AllocOutcome, NoProgress, Progress, TempTarget, TtyProgress};
use ntfs_contig::fsinfo;
use std::path::{Path, PathBuf};

pub fn run(args: &CreateArgs) -> Result<u8> {
    // ---- validate ----------------------------------------------------------
    let virtual_size = size::parse(&args.size)?;
    size::require_512_aligned(virtual_size)?;
    if let Some(warning) = size::mib_alignment_warning(virtual_size) {
        eprintln!("iodd: warning: {warning}");
    }
    // VHD Tool++ refuses anything under 12 MB. Whether that is a device floor
    // or a VhdTool one is unknown, so this warns rather than refuses.
    const VENDOR_MINIMUM: u64 = 12 * 1024 * 1024;
    if virtual_size < VENDOR_MINIMUM {
        eprintln!(
            "iodd: warning: {} is below the 12 MiB minimum VHD Tool++ enforces; \
             the device may not mount it",
            size::humanize(virtual_size)
        );
    }

    let creator = Creator::parse(&args.creator).map_err(Error::Usage)?;
    let out = resolve_extension(&args.out, args.removable);

    // Measured on hardware: the device decides by extension. A `.vhd` is
    // presented as file size minus 512, on the assumption that the last sector
    // is a footer; a `.rmd` is presented whole. So a removable image is a raw
    // image of exactly the requested size and carries no footer, matching the
    // `.ima` files the device itself ships. Giving it a footer would hand the
    // guest 512 bytes of it as trailing disk. See design F1 and D10.
    let raw = out
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("rmd"));

    let total_size = if raw {
        virtual_size
    } else {
        virtual_size
            .checked_add(u64::try_from(crate::footer::FOOTER_SIZE).unwrap_or(512))
            .ok_or_else(|| Error::Usage("size + 512 overflows a 64-bit byte count".into()))?
    };

    if out.exists() && !args.force {
        return Err(Error::TargetExists { path: out });
    }

    let dir = alloc::parent_dir(&out);
    if !dir.is_dir() {
        return Err(Error::Usage(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    // Pre-flight: a new file inherits the directory's compression attribute, so
    // catching it here avoids doing the work first. It is checked again on the
    // created file before publishing.
    if let Ok(d) = std::fs::File::open(dir)
        && matches!(
            fsinfo::is_compressed(std::os::fd::AsFd::as_fd(&d)),
            Ok(Some(true))
        )
    {
        return Err(Error::Compressed {
            path: dir.to_path_buf(),
        });
    }

    if raw {
        println!(
            "creating {} (raw removable image, {} = {} bytes, no footer)",
            out.display(),
            size::humanize(virtual_size),
            total_size
        );
    } else {
        println!(
            "creating {} ({} virtual + 512 footer = {} bytes)",
            out.display(),
            size::humanize(virtual_size),
            total_size
        );
    }

    // ---- 1. reserve --------------------------------------------------------
    let target = TempTarget::create(&out, args.keep_on_fail)?;
    let outcome = alloc::allocate(target.file(), total_size, target.path())?;

    let must_write = match outcome {
        AllocOutcome::Reserved => false,
        AllocOutcome::Unsupported => {
            eprintln!(
                "iodd: warning: this filesystem does not support fallocate; \
                 the file will be allocated by writing it, which forces a full pass"
            );
            true
        }
    };

    // ---- 2. early extent check --------------------------------------------
    // Before any data is written, so a doomed target costs nothing.
    if outcome == AllocOutcome::Reserved
        && let Err(e) = alloc::require_contiguous(target.file(), total_size, target.path())
    {
        // Release our own reservation *before* measuring. Otherwise the failed
        // allocation is still holding the space, and both the free-space figure
        // and the largest-run probe come back short by exactly the size we are
        // about to give back — which is precisely the number the operator is
        // trying to learn.
        let kept = args.keep_on_fail;
        drop(target);
        report_refusal(&e, dir, total_size, kept);
        return Err(e.into());
    }

    // ---- 3. zero, if asked -------------------------------------------------
    let zeroed = args.zero || must_write;
    if zeroed {
        let mut tty = TtyProgress::new("zeroing");
        let mut none = NoProgress;
        let progress: &mut dyn Progress = match tty.as_mut() {
            Some(p) => p,
            None => &mut none,
        };
        alloc::zero_region(target.file(), 0, total_size, target.path(), progress)?;
    }

    // ---- 4. post-write checks ---------------------------------------------
    alloc::verify_written(target.file(), total_size, target.path(), zeroed)?;

    // ---- 5. footer in place, unless this is a raw removable image ---------
    let footer = (!raw).then(|| Footer::now(virtual_size, creator));
    if let Some(f) = &footer {
        target.write_at(&f.to_bytes(), virtual_size)?;
    }

    // ---- 6. re-verify, then publish ---------------------------------------
    alloc::require_contiguous(target.file(), total_size, target.path())?;
    if matches!(
        fsinfo::is_compressed(std::os::fd::AsFd::as_fd(target.file())),
        Ok(Some(true))
    ) {
        return Err(Error::Compressed { path: out.clone() });
    }

    target.finalize(args.force)?;

    if let Some(f) = &footer {
        println!(
            "  geometry      {}/{}/{}",
            f.geometry.cylinders, f.geometry.heads, f.geometry.sectors_per_track
        );
        println!("  creator       {creator}");
    } else {
        println!("  footer        none (raw removable image)");
    }
    println!("  contiguous    yes (1 extent)");
    println!("  zeroed        {}", if zeroed { "yes" } else { "no" });
    println!("  guest sees    {}", size::humanize(virtual_size));
    println!("{} created", out.display());

    if !zeroed {
        eprintln!();
        eprintln!(
            "iodd: warning: the data region was NOT zeroed. It holds whatever was \
             previously on those clusters."
        );
        eprintln!(
            "iodd: the filesystem will report zeros, but the IODD reads sectors \
             straight off the device and will hand the guest the old bytes."
        );
        eprintln!(
            "iodd: this matches VhdTool.exe. If the disk may be seen by anyone else, \
             or booted on a machine you do not control, pass --zero."
        );
    }

    Ok(0)
}

/// Explain a refusal in terms an operator can act on.
///
/// `SPEC.md` line 162 asks for the extent map, the file size, and the largest
/// contiguous free run. The first two we have; the third we measure, since an
/// unzeroed allocation is cheap enough to probe for (design D9's unintended
/// dividend). Knowing that 8 GiB would fit is far more use than being told
/// 12 GiB will not.
fn report_refusal(err: &ntfs_contig::Error, dir: &Path, requested: u64, kept: bool) {
    let ntfs_contig::Error::Fragmented { runs, .. } = err else {
        return;
    };

    eprintln!();
    eprintln!(
        "iodd: {} could not be allocated in one extent. It came back as {}:",
        size::humanize(requested),
        runs.len()
    );
    for (i, run) in runs.iter().take(8).enumerate() {
        eprintln!(
            "iodd:   run {i}: physical={} length={}",
            run.physical,
            size::humanize(run.length)
        );
    }
    if runs.len() > 8 {
        eprintln!("iodd:   ... and {} more", runs.len() - 8);
    }

    if kept {
        eprintln!(
            "iodd: note: --keep-on-fail is retaining the partial file, so the \
             figures below exclude the space it still holds."
        );
    }

    if let Some(free) = fsinfo::free_space(dir) {
        eprintln!("iodd: free space on this volume: {}", size::humanize(free));
    }

    eprint!("iodd: measuring the largest contiguous run available... ");
    match alloc::probe_largest_contiguous(dir, requested, 64 * 1024 * 1024) {
        Some(best) => {
            eprintln!("{}", size::humanize(best));
            eprintln!(
                "iodd: a target of {} or less should succeed right now.",
                size::humanize(best)
            );
        }
        None => eprintln!("nothing usable"),
    }

    eprintln!(
        "iodd: nothing on Linux can relocate NTFS clusters, so this cannot be \
         repaired here."
    );
    eprintln!(
        "iodd: consolidate free space on Windows (built-in defrag, or Sysinternals \
         Contig on one file), or use a volume with a larger contiguous free run."
    );
}

/// Apply the default extension, and warn when an explicit one contradicts
/// `--removable`.
///
/// `.vhd` presents as a USB fixed drive and `.rmd` as removable; the bytes are
/// identical either way, so this is presentation only.
///
/// Only *recognized* extensions count as deliberate. `Path` treats anything
/// after the last dot as an extension, so honouring whatever it finds would
/// leave `--out "Win11 23H2.v2"` named exactly that, with no `.vhd` at all and
/// nothing the device would recognize. A dot in the stem is far commoner than
/// an exotic deliberate extension.
pub(crate) fn resolve_extension(out: &Path, removable: bool) -> PathBuf {
    /// Extensions IODD recognizes; anything else is treated as part of the name.
    const RECOGNIZED: [&str; 5] = ["vhd", "rmd", "vmdk", "ima", "iso"];

    let wanted = if removable { "rmd" } else { "vhd" };

    let Some(ext) = out.extension().and_then(|e| e.to_str()) else {
        return append_extension(out, wanted);
    };

    let lower = ext.to_ascii_lowercase();
    if !RECOGNIZED.contains(&lower.as_str()) {
        return append_extension(out, wanted);
    }

    if (lower == "rmd" && !removable) || (lower == "vhd" && removable) {
        eprintln!(
            "iodd: warning: --out ends in .{lower} but --removable {}; \
             honouring the extension you gave",
            if removable {
                "was given"
            } else {
                "was not given"
            }
        );
    }
    out.to_path_buf()
}

/// Append rather than replace, so dotted stems survive.
fn append_extension(out: &Path, ext: &str) -> PathBuf {
    let mut name = out.as_os_str().to_os_string();
    name.push(".");
    name.push(ext);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_defaults_to_vhd_or_rmd() {
        assert_eq!(
            resolve_extension(Path::new("/a/disk"), false),
            PathBuf::from("/a/disk.vhd")
        );
        assert_eq!(
            resolve_extension(Path::new("/a/disk"), true),
            PathBuf::from("/a/disk.rmd")
        );
    }

    #[test]
    fn an_explicit_extension_is_honoured() {
        // Including a contradictory one: the operator was specific, so we warn
        // rather than override.
        assert_eq!(
            resolve_extension(Path::new("/a/disk.rmd"), false),
            PathBuf::from("/a/disk.rmd")
        );
        assert_eq!(
            resolve_extension(Path::new("/a/disk.vhd"), true),
            PathBuf::from("/a/disk.vhd")
        );
    }

    #[test]
    fn other_recognized_extensions_are_left_alone() {
        // IODD reads .vmdk as fixed-VHD bytes under another name.
        assert_eq!(
            resolve_extension(Path::new("/a/disk.vmdk"), false),
            PathBuf::from("/a/disk.vmdk")
        );
    }

    #[test]
    fn an_unrecognized_suffix_is_treated_as_part_of_the_name() {
        // Path calls ".v2" an extension. Honouring it would leave the file
        // named "Win11 23H2.v2", which the device would not recognize.
        assert_eq!(
            resolve_extension(Path::new("/a/Win11 23H2.v2"), false),
            PathBuf::from("/a/Win11 23H2.v2.vhd")
        );
        assert_eq!(
            resolve_extension(Path::new("/a/backup.2026.01"), true),
            PathBuf::from("/a/backup.2026.01.rmd")
        );
    }

    #[test]
    fn mode_tags_in_filenames_survive() {
        // "&D" and "&DW" are IODD dual-mode tags and live in the filename.
        assert_eq!(
            resolve_extension(Path::new("/a/disk&DW"), false),
            PathBuf::from("/a/disk&DW.vhd")
        );
    }
}

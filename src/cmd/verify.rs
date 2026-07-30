//! `iodd verify` — inspect a single existing file.
//!
//! # Exit policy, which inverts what `SPEC.md` describes
//!
//! `SPEC.md` line 100 has `--strict` promote fragmentation and sparseness from
//! warnings to failures. Design decisions D2 and D3 invert that:
//!
//! * Fragmentation (exit 4) and sparseness (exit 5) fail **always**. These are
//!   the two properties the device actually enforces, so making them
//!   conditional on a flag would be misleading — the device will reject the
//!   file regardless of how we were invoked.
//! * Footer problems are **warnings** by default, and `--strict` promotes them
//!   to exit 7. Measurement showed the device mounts files with no valid footer
//!   at all, and that a guest partitioning the disk overwrites the footer with
//!   its GPT backup header. Failing on that would report a working file as
//!   broken (FINDINGS F1, F2).

use crate::cli::VerifyArgs;
use crate::error::{Error, Result};
use crate::{extents, footer, fsinfo, size};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;

pub fn run(args: &VerifyArgs) -> Result<u8> {
    let path = args.path.as_path();

    let mut file = File::open(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let meta = file.metadata().map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;

    if meta.is_dir() {
        return Err(Error::Usage(format!("{} is a directory", path.display())));
    }

    let file_size = meta.len();
    let allocated = meta.blocks().saturating_mul(512);
    let map = extents::map(file.as_fd(), file_size, path)?;

    println!("{}", path.display());
    println!(
        "  size          {file_size} ({})",
        size::humanize(file_size)
    );
    println!("  allocated     {allocated}  (st_blocks {})", meta.blocks());
    println!("  raw records   {}", map.raw_records);
    println!(
        "  merged runs   {}   contiguous={}",
        map.runs.len(),
        map.is_contiguous()
    );
    println!("  any unwritten {}", map.any_unwritten);
    if let Some(magic) = fsinfo::fs_magic(file.as_fd()) {
        let name = match magic {
            fsinfo::NTFS3_MAGIC => " (ntfs3)",
            fsinfo::NTFS_LEGACY_MAGIC => " (ntfs, legacy driver)",
            _ => "",
        };
        println!("  filesystem    0x{magic:x}{name}");
    }
    if matches!(fsinfo::is_compressed(file.as_fd()), Ok(Some(true))) {
        println!("  compressed    yes  (fixed allocation cannot be guaranteed)");
    }

    // ---- footer: reported, never fatal unless --strict ---------------------
    let mut footer_problems: Vec<String> = Vec::new();

    if file_size < footer::FOOTER_SIZE as u64 {
        footer_problems.push(format!(
            "file is {file_size} bytes, too small to hold a 512-byte footer"
        ));
        println!("  footer        absent (file too small)");
    } else {
        let mut raw = [0u8; footer::FOOTER_SIZE];
        file.seek(SeekFrom::Start(file_size - footer::FOOTER_SIZE as u64))
            .map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?;
        file.read_exact(&mut raw).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let expected_virtual = file_size - footer::FOOTER_SIZE as u64;
        let (parsed, problems) = footer::parse(&raw, Some(expected_virtual));

        if problems.is_empty() {
            println!("  footer        valid");
            println!(
                "  virtual size  {} ({})",
                parsed.virtual_size,
                size::humanize(parsed.virtual_size)
            );
            println!(
                "  geometry      {}/{}/{}  (product {} sectors)",
                parsed.geometry.cylinders,
                parsed.geometry.heads,
                parsed.geometry.sectors_per_track,
                parsed.geometry.product()
            );
            println!("  creator       {}", parsed.creator);
            println!("  checksum      0x{:08x}", parsed.checksum);
        } else {
            println!("  footer        {} problem(s)", problems.len());
            for p in &problems {
                println!("                - {p}");
                footer_problems.push(p.to_string());
            }
        }
    }

    // ---- hard gate 1: full allocation --------------------------------------
    // Checked before contiguity so a sparse file reports the more specific
    // problem; a sparse file is often fragmented too, and "not allocated" is
    // the more actionable of the two.
    if file_size > 0 && allocated < file_size {
        return Err(Error::Sparse {
            path: path.to_path_buf(),
            allocated,
            expected: file_size,
        });
    }
    if map.any_unwritten {
        return Err(Error::Uninitialized {
            path: path.to_path_buf(),
        });
    }

    // An allocated file that reports no extents at all is not something we are
    // willing to call contiguous. `SPEC.md` line 158 is explicit that an empty
    // or failed map must be treated as untrusted rather than as success. The
    // sparseness gate above already caught the honest case, a file that is all
    // hole, so reaching here means st_blocks and FIEMAP disagree.
    if file_size > 0 && map.runs.is_empty() {
        return Err(Error::Unverifiable {
            path: path.to_path_buf(),
            reason: format!(
                "{allocated} bytes are allocated but FS_IOC_FIEMAP reported no extents"
            ),
        });
    }

    // ---- hard gate 2: one extent ------------------------------------------
    if !map.is_contiguous() {
        for (i, run) in map.runs.iter().enumerate() {
            println!(
                "    run {i}: logical={} physical={} length={}",
                run.logical, run.physical, run.length
            );
        }
        return Err(Error::Fragmented {
            path: path.to_path_buf(),
            runs: map.runs.clone(),
            free_bytes: path.parent().and_then(fsinfo::free_space),
            largest_free_run: path.parent().and_then(fsinfo::largest_free_run),
        });
    }

    // ---- soft findings -----------------------------------------------------
    if !footer_problems.is_empty() {
        if args.strict {
            return Err(Error::Footer {
                path: path.to_path_buf(),
                reason: footer_problems.join("; "),
            });
        }
        eprintln!(
            "iodd: warning: {} footer problem(s); the device mounts files \
             without a valid footer, so this is not fatal. Use --strict to fail.",
            footer_problems.len()
        );
    }

    Ok(0)
}

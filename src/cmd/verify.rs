//! `iodd verify` — inspect a single existing file.
//!
//! Phase 2 implements the two gates the device actually enforces: the file
//! must occupy one extent, and it must be fully allocated. Footer checking
//! lands in phase 3.
//!
//! Note the exit policy, which inverts what `SPEC.md` describes. Design
//! decisions D2 and D3 make fragmentation and sparseness the only hard gates,
//! so they fail with or without `--strict`; `--strict` instead promotes the
//! soft findings (footer, geometry) to a failure. Making the hard gates
//! conditional would be misleading, since the device rejects such files
//! regardless of which flags we were invoked with.

use crate::cli::VerifyArgs;
use crate::error::{Error, Result};
use crate::{extents, fsinfo};
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;

pub fn run(args: &VerifyArgs) -> Result<u8> {
    let path = args.path.as_path();

    let file = File::open(path).map_err(|source| Error::Io {
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

    let size = meta.len();
    let allocated = meta.blocks().saturating_mul(512);
    let map = extents::map(file.as_fd(), size, path)?;

    println!("{}", path.display());
    println!("  size          {size}");
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
        println!("  compressed    yes");
    }
    println!("  footer        not checked (phase 3)");

    // ---- hard gate 1: full allocation -------------------------------------
    // Checked before contiguity so a sparse file reports the more specific
    // problem; a sparse file is often fragmented too, and "not allocated" is
    // the more actionable of the two.
    if size > 0 && allocated < size {
        return Err(Error::Sparse {
            path: path.to_path_buf(),
            allocated,
            expected: size,
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
    if size > 0 && map.runs.is_empty() {
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

    Ok(0)
}

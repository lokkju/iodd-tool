//! `iodd retype` — rename between `.vhd` and `.rmd` without rewriting bytes.
//!
//! Fixed versus removable is an IODD presentation concept selected by the file
//! extension, not a VHD-format concept. Both carry identical bytes and both
//! have `Disk Type = 2` in the footer, so converting between them is a rename
//! and nothing more (`SPEC.md` lines 26-28).
//!
//! The rename happens within one directory, so it preserves the extent layout.
//! Nothing is read from or written to the file's contents.

use crate::cli::{Presentation, RetypeArgs};
use crate::error::{Error, Result};
use crate::size;
use std::path::{Path, PathBuf};

/// `.vhd` presents as a USB fixed drive, `.rmd` as a USB removable drive.
const FIXED_EXT: &str = "vhd";
const REMOVABLE_EXT: &str = "rmd";

pub fn run(args: &RetypeArgs) -> Result<u8> {
    let path = args.path.as_path();

    let current = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| {
            Error::Usage(format!(
                "{} has no extension; retype only converts between .vhd and .rmd",
                path.display()
            ))
        })?;

    if current != FIXED_EXT && current != REMOVABLE_EXT {
        return Err(Error::Usage(format!(
            "{} has extension .{current}; retype only converts between .vhd and .rmd",
            path.display()
        )));
    }

    if !path.exists() {
        return Err(Error::Io {
            path: path.to_path_buf(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        });
    }
    if path.is_dir() {
        return Err(Error::Usage(format!("{} is a directory", path.display())));
    }

    let wanted = match args.to {
        Presentation::Fixed => FIXED_EXT,
        Presentation::Removable => REMOVABLE_EXT,
    };

    if current == wanted {
        println!("{} is already .{wanted}; nothing to do", path.display());
        return Ok(0);
    }

    let target = with_extension(path, wanted);

    // Refuse to clobber. Renaming over an existing file would destroy it
    // silently, and there is no flag that authorises that.
    if target.exists() {
        return Err(Error::TargetExists { path: target });
    }

    let size = std::fs::metadata(path)
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();

    match args.to {
        // .vhd -> .rmd: drop the footer.
        //
        // As a .vhd the last 512 bytes are outside the guest's view by
        // definition, so removing them costs the guest nothing — and it keeps
        // the guest-visible size identical across the conversion. Truncation
        // only ever releases the final cluster, so it cannot fragment what
        // remains.
        Presentation::Removable => {
            if size < 512 {
                return Err(Error::Usage(format!(
                    "{} is {size} bytes, too small to be a fixed VHD",
                    path.display()
                )));
            }
            let guest_before = size - 512;

            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|source| Error::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
            file.set_len(guest_before).map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?;
            file.sync_all().map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?;
            drop(file);

            std::fs::rename(path, &target).map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?;

            println!("{} -> {}", path.display(), target.display());
            println!("  removed the 512-byte footer, which a .rmd would expose to the guest");
            println!(
                "  guest-visible size unchanged: {}",
                size::humanize(guest_before)
            );
            Ok(0)
        }

        // .rmd -> .vhd: hides the final sector. Refuse unless forced.
        //
        // A .rmd is presented whole, a .vhd as file minus 512, so this
        // conversion takes 512 bytes of real disk away from the guest. On a
        // partitioned image that final sector is where GPT keeps its backup
        // header, so the damage is not hypothetical.
        Presentation::Fixed => {
            if !args.force {
                return Err(Error::Usage(format!(
                    "converting {} to .vhd would hide its final 512 bytes from the guest.\n\
                     A .rmd is presented whole; a .vhd has its last sector treated as a \
                     footer and excluded. On a partitioned image that sector holds the GPT \
                     backup header.\n\
                     Pass --force to rename anyway, or use `iodd convert` to build a proper \
                     fixed VHD that preserves every byte.",
                    path.display()
                )));
            }

            std::fs::rename(path, &target).map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?;

            println!("{} -> {}", path.display(), target.display());
            eprintln!(
                "iodd: warning: the final 512 bytes are now hidden from the guest; \
                 the visible disk shrank from {} to {}",
                size::humanize(size),
                size::humanize(size.saturating_sub(512))
            );
            Ok(0)
        }
    }
}

/// Replace the extension, preserving the rest of the name.
///
/// IODD filenames routinely contain dots and mode tags (`&D`, `&DW`), so this
/// is worth a test even though it is one line.
fn with_extension(path: &Path, ext: &str) -> PathBuf {
    let mut out = path.to_path_buf();
    out.set_extension(ext);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_swap_preserves_dotted_stems_and_mode_tags() {
        assert_eq!(
            with_extension(Path::new("/a/Win11 23H2.v2.vhd"), "rmd"),
            PathBuf::from("/a/Win11 23H2.v2.rmd")
        );
        assert_eq!(
            with_extension(Path::new("/a/disk&DW.rmd"), "vhd"),
            PathBuf::from("/a/disk&DW.vhd")
        );
    }
}

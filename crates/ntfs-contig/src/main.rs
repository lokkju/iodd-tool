//! `ntfs-contig` — a general-purpose contiguity and allocation checker.
//!
//! The engine behind `iodd`, exposed on its own for anyone who needs a file
//! that is fully allocated and occupies one extent: VM images, swap files,
//! database preallocation, and hardware that streams sectors off an extent map.

use clap::{Parser, Subcommand};
use ntfs_contig::{Error, FileFacts};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "ntfs-contig",
    version,
    about = "Check and allocate contiguous, fully-allocated files"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report a file's extent map, allocation, and contiguity
    Verify {
        /// Files to inspect
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Machine-readable output
        #[arg(long)]
        json: bool,

        /// Report only; never exit non-zero for fragmentation or sparseness
        #[arg(long)]
        report_only: bool,
    },

    /// Copy a file into a contiguous, fully-allocated target
    ///
    /// Byte-for-byte: no knowledge of disk image formats, so no chance of
    /// misreading one. Holes in the source are written out by default,
    /// because a reservation that was never written holds stale clusters to
    /// anything reading raw sectors.
    Copy {
        /// Source file
        source: PathBuf,

        /// Target path
        dest: PathBuf,

        /// Overwrite an existing target
        #[arg(long)]
        force: bool,

        /// Skip the source's holes instead of writing them out
        ///
        /// Much faster on a mostly-empty image, and unsafe when the source's
        /// holes carry meaning: the target keeps whatever was on those
        /// clusters rather than zeros.
        #[arg(long)]
        sparse: bool,

        /// On failure, leave the partial target in place
        #[arg(long)]
        keep_on_fail: bool,
    },

    /// Re-place an existing file so it occupies one extent, keeping its name
    ///
    /// Copies through a temporary beside it and renames over the original.
    /// Not a defragmenter: it cannot move other files, so it needs a
    /// contiguous free run at least as large as the file.
    Fix {
        /// Files to re-place
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Skip holes instead of writing them out
        #[arg(long)]
        sparse: bool,

        /// On failure, leave the partial target in place
        #[arg(long)]
        keep_on_fail: bool,
    },

    /// Report NTFS volume state, including the dirty flag that stops a volume
    /// mounting
    ///
    /// Reads the device directly, because a volume that will not mount has no
    /// mounted view left to ask. Needs read access to the block device, which
    /// usually means root or the `disk` group.
    Volume {
        /// Block device, image file, or any path on a mounted NTFS volume
        #[arg(required = true)]
        targets: Vec<PathBuf>,

        /// Machine-readable output
        #[arg(long)]
        json: bool,

        /// Report only; never exit non-zero for a dirty volume
        #[arg(long)]
        report_only: bool,
    },
}

fn main() -> std::process::ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(err) => {
            let code = match err.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => 1,
            };
            let _ = err.print();
            return std::process::ExitCode::from(code);
        }
    };

    match run(cli) {
        Ok(code) => std::process::ExitCode::from(code),
        Err(err) => {
            eprintln!("ntfs-contig: {err}");
            std::process::ExitCode::from(exit_code(&err))
        }
    }
}

/// Exit codes match `iodd`'s for the properties the two share, so scripts can
/// treat them interchangeably.
fn exit_code(err: &Error) -> u8 {
    match err {
        Error::Io { .. } => 1,
        Error::Allocation { .. } => 3,
        Error::Fragmented { .. } => 4,
        Error::Sparse { .. } | Error::Uninitialized { .. } => 5,
        Error::Compressed { .. } => 6,
        // Both mean "the question could not be answered", not "the answer is no".
        Error::Unverifiable { .. } | Error::VolumeUnreadable { .. } => 8,
    }
}

fn run(cli: Cli) -> Result<u8, Error> {
    match cli.command {
        Command::Verify {
            paths,
            json,
            report_only,
        } => verify(&paths, json, report_only),
        Command::Volume {
            targets,
            json,
            report_only,
        } => volume(&targets, json, report_only),
        Command::Copy {
            source,
            dest,
            force,
            sparse,
            keep_on_fail,
        } => {
            let opts = ntfs_contig::copy::Options {
                force,
                sparse,
                keep_on_fail,
            };
            let mut bar = progress_for(&format!("copying {}", dest.display()));
            let r = ntfs_contig::copy::copy(&source, &dest, &opts, bar.as_mut())?;
            report_copy(&dest, &r);
            Ok(0)
        }
        Command::Fix {
            paths,
            sparse,
            keep_on_fail,
        } => {
            let opts = ntfs_contig::copy::Options {
                force: false,
                sparse,
                keep_on_fail,
            };
            for path in &paths {
                let mut bar = progress_for(&format!("re-placing {}", path.display()));
                let r = ntfs_contig::copy::fix(path, &opts, bar.as_mut())?;
                if r.written == 0 && r.holes_skipped == 0 {
                    println!("{}: already contiguous, left alone", path.display());
                } else {
                    report_copy(path, &r);
                }
            }
            Ok(0)
        }
    }
}

/// A progress bar when attached to a terminal, and nothing otherwise, so
/// piped output stays clean.
fn progress_for(label: &str) -> Box<dyn ntfs_contig::alloc::Progress> {
    ntfs_contig::alloc::TtyProgress::new(label.to_owned()).map_or_else(
        || Box::new(ntfs_contig::alloc::NoProgress) as Box<dyn ntfs_contig::alloc::Progress>,
        |p| Box::new(p) as Box<dyn ntfs_contig::alloc::Progress>,
    )
}

fn report_copy(path: &std::path::Path, r: &ntfs_contig::copy::Report) {
    println!("{}", path.display());
    println!("  bytes         {}", r.bytes);
    println!("  written       {}", r.written);
    if r.holes_skipped > 0 {
        println!("  holes skipped {}", r.holes_skipped);
        println!(
            "  note: those regions were reserved but never written, so they hold\n        \
             whatever was previously on those clusters, not zeros."
        );
    }
    println!("  contiguous    yes (1 extent)");
}

/// A dirty volume exits 4: it is not a warning about something that might go
/// wrong, it is the volume declining to mount next time it is attached.
fn volume(targets: &[PathBuf], json: bool, report_only: bool) -> Result<u8, Error> {
    use ntfs_contig::volume as vol;

    let mut worst: u8 = 0;
    let mut records = Vec::new();

    for target in targets {
        let device = vol::resolve(target)?;
        let info = vol::read(&device)?;

        if json {
            records.push(format!(
                r#"{{"target":{},"device":{},"label":{},"version":"{}.{}","flags":{},"flag_names":[{}],"dirty":{}}}"#,
                serde_json::Value::String(target.display().to_string()),
                serde_json::Value::String(device.display().to_string()),
                info.label.as_ref().map_or_else(
                    || "null".to_owned(),
                    |l| serde_json::Value::String(l.clone()).to_string()
                ),
                info.major,
                info.minor,
                info.flags,
                info.flag_names()
                    .iter()
                    .map(|n| format!("\"{n}\""))
                    .collect::<Vec<_>>()
                    .join(","),
                info.is_dirty(),
            ));
        } else {
            println!("{}", target.display());
            if device != *target {
                println!("  device        {}", device.display());
            }
            println!(
                "  label         {}",
                info.label.as_deref().unwrap_or("(none)")
            );
            println!("  ntfs version  {}.{}", info.major, info.minor);
            let names = info.flag_names();
            println!(
                "  flags         0x{:04x}{}",
                info.flags,
                if names.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", names.join(", "))
                }
            );
            if info.is_dirty() {
                println!(
                    "  DIRTY: this volume will not mount until the flag is cleared.\n         \
                     Clear it with `ntfsfix -d {}`, or chkdsk on Windows.",
                    device.display()
                );
            }
        }

        if !report_only && info.is_dirty() {
            worst = worst.max(4);
        }
    }

    if json {
        println!("[{}]", records.join(","));
    }
    Ok(worst)
}

fn verify(paths: &[PathBuf], json: bool, report_only: bool) -> Result<u8, Error> {
    let mut worst: u8 = 0;
    let mut records = Vec::new();

    for path in paths {
        let facts = FileFacts::open(path)?;

        if json {
            records.push(render_json(path, &facts));
        } else {
            print_report(path, &facts);
        }

        if !report_only {
            // Allocation before contiguity: a sparse file is often fragmented
            // too, and "not allocated" is the more actionable of the two.
            if facts.is_sparse() || facts.map.any_unwritten {
                worst = worst.max(5);
            } else if facts.size > 0 && !facts.is_contiguous() {
                worst = worst.max(4);
            }
        }
    }

    if json {
        println!("[{}]", records.join(","));
    }
    Ok(worst)
}

fn print_report(path: &std::path::Path, f: &FileFacts) {
    println!("{}", path.display());
    println!("  size          {}", f.size);
    println!("  allocated     {}", f.allocated);
    println!("  sparse        {}", f.is_sparse());
    println!("  raw records   {}", f.map.raw_records);
    println!(
        "  merged runs   {}   contiguous={}",
        f.map.runs.len(),
        f.is_contiguous()
    );
    println!("  any unwritten {}", f.map.any_unwritten);
    if let Some(magic) = f.fs_magic {
        let name = match magic {
            ntfs_contig::fsinfo::NTFS3_MAGIC => " (ntfs3)",
            ntfs_contig::fsinfo::NTFS_LEGACY_MAGIC => " (ntfs, legacy driver)",
            _ => "",
        };
        println!("  filesystem    0x{magic:x}{name}");
    }
    if f.compressed == Some(true) {
        println!("  compressed    yes");
    }
    if !f.is_contiguous() {
        for (i, run) in f.map.runs.iter().enumerate() {
            println!(
                "    run {i}: logical={} physical={} length={}",
                run.logical, run.physical, run.length
            );
        }
    }
}

fn render_json(path: &std::path::Path, f: &FileFacts) -> String {
    let runs: Vec<String> = f
        .map
        .runs
        .iter()
        .map(|r| {
            format!(
                r#"{{"logical":{},"physical":{},"length":{}}}"#,
                r.logical, r.physical, r.length
            )
        })
        .collect();
    format!(
        r#"{{"path":{},"size":{},"allocated":{},"sparse":{},"raw_records":{},"merged_runs":{},"contiguous":{},"any_unwritten":{},"runs":[{}]}}"#,
        serde_json::Value::String(path.display().to_string()),
        f.size,
        f.allocated,
        f.is_sparse(),
        f.map.raw_records,
        f.map.runs.len(),
        f.is_contiguous(),
        f.map.any_unwritten,
        runs.join(",")
    )
}

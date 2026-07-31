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
    }
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

//! CLI surface. Mirrors the synopsis in `README.md`.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "iodd",
    version,
    about = "Create, convert, and verify contiguous fixed-size VHD/RMD virtual disks for IODD devices",
    long_about = None,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a blank fixed VHD/RMD of the given size
    Create(CreateArgs),
    /// Write an existing image into a contiguous fixed VHD/RMD
    Convert(ConvertArgs),
    /// Inspect a single existing file
    Verify(VerifyArgs),
    /// Audit every IODD-mountable file under ROOT (read-only)
    Doctor(DoctorArgs),
    /// Rename between .vhd and .rmd without rewriting bytes
    Retype(RetypeArgs),
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Virtual size, e.g. 20G, 20GiB, 512M, or a raw byte count
    #[arg(long, value_name = "SIZE")]
    pub size: String,

    /// Output path
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,

    /// Default the extension to .rmd (removable) instead of .vhd
    #[arg(long)]
    pub removable: bool,

    /// Overwrite an existing target
    #[arg(long)]
    pub force: bool,

    /// 4-byte creator tag written to the footer
    #[arg(long, value_name = "TAG", default_value = "iodd")]
    pub creator: String,

    /// Write zeros across the whole file before finalizing.
    ///
    /// Off by default, matching VhdTool.exe, which allocates with
    /// SetFileValidData and never writes the data region. Without this the
    /// file holds whatever was previously on those clusters: the filesystem
    /// reports zeros, but anything reading the raw device — the IODD included
    /// — sees the old bytes.
    #[arg(long)]
    pub zero: bool,

    /// On failure, leave the partial file in place instead of removing it
    #[arg(long)]
    pub keep_on_fail: bool,
}

#[derive(Debug, Args)]
pub struct ConvertArgs {
    /// Source image
    #[arg(long, value_name = "PATH")]
    pub source: PathBuf,

    /// Output path
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,

    /// Default the extension to .rmd (removable) instead of .vhd
    #[arg(long)]
    pub removable: bool,

    /// Overwrite an existing target
    #[arg(long)]
    pub force: bool,

    /// Enlarge the target beyond the source; must be >= the source virtual size
    #[arg(long, value_name = "SIZE")]
    pub size: Option<String>,

    /// 4-byte creator tag written to the footer
    #[arg(long, value_name = "TAG", default_value = "iodd")]
    pub creator: String,

    /// Zero the region the source does not cover before finalizing.
    ///
    /// Off by default, matching create and VhdTool.exe. Without it, any space
    /// beyond the source's virtual size holds whatever was previously on those
    /// clusters.
    #[arg(long)]
    pub zero: bool,

    /// On failure, leave the partial file in place instead of removing it
    #[arg(long)]
    pub keep_on_fail: bool,
}

#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// File to inspect
    pub path: PathBuf,

    /// Promote footer and geometry warnings to a non-zero exit.
    ///
    /// Note: fragmentation and sparseness always fail, with or without this
    /// flag. They are the two properties the device actually enforces, so
    /// making them conditional would be misleading. See design decision D3.
    #[arg(long)]
    pub strict: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Mounted IODD volume, or a subtree of one
    pub root: PathBuf,

    /// Accepted for compatibility; recursion is the default
    #[arg(long, hide = true)]
    pub recursive: bool,

    /// Scan only the top level
    #[arg(long)]
    pub no_recursive: bool,

    /// Restrict to a comma-separated set of extensions
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    pub r#type: Vec<String>,

    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Table)]
    pub format: Format,

    /// Promote warnings to a non-zero aggregate exit code
    #[arg(long)]
    pub strict: bool,

    /// Fragment ceiling for ISO images
    #[arg(long, value_name = "N", default_value_t = 24)]
    pub iso_max_fragments: u32,
}

#[derive(Debug, Args)]
pub struct RetypeArgs {
    /// File to rename
    pub path: PathBuf,

    /// Target presentation
    #[arg(long, value_enum)]
    pub to: Presentation,

    /// Allow a conversion that hides bytes from the guest.
    ///
    /// Only needed for .rmd to .vhd, which takes the final 512 bytes out of the
    /// guest-visible disk.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Table,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Presentation {
    /// .vhd — mounted as a USB fixed drive
    Fixed,
    /// .rmd — mounted as a USB removable drive
    Removable,
}

impl DoctorArgs {
    /// Recursion is the default; `--recursive` is accepted as a no-op so the
    /// documented synopsis keeps working. See the plan, OPEN-1.
    pub fn is_recursive(&self) -> bool {
        !self.no_recursive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn doctor_recurses_by_default() {
        let args = DoctorArgs {
            root: PathBuf::from("/x"),
            recursive: false,
            no_recursive: false,
            r#type: vec![],
            format: Format::Table,
            strict: false,
            iso_max_fragments: 24,
        };
        assert!(args.is_recursive());
    }

    #[test]
    fn doctor_no_recursive_opts_out() {
        let args = DoctorArgs {
            root: PathBuf::from("/x"),
            recursive: false,
            no_recursive: true,
            r#type: vec![],
            format: Format::Table,
            strict: false,
            iso_max_fragments: 24,
        };
        assert!(!args.is_recursive());
    }

    #[test]
    fn legacy_recursive_flag_is_accepted() {
        let cli = Cli::try_parse_from(["iodd", "doctor", "/mnt", "--recursive"]);
        assert!(cli.is_ok(), "--recursive must remain accepted");
    }

    #[test]
    fn type_list_splits_on_commas() {
        let cli = Cli::try_parse_from(["iodd", "doctor", "/mnt", "--type", "vhd,rmd,iso"])
            .expect("parse");
        match cli.command {
            Command::Doctor(d) => assert_eq!(d.r#type, vec!["vhd", "rmd", "iso"]),
            _ => panic!("expected doctor"),
        }
    }
}

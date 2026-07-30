//! Entry point. Does nothing but dispatch and map errors to exit codes.

use clap::Parser;
use iodd::cli::{Cli, Command};
use iodd::error::Error;

fn main() -> std::process::ExitCode {
    // Parse by hand rather than via `Cli::parse()` so usage errors exit 1.
    // clap's default is 2, which SPEC.md reserves for "target exists".
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let code = match err.kind() {
                // Only an explicit --help or --version is a success. Help
                // printed because no subcommand was given goes to stderr and
                // is a usage error, which SPEC.md puts at exit 1.
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => 1,
            };
            // print() writes help/version to stdout and errors to stderr.
            let _ = err.print();
            return std::process::ExitCode::from(code);
        }
    };

    match run(cli) {
        Ok(code) => std::process::ExitCode::from(code),
        Err(err) => {
            eprintln!("iodd: {err}");
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            std::process::ExitCode::from(err.exit_code() as u8)
        }
    }
}

/// Returns an exit code on success, because `doctor` can succeed at its job
/// while still reporting that files will not mount.
fn run(cli: Cli) -> Result<u8, Error> {
    match cli.command {
        Command::Create(args) => iodd::cmd::create::run(&args),
        Command::Convert(args) => iodd::cmd::convert::run(&args),
        Command::Verify(args) => iodd::cmd::verify::run(&args),
        Command::Doctor(args) => iodd::cmd::doctor::run(&args),
        Command::Retype(args) => iodd::cmd::retype::run(&args),
    }
}

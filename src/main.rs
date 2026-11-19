#![forbid(unsafe_code)]

use std::io::Write;
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches};

fn main() -> miette::Result<ExitCode> {
    let matches = pixi_oci::cli::Cli::command().get_matches();
    let cli = pixi_oci::cli::Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());
    let output = pixi_oci::run(cli, &matches)?;
    let mut stdout = std::io::stdout().lock();
    // A closed pipe (`pixi oci plan | head`) is not an error worth reporting.
    let _ = stdout.write_all(output.stdout.as_bytes());
    let _ = stdout.flush();
    Ok(if output.success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

use std::io::Write;

use clap::Parser;

fn main() -> miette::Result<()> {
    let output = pixi_oci::run(pixi_oci::cli::Cli::parse())?;
    let mut stdout = std::io::stdout().lock();
    // A closed pipe (`pixi oci plan | head`) is not an error worth reporting.
    let _ = stdout.write_all(output.as_bytes());
    let _ = stdout.flush();
    Ok(())
}

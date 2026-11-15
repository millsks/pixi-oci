//! pixi-oci builds OCI container images straight from `pixi.lock`.
//!
//! This library is the binary's insides, exposed so the tests can reach them. It is not a designed API and makes no
//! stability promise.

pub mod cli;
pub mod lock;
pub mod plan;
pub mod platform;
pub mod report;

use cli::{Cli, Command, Format, PlanArgs};

/// Run a parsed command line, returning what to print.
pub fn run(cli: Cli) -> miette::Result<String> {
    match cli.command {
        Command::Plan(args) => run_plan(args),
    }
}

fn run_plan(args: PlanArgs) -> miette::Result<String> {
    let selection = args.selection;
    let platform = selection
        .platform
        .unwrap_or_else(|| platform::default_conda_platform().to_string());
    let lock = lock::load(&selection.lockfile)?;
    let locked = lock::select(&lock, &selection.environment, &platform)?;
    let plan = plan::plan(locked, args.max_layers)?;
    Ok(match args.format {
        Format::Text => report::text(&plan),
        Format::Json => report::json(&plan),
    })
}

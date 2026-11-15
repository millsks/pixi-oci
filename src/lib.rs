//! pixi-oci builds OCI container images straight from `pixi.lock`.
//!
//! This library is the binary's insides, exposed so the tests can reach them. It is not a designed API and makes no
//! stability promise.

#![forbid(unsafe_code)]

pub mod build;
pub mod cli;
pub mod fetch;
pub mod image;
#[cfg(test)]
pub mod install;
pub mod layer;
pub mod lock;
#[cfg(all(test, unix))]
mod parity;
pub mod plan;
pub mod platform;
pub mod report;
pub mod stream;
pub mod target;
#[cfg(test)]
mod testing;

use cli::{BuildArgs, Cli, Command, Format, PlanArgs};

/// Run a parsed command line, returning what to print.
pub fn run(cli: Cli) -> miette::Result<String> {
    match cli.command {
        Command::Plan(args) => run_plan(args),
        Command::Build(args) => run_build(args),
    }
}

/// Why the command line was refused before any work began.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum UsageError {
    #[error("`{value}` is not an image name and tag")]
    #[diagnostic(help("use a lowercase name, optionally with a registry and a tag: quay.io/team/app:1.0"))]
    Reference { value: String },
}

fn run_build(args: BuildArgs) -> miette::Result<String> {
    let selection = args.selection;
    let platform = selection
        .platform
        .unwrap_or_else(|| platform::default_conda_platform().to_string());
    let tag = args.tag.unwrap_or_else(|| selection.environment.clone());
    let reference = image::Reference::parse(&tag).ok_or(UsageError::Reference { value: tag })?;
    let lock = lock::load(&selection.lockfile)?;
    let locked = lock::select(&lock, &selection.environment, &platform)?;
    let plan = plan::plan(locked, args.layers.layers, args.layers.max_layers)?;
    let options = build::BuildOptions {
        prefix: args.prefix,
        output: args.output,
        force: args.force,
        reference,
        env: args.env,
        entrypoint: args.entrypoint.map(|command| command.0),
        cmd: args.cmd.map(|command| command.0),
        workdir: args.workdir,
        source_date_epoch: build::source_date_epoch(),
        max_memory: args.max_memory,
    };
    let fetcher = fetch::Fetcher::new(args.cache_dir.unwrap_or_else(fetch::Fetcher::default_root));
    let written = build::build(&plan, &options, &fetcher, &mut std::io::stderr())?;
    Ok(format!(
        "Wrote {} to {}\n  manifest {}\n  {}, {} for {}\n",
        written.reference,
        options.output.display(),
        written.manifest.digest,
        report::plural(plan.layers.len(), "layer"),
        report::size_text(Some(written.layers_size)),
        plan.oci_platform,
    ))
}

fn run_plan(args: PlanArgs) -> miette::Result<String> {
    let selection = args.selection;
    let platform = selection
        .platform
        .unwrap_or_else(|| platform::default_conda_platform().to_string());
    let lock = lock::load(&selection.lockfile)?;
    let locked = lock::select(&lock, &selection.environment, &platform)?;
    let plan = plan::plan(locked, args.layers.layers, args.layers.max_layers)?;
    Ok(match args.format {
        Format::Text => report::text(&plan, args.packages),
        Format::Json => report::json(&plan),
    })
}

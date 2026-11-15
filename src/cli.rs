//! The command line. pixi runs `pixi-oci` for `pixi oci`, passing everything after `oci`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::plan::DEFAULT_MAX_LAYERS;

/// Build OCI container images straight from pixi.lock.
#[derive(Debug, Parser)]
#[command(name = "pixi-oci", bin_name = "pixi oci", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show the layers an image of an environment would be built from, without downloading anything.
    Plan(PlanArgs),
}

/// Which environment of which lockfile, and for which platform.
#[derive(Debug, Args)]
pub struct Selection {
    /// The lockfile to read.
    #[arg(long, default_value = "pixi.lock", value_name = "PATH")]
    pub lockfile: PathBuf,

    /// The environment to put in the image.
    #[arg(short, long, default_value = "default")]
    pub environment: String,

    /// The conda platform to build for; a linux-* platform. Defaults to Linux on the host's architecture.
    #[arg(short, long)]
    pub platform: Option<String>,
}

#[derive(Debug, Args)]
pub struct PlanArgs {
    #[command(flatten)]
    pub selection: Selection,

    /// The most layers to use. Past it, the packages fewest others depend on share the last conda layer.
    #[arg(long, default_value_t = DEFAULT_MAX_LAYERS, value_name = "N")]
    pub max_layers: usize,

    /// How to print the plan.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Text,
    Json,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }
}

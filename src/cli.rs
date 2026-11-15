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
    /// Build an image of an environment, as an OCI image layout directory or a `.tar` archive.
    Build(BuildArgs),
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

#[derive(Debug, Args)]
pub struct BuildArgs {
    #[command(flatten)]
    pub selection: Selection,

    /// Where to write the image: a `.tar` archive for `docker load`, `podman load` and `skopeo copy oci-archive:`, or
    /// any other path for an OCI image layout directory.
    #[arg(short, long, default_value = "image.tar", value_name = "PATH")]
    pub output: PathBuf,

    /// Replace the output if it exists.
    #[arg(long)]
    pub force: bool,

    /// The image's name and tag. Defaults to the environment's name, tagged `latest`.
    #[arg(short, long, value_name = "NAME[:TAG]")]
    pub tag: Option<String>,

    /// Where the environment lives in the image.
    #[arg(long, default_value = "/opt/env", value_name = "PATH")]
    pub prefix: String,

    /// Set an environment variable in the image; repeat for more. `PATH` and `CONDA_PREFIX` are set already.
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = crate::build::parse_env)]
    pub env: Vec<String>,

    /// The image's entrypoint, as a JSON array (`["python", "-m", "http.server"]`) or words.
    #[arg(long, value_name = "COMMAND", value_parser = CommandLine::parse)]
    pub entrypoint: Option<CommandLine>,

    /// The image's default arguments, as a JSON array or words.
    #[arg(long, value_name = "ARGS", value_parser = CommandLine::parse)]
    pub cmd: Option<CommandLine>,

    /// The working directory processes in the image start in.
    #[arg(long, value_name = "PATH")]
    pub workdir: Option<String>,

    /// The most layers to use, as for `plan`.
    #[arg(long, default_value_t = DEFAULT_MAX_LAYERS, value_name = "N")]
    pub max_layers: usize,

    /// Where downloaded and extracted packages are kept. Defaults to `PIXI_OCI_CACHE_DIR`, else the platform's cache
    /// directory.
    #[arg(long, value_name = "PATH")]
    pub cache_dir: Option<PathBuf>,
}

/// A command line given as one argument. Its own type so clap does not read `Option<Vec<_>>` as a repeated option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine(pub Vec<String>);

impl CommandLine {
    fn parse(value: &str) -> Result<Self, String> {
        crate::build::parse_command(value).map(Self)
    }
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

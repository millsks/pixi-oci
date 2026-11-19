//! The command line. pixi runs `pixi-oci` for `pixi oci`, passing everything after `oci`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::plan::{DEFAULT_MAX_LAYERS, Layering};

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
    Build(Box<BuildArgs>),
    /// Check an image against the OCI image spec: every blob, digest, size and diff_id. Nothing is run.
    Validate(ValidateArgs),
    /// Show an image's platform, settings and layers, with the packages each layer holds.
    Inspect(InspectArgs),
    /// Check that an image will run, without running it: architecture, loader, libraries, shebangs, placeholders
    /// and the entrypoint. Runs `validate` first.
    Check(CheckArgs),
    /// Send an image to a registry: only the blobs it lacks, then the manifest under the tag. Runs `validate` first.
    Push(PushArgs),
    /// Download an image from a registry into a `.tar` archive or an OCI image layout directory.
    Pull(PullArgs),
    /// Print the digest a registry serves for a tag, to pin a base with; downloads no layers.
    Digest(DigestArgs),
}

/// Whether the command line gave the option with this id, rather than its default: what was given wins over a
/// config file's key.
pub fn given(matches: &clap::ArgMatches, id: &str) -> bool {
    matches.value_source(id) == Some(clap::parser::ValueSource::CommandLine)
}

/// An image a config file defines, and the file to read.
#[derive(Debug, Clone, Args)]
pub struct ImageArgs {
    /// An image a config file defines: `[tool.pixi-oci.images.NAME]` in pixi.toml or pyproject.toml, or
    /// `[images.NAME]` in pixi-oci.toml. Without one, `build` builds every image the file defines.
    #[arg(value_name = "IMAGE")]
    pub image: Option<String>,

    /// The config file to read, and no other; by default the manifest beside the lockfile, else pixi-oci.toml there.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

/// Which environment of which lockfile, and for which platform.
#[derive(Debug, Clone, Args)]
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

/// How the environment is split into layers.
#[derive(Debug, Clone, Args)]
pub struct LayerArgs {
    /// How to split the environment into layers: by default the whole environment in one, on top of the base's.
    #[arg(long, value_enum, default_value_t = Layering::Single)]
    pub layers: Layering,

    /// The most layers to use. Past it, the layers of the packages fewest others depend on are merged into the last
    /// conda layer.
    #[arg(long, default_value_t = DEFAULT_MAX_LAYERS, value_name = "N")]
    pub max_layers: usize,
}

#[derive(Debug, Args)]
pub struct PlanArgs {
    #[command(flatten)]
    pub image: ImageArgs,

    #[command(flatten)]
    pub selection: Selection,

    #[command(flatten)]
    pub layers: LayerArgs,

    /// List the packages of every layer that holds several.
    #[arg(long)]
    pub packages: bool,

    /// How to print the plan.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct DigestArgs {
    /// The image: [registry/]repository[:tag], such as debian:bookworm-slim.
    #[arg(value_name = "REFERENCE")]
    pub reference: String,

    /// Print the digest of this platform's manifest (linux-64, linux-aarch64), rather than the tag's, which may be an
    /// index of several.
    #[arg(short, long)]
    pub platform: Option<String>,

    /// Print the whole reference with the digest, ready for `build --base`.
    #[arg(long)]
    pub pinned: bool,

    /// Where downloaded manifests are kept, as for `build`.
    #[arg(long, value_name = "PATH")]
    pub cache_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct PullArgs {
    /// The image: [registry/]repository[:tag][@digest], such as debian:bookworm-slim or ghcr.io/team/app:1.0.
    #[arg(value_name = "REFERENCE")]
    pub reference: String,

    /// Where to write it: a `.tar` archive, or any other path for an OCI image layout directory. Defaults to the
    /// repository's last part, as `<name>.tar`.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Replace the output if it exists.
    #[arg(long)]
    pub force: bool,

    /// The platform to pull from an image of several: a conda platform (linux-64, linux-aarch64). Defaults to Linux
    /// on the host's architecture.
    #[arg(short, long)]
    pub platform: Option<String>,

    /// Where downloaded blobs are kept, as for `build`.
    #[arg(long, value_name = "PATH")]
    pub cache_dir: Option<PathBuf>,

    /// How to print the result.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct PushArgs {
    /// The image: an OCI image layout directory or a `.tar` archive of one.
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

    /// Where to push it: [registry/]repository[:tag], such as ghcr.io/team/app:1.0; `latest` when no tag is given.
    /// Credentials are pixi's (`pixi auth login`), or PIXI_OCI_REGISTRY_USERNAME and PIXI_OCI_REGISTRY_PASSWORD.
    #[arg(value_name = "DESTINATION")]
    pub destination: String,

    /// The platform to push when the image holds several: an OCI platform (linux/arm64/v8) or a conda one
    /// (linux-aarch64).
    #[arg(short, long)]
    pub platform: Option<String>,

    /// How to print the result.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// The image: an OCI image layout directory, a `.tar` archive of one, or an image in a registry
    /// (ghcr.io/team/app:1.0, debian:bookworm-slim, or docker://REFERENCE).
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

    /// The platform to check when the image holds several: an OCI platform (linux/arm64/v8) or a conda one
    /// (linux-aarch64).
    #[arg(short, long)]
    pub platform: Option<String>,

    /// How to print the result.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// The image: an OCI image layout directory, a `.tar` archive of one, or an image in a registry
    /// (ghcr.io/team/app:1.0, debian:bookworm-slim, or docker://REFERENCE).
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

    /// The platform to show when the image holds several: an OCI platform (linux/arm64/v8) or a conda one
    /// (linux-aarch64).
    #[arg(short, long)]
    pub platform: Option<String>,

    /// List the packages of every layer that holds several.
    #[arg(long)]
    pub packages: bool,

    /// Print a stored document exactly as it is, instead of the summary.
    #[arg(long, value_enum, conflicts_with_all = ["format", "packages"])]
    pub raw: Option<crate::inspect::Raw>,

    /// How to print the summary.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// The image: an OCI image layout directory, a `.tar` archive of one, or an image in a registry
    /// (ghcr.io/team/app:1.0, debian:bookworm-slim, or docker://REFERENCE).
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

    /// From a registry, the platform to validate when the image has several: a conda platform (linux-aarch64) or an
    /// OCI one (linux/arm64/v8). Defaults to Linux on the host's architecture. A file is validated whole.
    #[arg(short, long)]
    pub platform: Option<String>,

    /// How to print the result.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
}

#[derive(Debug, Clone, Args)]
pub struct BuildArgs {
    #[command(flatten)]
    pub image: ImageArgs,

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
    #[arg(long, value_name = "COMMAND", value_parser = CommandLine::parse, allow_hyphen_values = true)]
    pub entrypoint: Option<CommandLine>,

    /// The image's default arguments, as a JSON array or words; they may start with `-` (`--cmd "-m app"`).
    #[arg(long, value_name = "ARGS", value_parser = CommandLine::parse, allow_hyphen_values = true)]
    pub cmd: Option<CommandLine>,

    /// The working directory processes in the image start in.
    #[arg(long, value_name = "PATH")]
    pub workdir: Option<String>,

    /// A pixi task to start the image with, from the workspace's manifest beside the lockfile: its command becomes the
    /// entrypoint, its `cwd` (from `/`) the working directory, and its `env` is added. A command with shell syntax
    /// runs through /bin/sh -c. --entrypoint, --workdir and --env win over it.
    #[arg(long, value_name = "NAME")]
    pub task: Option<String>,

    /// A file or directory to put in the image, as a Dockerfile's COPY: SRC:DEST, such as src:/app; repeat for
    /// more. Every --add goes into one application layer above the environment. A directory leaves out what
    /// .gitignore, .dockerignore and .pixi-oci-ignore name, and .git and .pixi; a file named by itself is always
    /// added. Files are owned by root, 0755 when they start with #! or are ELF files, 0644 otherwise.
    #[arg(long = "add", value_name = "SRC:DEST", value_parser = crate::add::parse_add)]
    pub add: Vec<crate::add::Add>,

    /// A label in the image config, as a Dockerfile's LABEL; repeat for more. pixi-oci's own dev.pixi.* labels win.
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = crate::build::parse_key_value)]
    pub label: Vec<(String, String)>,

    /// An annotation on the image manifest, such as org.opencontainers.image.source=https://github.com/team/app;
    /// repeat for more.
    #[arg(long = "annotation", value_name = "KEY=VALUE", value_parser = crate::build::parse_key_value)]
    pub annotation: Vec<(String, String)>,

    /// The user processes run as, as a Dockerfile's USER: NAME, UID, NAME:GROUP or UID:GID.
    #[arg(long, value_name = "USER", value_parser = crate::build::parse_user)]
    pub user: Option<String>,

    /// A port the image listens on, as a Dockerfile's EXPOSE: PORT or PORT/PROTOCOL; repeat for more.
    #[arg(long, value_name = "PORT[/PROTOCOL]", value_parser = crate::build::parse_port)]
    pub expose: Vec<String>,

    /// The signal that stops the container, as a Dockerfile's STOPSIGNAL: SIGTERM, TERM or a number.
    #[arg(long, value_name = "SIGNAL", value_parser = crate::build::parse_signal)]
    pub stop_signal: Option<String>,

    #[command(flatten)]
    pub layers: LayerArgs,

    /// Roughly how much memory the build's buffers may use, such as 512M or 2G. Less means fewer layers written at
    /// once and large rewritten files waiting on disk; the image comes out the same.
    #[arg(long, default_value = "512M", value_name = "SIZE", value_parser = crate::build::parse_size)]
    pub max_memory: u64,

    /// Where downloaded package archives and base image blobs are kept, named by their sha256. Defaults to
    /// `PIXI_OCI_CACHE_DIR`, else the platform's cache directory.
    #[arg(long, value_name = "PATH")]
    pub cache_dir: Option<PathBuf>,

    /// An image to build on, pinned by digest, such as
    /// registry.access.redhat.com/ubi9/ubi-minimal@sha256:... or debian:bookworm-slim@sha256:...: its layers go
    /// under the environment's, and it provides glibc, /bin/sh and the rest of a Linux userland. Its layers count
    /// toward --max-layers.
    #[arg(long, value_name = "IMAGE@DIGEST")]
    pub base: Option<String>,
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

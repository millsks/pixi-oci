//! pixi-oci builds OCI container images straight from `pixi.lock`.
//!
//! This library is the binary's insides, exposed so the tests can reach them. It is not a designed API and makes no
//! stability promise.

#![forbid(unsafe_code)]

pub mod add;
pub mod build;
pub mod check;
pub mod cli;
pub mod digest;
pub mod fetch;
pub mod image;
pub mod inspect;
#[cfg(test)]
pub mod install;
pub mod layer;
pub mod lock;
#[cfg(all(test, unix))]
mod parity;
pub mod plan;
pub mod platform;
pub mod pull;
pub mod push;
pub mod read;
pub mod registry;
pub mod remote;
pub mod report;
pub mod sources;
pub mod stream;
pub mod target;
#[cfg(test)]
mod testing;
pub mod validate;
pub mod wheel;

use cli::{
    BuildArgs, CheckArgs, Cli, Command, DigestArgs, Format, InspectArgs, PlanArgs, PullArgs, PushArgs, ValidateArgs,
};

/// What a command prints, and whether it succeeded: a command can print a full report and still fail, as
/// `validate` does for an invalid image.
#[derive(Debug)]
pub struct Output {
    pub stdout: String,
    pub success: bool,
}

impl From<String> for Output {
    fn from(stdout: String) -> Self {
        Output { stdout, success: true }
    }
}

/// Run a parsed command line, returning what to print.
pub fn run(cli: Cli) -> miette::Result<Output> {
    match cli.command {
        Command::Plan(args) => run_plan(args).map(Output::from),
        Command::Build(args) => run_build(*args).map(Output::from),
        Command::Validate(args) => run_validate(args),
        Command::Inspect(args) => run_inspect(args).map(Output::from),
        Command::Check(args) => run_check(args),
        Command::Push(args) => run_push(args).map(Output::from),
        Command::Pull(args) => run_pull(args).map(Output::from),
        Command::Digest(args) => run_digest(args).map(Output::from),
    }
}

fn run_digest(args: DigestArgs) -> miette::Result<String> {
    let reference = registry::ImageRef::parse(&args.reference)?;
    let platform = match &args.platform {
        Some(name) => {
            Some(platform::oci_platform(name).ok_or_else(|| plan::PlanError::NotLinux { platform: name.clone() })?)
        }
        None => None,
    };
    let cache = args.cache_dir.unwrap_or_else(fetch::Fetcher::default_root);
    let registry = registry::Registry::new(cache);
    let digest = digest::digest(&args.reference, &reference, platform.as_ref(), args.pinned, &registry)?;
    Ok(digest + "\n")
}

fn run_pull(args: PullArgs) -> miette::Result<String> {
    let reference = registry::ImageRef::parse(&args.reference)?;
    let conda_platform = args
        .platform
        .unwrap_or_else(|| platform::default_conda_platform().to_string());
    let oci = platform::oci_platform(&conda_platform).ok_or_else(|| plan::PlanError::NotLinux {
        platform: conda_platform.clone(),
    })?;
    let output = args.output.unwrap_or_else(|| {
        let name = reference.repository.rsplit('/').next().unwrap_or("image");
        std::path::PathBuf::from(format!("{name}.tar"))
    });
    let cache = args.cache_dir.unwrap_or_else(fetch::Fetcher::default_root);
    let registry = registry::Registry::new(cache);
    let pulled = pull::pull(&reference, &oci, &output, args.force, &registry)?;
    Ok(match args.format {
        Format::Text => pulled.text(),
        Format::Json => pulled.json() + "\n",
    })
}

fn run_push(args: PushArgs) -> miette::Result<String> {
    let destination = registry::ImageRef::parse(&args.destination)?;
    let registry = registry::Registry::for_registry(fetch::Fetcher::default_root(), Some(&destination.registry));
    let pushed = push::push(
        &args.image,
        args.platform.as_deref(),
        &destination,
        &registry,
        &mut std::io::stderr(),
    )?;
    Ok(match args.format {
        Format::Text => pushed.text(),
        Format::Json => pushed.json() + "\n",
    })
}

/// The image as a path to read: the path given, or a registry image fetched into a temporary layout, with its layers
/// when `layers` is set, and the name to report it by.
fn open_image(
    image: &std::path::Path,
    platform: Option<&str>,
    layers: bool,
) -> miette::Result<(std::path::PathBuf, Option<remote::RemoteImage>)> {
    match remote::reference(image)? {
        None => Ok((image.to_path_buf(), None)),
        Some(reference) => {
            let registry = registry::Registry::new(fetch::Fetcher::default_root());
            let fetched = remote::fetch(&reference, platform, layers, &registry)?;
            Ok((fetched.layout.clone(), Some(fetched)))
        }
    }
}

fn run_check(args: CheckArgs) -> miette::Result<Output> {
    let (path, fetched) = open_image(&args.image, args.platform.as_deref(), true)?;
    // A fetched layout holds the one platform it was fetched for.
    let platform = if fetched.is_some() {
        None
    } else {
        args.platform.as_deref()
    };
    let mut report = check::check(&path, platform)?;
    if let Some(fetched) = &fetched {
        report.image = fetched.name.clone();
    }
    let stdout = match args.format {
        Format::Text => report.text(),
        Format::Json => report.json() + "\n",
    };
    Ok(Output {
        stdout,
        success: report.ok,
    })
}

fn run_inspect(args: InspectArgs) -> miette::Result<String> {
    let (path, fetched) = open_image(&args.image, args.platform.as_deref(), false)?;
    let platform = if fetched.is_some() {
        None
    } else {
        args.platform.as_deref()
    };
    if let Some(which) = args.raw {
        // From a registry, the index is the document it serves for the reference, not the layout's own.
        if let (Some(fetched), inspect::Raw::Index) = (&fetched, which) {
            return Ok(String::from_utf8_lossy(&fetched.served).into_owned());
        }
        return Ok(inspect::raw(&path, platform, which)?);
    }
    let mut inspection = inspect::inspect(&path, platform)?;
    if let Some(fetched) = &fetched {
        inspection.image = fetched.name.clone();
    }
    Ok(match args.format {
        Format::Text => inspection.text(args.packages),
        Format::Json => inspection.json() + "\n",
    })
}

fn run_validate(args: ValidateArgs) -> miette::Result<Output> {
    let (path, fetched) = open_image(&args.image, args.platform.as_deref(), true)?;
    let mut report = validate::validate(&path)?;
    if let Some(fetched) = &fetched {
        report.image = fetched.name.clone();
    }
    let stdout = match args.format {
        Format::Text => report.text(),
        Format::Json => report.json() + "\n",
    };
    Ok(Output {
        stdout,
        success: report.valid,
    })
}

/// Why the command line was refused before any work began.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum UsageError {
    #[error("`{value}` is not an image name and tag")]
    #[diagnostic(help("use a lowercase name, optionally with a registry and a tag: quay.io/team/app:1.0"))]
    Reference { value: String },

    #[error(
        "the base image {base} has {base_layers} layers, which leaves too few of --max-layers {max_layers} for the environment"
    )]
    #[diagnostic(help("raise --max-layers, or pick a base with fewer layers"))]
    TooManyBaseLayers {
        base: String,
        base_layers: usize,
        max_layers: usize,
    },
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
    let workspace = selection.lockfile.parent().map(std::path::Path::to_path_buf);
    // The application's files are collected before anything is downloaded, so a wrong path fails at once.
    let app = if args.add.is_empty() {
        None
    } else {
        let root = workspace.as_deref().map(|dir| {
            if dir.as_os_str().is_empty() {
                std::path::Path::new(".")
            } else {
                dir
            }
        });
        Some(add::collect(&args.add, root)?)
    };
    let app_layers = usize::from(app.is_some());
    let cache = args.cache_dir.unwrap_or_else(fetch::Fetcher::default_root);
    // The base first: it is pinned and small next to the environment, and its layers come out of the layer budget.
    let base = match &args.base {
        Some(value) => {
            let reference = registry::ImageRef::parse(value)?;
            let oci = platform::oci_platform(&platform).ok_or_else(|| plan::PlanError::NotLinux {
                platform: platform.clone(),
            })?;
            eprintln!("base {reference}");
            Some(registry::Registry::new(cache.clone()).pull(&reference, &oci)?)
        }
        None => None,
    };
    let base_layers = base.as_ref().map_or(0, |base| base.layers.len());
    let max_layers = args.layers.max_layers.saturating_sub(base_layers + app_layers);
    if base_layers > 0 && max_layers < 2 && args.layers.layers != plan::Layering::Single {
        return Err(UsageError::TooManyBaseLayers {
            base: args.base.unwrap_or_default(),
            base_layers,
            max_layers: args.layers.max_layers,
        }
        .into());
    }
    let plan = plan::plan(locked, args.layers.layers, max_layers)?;
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
        base,
        labels: args.label,
        annotations: args.annotation,
        user: args.user,
        expose: args.expose,
        stop_signal: args.stop_signal,
        app,
    };
    let fetcher = fetch::Fetcher::new(cache)
        .with_pixi_credentials()?
        .with_pixi_config(workspace.as_deref())?;
    let written = build::build(&plan, &options, &fetcher, &mut std::io::stderr())?;
    Ok(format!(
        "Wrote {} to {}\n  manifest {}\n  {}, {} for {}\n",
        written.reference,
        options.output.display(),
        written.manifest.digest,
        report::plural(plan.layers.len() + base_layers + app_layers, "layer"),
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

//! `check`: whether an image will run, by Linux's rules, without running anything.
//!
//! The image is validated first (`validate`); then its layers are applied in order, whiteouts included, into a map
//! of the file system that keeps facts, never contents: each file's mode, whether it is an ELF file and what its
//! dynamic section asks for, its shebang, and whether a conda build placeholder is left in it. The checks then look
//! up loaders, libraries, interpreters and the entrypoint the way Linux would, inside that map. Nothing touches the
//! host but a temporary file for an ELF file too large to hold in memory.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::path::Path;

use object::elf;
use object::read::elf::{Dyn, FileHeader, ProgramHeader};
use object::{Endianness, ReadRef};
use serde::Serialize;

use crate::inspect::{InspectError, Inspection, inspect};
use crate::read::{Image, blob_path};
use crate::report::plural;

/// An ELF file up to this size is parsed in memory; a larger one from a temporary file.
const IN_MEMORY: usize = 64 << 20;
/// Strings conda-build leaves where the install prefix goes, which installing must replace.
const PLACEHOLDERS: [&[u8]; 2] = [b"/opt/anaconda1anaconda2anaconda3", b"placehold_placehold_placehold"];
/// The `PATH` a container gets when its image sets none.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// How many symlinks a lookup follows before giving up, as Linux does.
const MAX_LINKS: usize = 40;
/// The libraries glibc provides, besides its loader (`ld-linux*`, `ld64.so*`).
const GLIBC: [&str; 12] = [
    "libc.so.6",
    "libm.so.6",
    "libdl.so.2",
    "libpthread.so.0",
    "librt.so.1",
    "libutil.so.1",
    "libresolv.so.2",
    "libnsl.so.1",
    "libcrypt.so.1",
    "libanl.so.1",
    "libmvec.so.1",
    "libBrokenLocale.so.1",
];

/// Why an image could not be checked at all.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum CheckError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Inspect(#[from] InspectError),

    #[error("cannot read layer {layer} of {image}: {source}")]
    Layer {
        image: String,
        layer: usize,
        #[source]
        source: io::Error,
    },
}

/// One thing wrong, or worth knowing: what kind, what, and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub kind: &'static str,
    pub message: String,
    /// Every affected path in the image, sorted.
    pub paths: Vec<String>,
    pub help: Option<String>,
}

/// How much was looked at.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub files: usize,
    pub elf: usize,
    pub scripts: usize,
}

/// What `check` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub image: String,
    pub platform: Option<String>,
    pub ok: bool,
    pub checked: Counts,
    pub problems: Vec<Finding>,
    pub notes: Vec<Finding>,
}

impl Report {
    /// For a person: a verdict, then each problem and note with a few of its paths.
    pub fn text(&self) -> String {
        let mut out = String::new();
        let looked = format!(
            "{} ({}, {})",
            plural(self.checked.files, "file"),
            plural(self.checked.elf, "ELF file"),
            plural(self.checked.scripts, "script"),
        );
        if self.ok {
            out.push_str(&format!("{}: no problems in {looked}\n", self.image));
        } else {
            out.push_str(&format!(
                "{}: {} in {looked}\n",
                self.image,
                plural(self.problems.len(), "problem")
            ));
        }
        for (label, findings) in [("", &self.problems), ("note: ", &self.notes)] {
            for finding in findings {
                out.push_str(&format!("  {label}{}: {}\n", finding.kind, finding.message));
                if !finding.paths.is_empty() {
                    let shown: Vec<&str> = finding.paths.iter().take(3).map(String::as_str).collect();
                    let more = finding.paths.len().saturating_sub(shown.len());
                    let tail = if more > 0 {
                        format!(", and {more} more")
                    } else {
                        String::new()
                    };
                    out.push_str(&format!("    {}{tail}\n", shown.join(", ")));
                }
                if let Some(help) = &finding.help {
                    out.push_str(&format!("    help: {help}\n"));
                }
            }
        }
        out
    }

    /// For a script.
    pub fn json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a report always serializes")
    }
}

/// Check the image at `path`, for `platform` when it holds several.
pub fn check(path: &Path, platform: Option<&str>) -> Result<Report, CheckError> {
    let name = path.display().to_string();
    let validation = crate::validate::validate(path).map_err(InspectError::from)?;
    if !validation.valid {
        let problems = validation
            .problems
            .into_iter()
            .map(|problem| Finding {
                kind: "spec",
                message: format!("{}: {}", problem.at, problem.message),
                paths: Vec::new(),
                help: None,
            })
            .collect();
        return Ok(Report {
            image: name,
            platform: None,
            ok: false,
            checked: Counts::default(),
            problems,
            notes: vec![Finding {
                kind: "skipped",
                message: "the image is not a valid OCI image, so its files were not checked".to_string(),
                paths: Vec::new(),
                help: Some(format!(
                    "`pixi oci validate {}` says the same, in more detail",
                    path.display()
                )),
            }],
        });
    }
    let inspection = inspect(path, platform)?;
    let image = Image::open(path).map_err(InspectError::from)?;
    let mut fs = Fs::default();
    for (i, layer) in inspection.layers.iter().enumerate() {
        let layer_err = |source| CheckError::Layer {
            image: name.clone(),
            layer: i + 1,
            source,
        };
        let blob = blob_path(&layer.digest).ok_or_else(|| layer_err(io::Error::other("no sha256 digest")))?;
        let reader = image.reader(&blob).map_err(layer_err)?;
        let reader: Box<dyn Read> = if layer.media_type.ends_with("gzip") {
            Box::new(flate2::read::MultiGzDecoder::new(reader))
        } else if layer.media_type.ends_with(".tar") {
            reader
        } else {
            return Err(layer_err(io::Error::other(format!(
                "pixi oci check reads gzip and uncompressed layers, not {}",
                layer.media_type
            ))));
        };
        fs.apply(reader, i).map_err(layer_err)?;
    }
    let context = Context::from(&inspection);
    let (problems, notes) = fs.findings(&context);
    Ok(Report {
        image: name,
        platform: inspection.platform,
        ok: problems.is_empty(),
        checked: fs.counts(),
        problems,
        notes,
    })
}

/// What the checks need to know about the image besides its files.
#[derive(Debug, Clone, Default)]
struct Context {
    /// `amd64`, `arm64`, ...
    architecture: String,
    path: Vec<String>,
    ld_library_path: Vec<String>,
    /// The program a container runs by default: the entrypoint's first word, or else cmd's.
    program: Option<String>,
    workdir: String,
}

impl From<&Inspection> for Context {
    fn from(inspection: &Inspection) -> Self {
        let env = |key: &str| {
            inspection
                .env
                .iter()
                .find_map(|pair| pair.strip_prefix(&format!("{key}=")).map(str::to_string))
        };
        let split = |value: String| value.split(':').filter(|s| !s.is_empty()).map(str::to_string).collect();
        Context {
            architecture: inspection
                .platform
                .as_deref()
                .and_then(|platform| platform.split('/').nth(1))
                .unwrap_or_default()
                .to_string(),
            path: split(env("PATH").unwrap_or_else(|| DEFAULT_PATH.to_string())),
            ld_library_path: env("LD_LIBRARY_PATH").map(split).unwrap_or_default(),
            program: inspection
                .entrypoint
                .as_ref()
                .and_then(|words| words.first())
                .or_else(|| inspection.cmd.as_ref().and_then(|words| words.first()))
                .cloned(),
            workdir: inspection.workdir.clone().unwrap_or_else(|| "/".to_string()),
        }
    }
}

/// What a dynamic ELF file asks of the system.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ElfFacts {
    machine: u16,
    class64: bool,
    little_endian: bool,
    interpreter: Option<String>,
    needed: Vec<String>,
    rpath: Vec<String>,
    runpath: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FileFacts {
    layer: usize,
    executable: bool,
    elf: Option<Result<ElfFacts, String>>,
    /// The interpreter and its first argument, for an executable that starts with `#!`.
    shebang: Option<Vec<String>>,
    placeholder: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Dir { layer: usize },
    File(Box<FileFacts>),
    Symlink { target: String, layer: usize },
}

impl Node {
    fn layer(&self) -> usize {
        match self {
            Node::Dir { layer } | Node::Symlink { layer, .. } => *layer,
            Node::File(facts) => facts.layer,
        }
    }
}

/// The image's file system after its layers: absolute path to node.
#[derive(Debug, Default)]
struct Fs {
    nodes: BTreeMap<String, Node>,
}

/// `/a/b` from a tar path or a link target relative to `/`: `.` and empty parts dropped, `..` taken back.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

fn join(dir: &str, name: &str) -> String {
    if name.starts_with('/') {
        normalize(name)
    } else {
        normalize(&format!("{dir}/{name}"))
    }
}

impl Fs {
    /// Remove `path` and everything under it that came from a layer below `layer`.
    fn remove_tree(&mut self, path: &str, below: usize) {
        if self.nodes.get(path).is_some_and(|node| node.layer() < below) {
            self.nodes.remove(path);
        }
        self.remove_children(path, below);
    }

    fn remove_children(&mut self, dir: &str, below: usize) {
        let prefix = if dir == "/" { "/".to_string() } else { format!("{dir}/") };
        let doomed: Vec<String> = self
            .nodes
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.starts_with(&prefix))
            .filter(|(_, node)| node.layer() < below)
            .map(|(path, _)| path.clone())
            .collect();
        for path in doomed {
            self.nodes.remove(&path);
        }
    }

    fn insert(&mut self, path: String, node: Node) {
        let layer = node.layer();
        if !matches!(node, Node::Dir { .. }) {
            // A file or link in place of a directory hides everything the directory held.
            self.remove_children(&path, layer + 1);
        }
        if matches!(node, Node::Dir { .. }) && matches!(self.nodes.get(&path), Some(Node::Dir { .. })) {
            return;
        }
        self.nodes.insert(path, node);
    }

    /// Apply one uncompressed layer tar on top of what is there.
    fn apply(&mut self, reader: impl Read, layer: usize) -> io::Result<()> {
        let mut archive = tar::Archive::new(reader);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = normalize(&entry.path()?.to_string_lossy());
            if path == "/" {
                continue;
            }
            let name = path.rsplit('/').next().unwrap_or_default().to_string();
            let dir = parent(&path).to_string();
            if name == ".wh..wh..opq" {
                self.remove_children(&dir, layer);
                continue;
            }
            if let Some(hidden) = name.strip_prefix(".wh.") {
                self.remove_tree(&join(&dir, hidden), layer + 1);
                continue;
            }
            let mode = entry.header().mode().unwrap_or(0o644);
            match entry.header().entry_type() {
                tar::EntryType::Directory => self.insert(path, Node::Dir { layer }),
                tar::EntryType::Symlink => {
                    let target = entry
                        .link_name()?
                        .map(|target| target.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.insert(path, Node::Symlink { target, layer });
                }
                tar::EntryType::Link => {
                    let target = entry
                        .link_name()?
                        .map(|target| normalize(&target.to_string_lossy()))
                        .unwrap_or_default();
                    if let Some(node) = self.nodes.get(&target).cloned() {
                        let node = match node {
                            Node::File(mut facts) => {
                                facts.layer = layer;
                                Node::File(facts)
                            }
                            other => other,
                        };
                        self.insert(path, node);
                    }
                }
                tar::EntryType::Regular | tar::EntryType::Continuous => {
                    let facts = examine(&mut entry, mode, layer)?;
                    self.insert(path, Node::File(Box::new(facts)));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for node in self.nodes.values() {
            if let Node::File(facts) = node {
                counts.files += 1;
                counts.elf += usize::from(facts.elf.is_some());
                counts.scripts += usize::from(facts.shebang.is_some());
            }
        }
        counts
    }

    fn is_dir(&self, path: &str) -> bool {
        if path == "/" || matches!(self.nodes.get(path), Some(Node::Dir { .. })) {
            return true;
        }
        let prefix = format!("{path}/");
        self.nodes
            .range(prefix.clone()..)
            .next()
            .is_some_and(|(next, _)| next.starts_with(&prefix))
    }

    /// The real path of `path`, following symlinks in every component, if it names something in the image.
    fn resolve(&self, path: &str) -> Option<String> {
        let mut pending: Vec<String> = normalize(path)
            .split('/')
            .filter(|p| !p.is_empty())
            .rev()
            .map(str::to_string)
            .collect();
        let mut current = "/".to_string();
        let mut links = 0;
        while let Some(part) = pending.pop() {
            let candidate = join(&current, &part);
            match self.nodes.get(&candidate) {
                Some(Node::Symlink { target, .. }) => {
                    links += 1;
                    if links > MAX_LINKS {
                        return None;
                    }
                    let resolved = join(&current, target);
                    current = "/".to_string();
                    pending.extend(resolved.split('/').filter(|p| !p.is_empty()).rev().map(str::to_string));
                }
                Some(_) => current = candidate,
                None if self.is_dir(&candidate) => current = candidate,
                None => return None,
            }
        }
        Some(current)
    }

    /// The file at `path`, through symlinks, with its real path.
    fn file(&self, path: &str) -> Option<(String, &FileFacts)> {
        let real = self.resolve(path)?;
        match self.nodes.get(&real) {
            Some(Node::File(facts)) => Some((real, facts)),
            _ => None,
        }
    }

    /// Where the loader would find the library `name` needed by the ELF file at `real` (a real path).
    fn find_library(&self, name: &str, real: &str, facts: &ElfFacts, context: &Context) -> bool {
        if name.contains('/') {
            return self.file(&join(parent(real), name)).is_some();
        }
        let origin = parent(real);
        let lib = if facts.class64 { "lib64" } else { "lib" };
        let expand = |dir: &String| -> Option<String> {
            if dir.contains("$PLATFORM") || dir.contains("${PLATFORM}") {
                return None;
            }
            let dir = dir
                .replace("${ORIGIN}", origin)
                .replace("$ORIGIN", origin)
                .replace("${LIB}", lib)
                .replace("$LIB", lib);
            Some(if dir.starts_with('/') {
                normalize(&dir)
            } else {
                join(&context.workdir, &dir)
            })
        };
        let triplet = match context.architecture.as_str() {
            "amd64" => "x86_64-linux-gnu",
            "arm64" => "aarch64-linux-gnu",
            "ppc64le" => "powerpc64le-linux-gnu",
            "s390x" => "s390x-linux-gnu",
            "riscv64" => "riscv64-linux-gnu",
            _ => "",
        };
        let mut dirs: Vec<String> = Vec::new();
        if facts.runpath.is_empty() {
            dirs.extend(facts.rpath.iter().filter_map(expand));
        }
        dirs.extend(context.ld_library_path.iter().filter_map(expand));
        dirs.extend(facts.runpath.iter().filter_map(expand));
        for default in ["/lib", "/usr/lib", "/lib64", "/usr/lib64"] {
            dirs.push(default.to_string());
        }
        if !triplet.is_empty() {
            dirs.push(format!("/lib/{triplet}"));
            dirs.push(format!("/usr/lib/{triplet}"));
        }
        dirs.iter().any(|dir| {
            self.file(&join(dir, name)).is_some_and(|(_, found)| match &found.elf {
                // The loader skips a library of the other class and keeps looking.
                Some(Ok(lib)) => lib.class64 == facts.class64,
                _ => true,
            })
        })
    }

    /// A program found as a shell would find it: a path, or a name on `PATH`.
    fn find_program(&self, program: &str, context: &Context) -> Option<(String, &FileFacts)> {
        if program.contains('/') {
            return self.file(&join(&context.workdir, program));
        }
        context.path.iter().find_map(|dir| self.file(&join(dir, program)))
    }

    /// Every problem and note about the files.
    fn findings(&self, context: &Context) -> (Vec<Finding>, Vec<Finding>) {
        let expected = expected_machine(&context.architecture);
        let mut wrong_machine: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut unreadable: Vec<String> = Vec::new();
        let mut missing_loaders: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut missing_libraries: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut missing_interpreters: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut placeholders: Vec<String> = Vec::new();

        for (path, node) in &self.nodes {
            let Node::File(facts) = node else { continue };
            // A conda-meta record names each file's placeholder on purpose. A `.pyc` cache keeps the build's paths
            // when its package does not declare it, but Python ignores it once its rewritten source no longer
            // matches the size and time the cache records.
            let harmless = path.contains("/conda-meta/") || (path.contains("/__pycache__/") && path.ends_with(".pyc"));
            if facts.placeholder && !harmless {
                placeholders.push(path.clone());
            }
            match &facts.elf {
                Some(Err(err)) => unreadable.push(format!("{path} ({err})")),
                Some(Ok(elf_facts)) => {
                    if let Some((machine, little)) = expected
                        && (elf_facts.machine != machine || elf_facts.little_endian != little)
                    {
                        wrong_machine
                            .entry(machine_name(elf_facts.machine))
                            .or_default()
                            .push(path.clone());
                    }
                    if let Some(loader) = &elf_facts.interpreter
                        && self.file(loader).is_none()
                    {
                        missing_loaders.entry(loader.clone()).or_default().insert(path.clone());
                    }
                    for needed in &elf_facts.needed {
                        if !self.find_library(needed, path, elf_facts, context) {
                            missing_libraries
                                .entry(needed.clone())
                                .or_default()
                                .insert(path.clone());
                        }
                    }
                }
                None => {}
            }
            if let Some(shebang) = &facts.shebang {
                let interpreter = &shebang[0];
                let env_program = shebang.get(1).filter(|_| interpreter.ends_with("/env"));
                if self.file(interpreter).is_none() {
                    missing_interpreters
                        .entry(interpreter.clone())
                        .or_default()
                        .insert(path.clone());
                } else if let Some(program) = env_program
                    && self.find_program(program, context).is_none()
                {
                    missing_interpreters
                        .entry(format!("{interpreter} {program}"))
                        .or_default()
                        .insert(path.clone());
                }
            }
        }

        let mut problems = Vec::new();
        if !wrong_machine.is_empty() {
            let paths: Vec<String> = wrong_machine.values().flatten().cloned().collect();
            let machines: Vec<&str> = wrong_machine.keys().map(String::as_str).collect();
            problems.push(Finding {
                kind: "architecture",
                message: format!(
                    "{} built for {}, not {}",
                    if paths.len() == 1 {
                        "1 ELF file is".to_string()
                    } else {
                        format!("{} ELF files are", paths.len())
                    },
                    machines.join(", "),
                    context.architecture
                ),
                paths,
                help: Some(
                    "build for the platform these binaries are for, or lock the environment for this one".to_string(),
                ),
            });
        }
        if !unreadable.is_empty() {
            problems.push(Finding {
                kind: "elf",
                message: format!("{} cannot be read as ELF", plural(unreadable.len(), "file")),
                paths: unreadable,
                help: None,
            });
        }

        // glibc is one finding: its loader and libraries come together from a base image, or not at all.
        let glibc_libraries: Vec<String> = missing_libraries
            .keys()
            .filter(|name| is_glibc(name))
            .cloned()
            .collect();
        if !missing_loaders.is_empty() || !glibc_libraries.is_empty() {
            let mut affected: BTreeSet<String> = missing_loaders.values().flatten().cloned().collect();
            for name in &glibc_libraries {
                affected.extend(missing_libraries[name].iter().cloned());
            }
            let mut wanted: Vec<String> = missing_loaders.keys().cloned().collect();
            wanted.extend(glibc_libraries.iter().cloned());
            let lacking = if missing_loaders
                .keys()
                .all(|loader| is_glibc(loader.rsplit('/').next().unwrap_or(loader)))
            {
                "glibc is not in the image"
            } else {
                "the loader these files name is not in the image"
            };
            problems.push(Finding {
                kind: "glibc",
                message: format!(
                    "{lacking}: {} need {}",
                    plural(affected.len(), "ELF file"),
                    wanted.join(", ")
                ),
                paths: affected.into_iter().collect(),
                help: Some(
                    "conda packages use the system's glibc: build on a base image that has it (`--base`, #12), or \
                     add the loader and libraries in a layer below the environment"
                        .to_string(),
                ),
            });
        }
        for (name, paths) in missing_libraries.iter().filter(|(name, _)| !is_glibc(name)) {
            problems.push(Finding {
                kind: "library",
                message: format!(
                    "{name} is needed by {}, but the loader would not find it",
                    plural(paths.len(), "ELF file")
                ),
                paths: paths.iter().cloned().collect(),
                help: Some(
                    "add the package that provides it to the environment, or check its RUNPATH and the image's \
                     LD_LIBRARY_PATH"
                        .to_string(),
                ),
            });
        }
        for (interpreter, paths) in &missing_interpreters {
            problems.push(Finding {
                kind: "shebang",
                message: format!(
                    "{} start with #!{interpreter}, which is not in the image",
                    plural(paths.len(), "script")
                ),
                paths: paths.iter().cloned().collect(),
                help: if interpreter.starts_with("/bin/") || interpreter.starts_with("/usr/bin/") {
                    Some("a base image provides /bin/sh and /usr/bin/env (`--base`, #12)".to_string())
                } else {
                    None
                },
            });
        }
        if !placeholders.is_empty() {
            problems.push(Finding {
                kind: "placeholder",
                message: format!(
                    "{} still hold a conda build placeholder",
                    plural(placeholders.len(), "file")
                ),
                paths: placeholders,
                help: Some(
                    "installing replaces the placeholders a package declares, so these files were not declared: if \
                     one is read at run time, report it to the package's feedstock (or, if the package does \
                     declare it, at https://github.com/millsks/pixi-oci/issues)"
                        .to_string(),
                ),
            });
        }

        let mut notes = Vec::new();
        match &context.program {
            None => notes.push(Finding {
                kind: "entrypoint",
                message: "the image sets no entrypoint or cmd, so a container needs a command".to_string(),
                paths: Vec::new(),
                help: Some("set one with `pixi oci build --entrypoint` or `--cmd`".to_string()),
            }),
            Some(program) => match self.find_program(program, context) {
                None => problems.push(Finding {
                    kind: "entrypoint",
                    message: format!(
                        "{program} is not in the image{}",
                        if program.contains('/') {
                            String::new()
                        } else {
                            format!(" on its PATH ({})", context.path.join(":"))
                        }
                    ),
                    paths: Vec::new(),
                    help: Some("check `--entrypoint` and `--cmd`, and the image's PATH (`--env PATH=...`)".to_string()),
                }),
                Some((real, facts)) if !facts.executable => problems.push(Finding {
                    kind: "entrypoint",
                    message: format!("{program} is {real}, which is not executable"),
                    paths: vec![real],
                    help: None,
                }),
                Some(_) => {}
            },
        }
        (problems, notes)
    }
}

fn is_glibc(name: &str) -> bool {
    GLIBC.contains(&name) || name.starts_with("ld-linux") || name.starts_with("ld64.so") || name.starts_with("ld.so")
}

/// The ELF machine and byte order an image's architecture runs.
fn expected_machine(architecture: &str) -> Option<(u16, bool)> {
    Some(match architecture {
        "amd64" => (elf::EM_X86_64.0, true),
        "arm64" => (elf::EM_AARCH64.0, true),
        "ppc64le" => (elf::EM_PPC64.0, true),
        "s390x" => (elf::EM_S390.0, false),
        "riscv64" => (elf::EM_RISCV.0, true),
        "386" => (elf::EM_386.0, true),
        "arm" => (elf::EM_ARM.0, true),
        _ => return None,
    })
}

fn machine_name(machine: u16) -> String {
    match elf::Machine(machine) {
        elf::EM_X86_64 => "x86-64".to_string(),
        elf::EM_AARCH64 => "AArch64".to_string(),
        elf::EM_PPC64 => "PowerPC64".to_string(),
        elf::EM_S390 => "s390".to_string(),
        elf::EM_RISCV => "RISC-V".to_string(),
        elf::EM_386 => "x86".to_string(),
        elf::EM_ARM => "ARM".to_string(),
        _ => format!("machine {machine}"),
    }
}

/// Finds the placeholders across reads of a stream, keeping enough of each read to catch one split between two.
struct Scanner {
    tail: Vec<u8>,
    found: bool,
}

impl Scanner {
    fn feed(&mut self, chunk: &[u8]) {
        if self.found {
            return;
        }
        let longest = PLACEHOLDERS.iter().map(|p| p.len()).max().unwrap_or(0);
        let mut window = std::mem::take(&mut self.tail);
        window.extend_from_slice(chunk);
        self.found = PLACEHOLDERS.iter().any(|needle| contains(&window, needle));
        let keep = window.len().min(longest - 1);
        self.tail = window.split_off(window.len() - keep);
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return true;
    };
    let mut start = 0;
    while let Some(offset) = haystack[start..].iter().position(|&b| b == first) {
        let at = start + offset;
        if haystack.len() - at < needle.len() {
            return false;
        }
        if &haystack[at + 1..at + needle.len()] == rest {
            return true;
        }
        start = at + 1;
    }
    false
}

/// Read one file of a layer, keeping its contents only if it is an ELF file, and only what an ELF parser needs.
fn examine(reader: &mut impl Read, mode: u32, layer: usize) -> io::Result<FileFacts> {
    let mut scanner = Scanner {
        tail: Vec::new(),
        found: false,
    };
    let mut buffer = vec![0u8; 64 << 10];
    let mut head: Vec<u8> = Vec::new();
    let mut spill: Option<tempfile::NamedTempFile> = None;
    let mut is_elf: Option<bool> = None;
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        let chunk = &buffer[..n];
        scanner.feed(chunk);
        if is_elf.is_none() {
            head.extend_from_slice(chunk);
            if head.len() >= 4 {
                is_elf = Some(head.starts_with(b"\x7fELF"));
                if is_elf == Some(false) {
                    head.truncate(512);
                }
            }
            continue;
        }
        if is_elf == Some(true) {
            if let Some(file) = &mut spill {
                file.write_all(chunk)?;
            } else if head.len() + n > IN_MEMORY {
                let mut file = tempfile::NamedTempFile::new()?;
                file.write_all(&head)?;
                file.write_all(chunk)?;
                head = Vec::new();
                spill = Some(file);
            } else {
                head.extend_from_slice(chunk);
            }
        }
    }
    let executable = mode & 0o111 != 0;
    let elf = if head.starts_with(b"\x7fELF") || spill.is_some() {
        Some(match &mut spill {
            Some(file) => {
                file.flush()?;
                let cache = object::ReadCache::new(file.reopen()?);
                elf_facts(&cache)
            }
            None => elf_facts(head.as_slice()),
        })
    } else {
        None
    };
    let shebang = (executable && elf.is_none() && head.starts_with(b"#!"))
        .then(|| shebang(&head))
        .flatten();
    Ok(FileFacts {
        layer,
        executable,
        elf,
        shebang,
        placeholder: scanner.found,
    })
}

/// The interpreter of a `#!` line and its first argument; for `env -S`, the program after the flag.
fn shebang(head: &[u8]) -> Option<Vec<String>> {
    let line = head[2..].split(|&b| b == b'\n').next()?;
    let line = String::from_utf8_lossy(line);
    let mut words: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    if words.is_empty() {
        return None;
    }
    if words[0].ends_with("/env") {
        let env = words[0].clone();
        words.retain(|word| !word.starts_with('-') || *word == env);
        if words.get(1).is_some_and(|word| word.contains('=')) {
            words.truncate(1);
        }
    }
    words.truncate(2);
    Some(words)
}

fn elf_facts<'data, R: ReadRef<'data>>(data: R) -> Result<ElfFacts, String> {
    let ident = data.read_bytes_at(0, 16).map_err(|()| "too short".to_string())?;
    // e_ident[EI_CLASS]
    match elf::FileClass(ident[4]) {
        elf::ELFCLASS32 => parse_elf::<elf::FileHeader32<Endianness>, R>(data),
        elf::ELFCLASS64 => parse_elf::<elf::FileHeader64<Endianness>, R>(data),
        class => Err(format!("unknown ELF class {class}")),
    }
}

fn parse_elf<'data, H, R>(data: R) -> Result<ElfFacts, String>
where
    H: FileHeader<Endian = Endianness>,
    R: ReadRef<'data>,
{
    let text = |err: object::read::Error| err.to_string();
    let header = H::parse(data).map_err(text)?;
    let endian = header.endian().map_err(text)?;
    let mut facts = ElfFacts {
        machine: header.e_machine(endian).0,
        class64: header.is_type_64(),
        little_endian: header.is_little_endian(),
        ..ElfFacts::default()
    };
    for segment in header.program_headers(endian, data).map_err(text)? {
        if let Some(interpreter) = segment.interpreter(endian, data).map_err(text)? {
            facts.interpreter = Some(String::from_utf8_lossy(interpreter).into_owned());
        }
    }
    let sections = header.sections(endian, data).map_err(text)?;
    if let Some((dynamic, link)) = sections.dynamic(endian, data).map_err(text)? {
        let strings = sections.strings(endian, data, link).map_err(text)?;
        for entry in dynamic {
            let tag = entry.tag(endian);
            if tag == elf::DT_NULL {
                break;
            }
            let value = || {
                entry
                    .string(endian, strings)
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .map_err(text)
            };
            let list = |value: String| -> Vec<String> {
                value.split(':').filter(|s| !s.is_empty()).map(str::to_string).collect()
            };
            if tag == elf::DT_NEEDED {
                facts.needed.push(value()?);
            } else if tag == elf::DT_RPATH {
                facts.rpath.extend(list(value()?));
            } else if tag == elf::DT_RUNPATH {
                facts.runpath.extend(list(value()?));
            }
        }
    }
    Ok(facts)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// What an ELF file built by [`elf_file`] asks for.
    #[derive(Default)]
    pub(crate) struct Elf<'a> {
        pub machine: u16,
        pub interpreter: Option<&'a str>,
        pub needed: &'a [&'a str],
        pub rpath: Option<&'a str>,
        pub runpath: Option<&'a str>,
    }

    /// A little-endian 64-bit ELF shared object: a `PT_INTERP` segment when it names a loader, and `.dynstr` and
    /// `.dynamic` sections with `DT_NEEDED`, `DT_RPATH` and `DT_RUNPATH`.
    pub(crate) fn elf_file(spec: &Elf) -> Vec<u8> {
        fn pad(bytes: &mut Vec<u8>, to: usize) {
            while !bytes.len().is_multiple_of(to) {
                bytes.push(0);
            }
        }
        let phnum: u16 = spec.interpreter.is_some().into();
        let mut body = Vec::new();
        let base = 64 + 56 * usize::from(phnum);
        let interp_offset = base;
        if let Some(interpreter) = spec.interpreter {
            body.extend_from_slice(interpreter.as_bytes());
            body.push(0);
        }
        let interp_size = body.len();
        let mut strings = vec![0u8];
        let mut add = |text: &str| {
            let offset = strings.len() as u64;
            strings.extend_from_slice(text.as_bytes());
            strings.push(0);
            offset
        };
        let mut dynamic: Vec<(i64, u64)> = spec.needed.iter().map(|name| (1, add(name))).collect();
        if let Some(rpath) = spec.rpath {
            dynamic.push((15, add(rpath)));
        }
        if let Some(runpath) = spec.runpath {
            dynamic.push((29, add(runpath)));
        }
        dynamic.push((0, 0));
        let strings_offset = base + body.len();
        body.extend_from_slice(&strings);
        pad(&mut body, 8);
        let dynamic_offset = base + body.len();
        for (tag, value) in &dynamic {
            body.extend_from_slice(&tag.to_le_bytes());
            body.extend_from_slice(&value.to_le_bytes());
        }
        let names = b"\0.dynstr\0.dynamic\0.shstrtab\0";
        let names_offset = base + body.len();
        body.extend_from_slice(names);
        pad(&mut body, 8);
        let shoff = (base + body.len()) as u64;

        let mut out = Vec::new();
        out.extend_from_slice(b"\x7fELF");
        out.extend_from_slice(&[2, 1, 1, 0]);
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(&3u16.to_le_bytes()); // e_type: ET_DYN
        out.extend_from_slice(&spec.machine.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes()); // e_entry
        out.extend_from_slice(&64u64.to_le_bytes()); // e_phoff
        out.extend_from_slice(&shoff.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // e_flags
        out.extend_from_slice(&64u16.to_le_bytes());
        out.extend_from_slice(&56u16.to_le_bytes());
        out.extend_from_slice(&phnum.to_le_bytes());
        out.extend_from_slice(&64u16.to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes()); // e_shnum
        out.extend_from_slice(&3u16.to_le_bytes()); // e_shstrndx: .shstrtab
        if spec.interpreter.is_some() {
            out.extend_from_slice(&3u32.to_le_bytes()); // PT_INTERP
            out.extend_from_slice(&4u32.to_le_bytes());
            for value in [interp_offset as u64, 0, 0, interp_size as u64, interp_size as u64, 1] {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        out.extend_from_slice(&body);
        out.extend_from_slice(&[0; 64]); // the null section
        let section = |out: &mut Vec<u8>,
                       name: u32,
                       kind: u32,
                       offset: usize,
                       size: usize,
                       link: u32,
                       align: u64,
                       entsize: u64| {
            out.extend_from_slice(&name.to_le_bytes());
            out.extend_from_slice(&kind.to_le_bytes());
            for value in [0u64, 0, offset as u64, size as u64] {
                out.extend_from_slice(&value.to_le_bytes());
            }
            out.extend_from_slice(&link.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&align.to_le_bytes());
            out.extend_from_slice(&entsize.to_le_bytes());
        };
        section(&mut out, 1, 3, strings_offset, strings.len(), 0, 1, 0); // .dynstr: SHT_STRTAB
        section(&mut out, 9, 6, dynamic_offset, dynamic.len() * 16, 1, 8, 16); // .dynamic: SHT_DYNAMIC
        section(&mut out, 18, 3, names_offset, names.len(), 0, 1, 0); // .shstrtab
        out
    }

    pub(crate) const X86_64: u16 = 62;
    pub(crate) const AARCH64: u16 = 183;

    /// A layer tar of `(path, entry)`.
    enum Entry<'a> {
        File(&'a [u8], u32),
        Dir,
        Link(&'a str),
        Hard(&'a str),
    }

    fn layer(entries: &[(&str, Entry)]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (path, entry) in entries {
            let mut header = tar::Header::new_gnu();
            match entry {
                Entry::File(content, mode) => {
                    header.set_size(content.len() as u64);
                    header.set_mode(*mode);
                    tar.append_data(&mut header, path, *content).unwrap();
                }
                Entry::Dir => {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_mode(0o755);
                    header.set_size(0);
                    tar.append_data(&mut header, path, io::empty()).unwrap();
                }
                Entry::Link(target) => {
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_size(0);
                    tar.append_link(&mut header, path, target).unwrap();
                }
                Entry::Hard(target) => {
                    header.set_entry_type(tar::EntryType::Link);
                    header.set_size(0);
                    tar.append_link(&mut header, path, target).unwrap();
                }
            }
        }
        tar.into_inner().unwrap()
    }

    fn fs_of(layers: &[Vec<u8>]) -> Fs {
        let mut fs = Fs::default();
        for (i, bytes) in layers.iter().enumerate() {
            fs.apply(bytes.as_slice(), i).unwrap();
        }
        fs
    }

    fn context(program: Option<&str>) -> Context {
        Context {
            architecture: "amd64".to_string(),
            path: vec!["/opt/env/bin".to_string(), "/usr/bin".to_string(), "/bin".to_string()],
            ld_library_path: Vec::new(),
            program: program.map(str::to_string),
            workdir: "/".to_string(),
        }
    }

    fn kinds(findings: &[Finding]) -> Vec<&'static str> {
        findings.iter().map(|finding| finding.kind).collect()
    }

    fn tool() -> Vec<u8> {
        elf_file(&Elf {
            machine: X86_64,
            interpreter: Some("/lib64/ld-linux-x86-64.so.2"),
            needed: &["libthing.so.1", "libc.so.6"],
            runpath: Some("$ORIGIN/../lib"),
            ..Elf::default()
        })
    }

    fn libthing() -> Vec<u8> {
        elf_file(&Elf {
            machine: X86_64,
            needed: &["libc.so.6"],
            ..Elf::default()
        })
    }

    fn environment() -> Vec<u8> {
        let (tool, lib) = (tool(), libthing());
        layer(&[
            ("opt/env/bin/tool", Entry::File(&tool, 0o755)),
            ("opt/env/lib/libthing.so.1", Entry::File(&lib, 0o755)),
        ])
    }

    fn glibc() -> Vec<u8> {
        let libc = elf_file(&Elf {
            machine: X86_64,
            ..Elf::default()
        });
        layer(&[
            ("lib/x86_64-linux-gnu/ld-linux-x86-64.so.2", Entry::File(&libc, 0o755)),
            ("lib/x86_64-linux-gnu/libc.so.6", Entry::File(&libc, 0o755)),
            ("lib64", Entry::Dir),
            (
                "lib64/ld-linux-x86-64.so.2",
                Entry::Link("../lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"),
            ),
            ("bin/sh", Entry::File(&libc, 0o755)),
            ("usr/bin/env", Entry::File(b"#!/bin/sh\n", 0o755)),
        ])
    }

    #[test]
    fn the_elf_parser_reads_what_the_loader_reads() {
        let facts = elf_facts(tool().as_slice()).unwrap();
        assert_eq!(facts.machine, X86_64);
        assert!(facts.class64 && facts.little_endian);
        assert_eq!(facts.interpreter.as_deref(), Some("/lib64/ld-linux-x86-64.so.2"));
        assert_eq!(facts.needed, ["libthing.so.1", "libc.so.6"]);
        assert_eq!(facts.runpath, ["$ORIGIN/../lib"]);
        assert!(facts.rpath.is_empty());
        let rpath = elf_facts(
            elf_file(&Elf {
                machine: AARCH64,
                rpath: Some("/a:/b:"),
                ..Elf::default()
            })
            .as_slice(),
        )
        .unwrap();
        assert_eq!((rpath.machine, rpath.interpreter), (AARCH64, None));
        assert_eq!(rpath.rpath, ["/a", "/b"]);
        assert!(elf_facts(&b"\x7fELF"[..]).is_err());
        let mut bad_class = tool();
        bad_class[4] = 9;
        assert!(
            elf_facts(bad_class.as_slice())
                .unwrap_err()
                .contains("unknown ELF class")
        );
        let mut truncated = tool();
        truncated.truncate(100);
        assert!(elf_facts(truncated.as_slice()).is_err());
    }

    #[test]
    fn a_binary_without_glibc_is_one_finding() {
        let fs = fs_of(&[environment()]);
        let (problems, notes) = fs.findings(&context(Some("tool")));
        assert_eq!(kinds(&problems), ["glibc"]);
        assert_eq!(problems[0].paths, ["/opt/env/bin/tool", "/opt/env/lib/libthing.so.1"]);
        assert!(
            problems[0].message.contains("/lib64/ld-linux-x86-64.so.2, libc.so.6"),
            "{}",
            problems[0].message
        );
        assert!(notes.is_empty());
        assert_eq!(
            fs.counts(),
            Counts {
                files: 2,
                elf: 2,
                scripts: 0
            }
        );
    }

    #[test]
    fn a_glibc_layer_below_satisfies_the_loader() {
        let fs = fs_of(&[glibc(), environment()]);
        let (problems, _) = fs.findings(&context(Some("tool")));
        assert!(problems.is_empty(), "{problems:#?}");
        // A library the binary's RUNPATH does not reach, nor the default directories, is missing.
        let fs = fs_of(&[
            glibc(),
            environment(),
            layer(&[("opt/env/lib/.wh.libthing.so.1", Entry::File(b"", 0o644))]),
        ]);
        let (problems, _) = fs.findings(&context(Some("tool")));
        assert_eq!(kinds(&problems), ["library"]);
        assert_eq!(problems[0].paths, ["/opt/env/bin/tool"]);
        // LD_LIBRARY_PATH reaches a library elsewhere.
        let lib = libthing();
        let fs = fs_of(&[
            glibc(),
            environment(),
            layer(&[("opt/other/libthing.so.1", Entry::File(&lib, 0o755))]),
        ]);
        let mut wide = context(Some("tool"));
        wide.ld_library_path = vec!["/opt/other".to_string()];
        let without = fs_of(&[
            glibc(),
            layer(&[("opt/env/bin/tool", Entry::File(&tool(), 0o755))]),
            layer(&[("opt/other/libthing.so.1", Entry::File(&lib, 0o755))]),
        ]);
        assert_eq!(kinds(&without.findings(&context(Some("tool"))).0), ["library"]);
        assert!(without.findings(&wide).0.is_empty());
        assert!(fs.findings(&wide).0.is_empty());
    }

    #[test]
    fn binaries_for_another_machine_are_named() {
        let arm = elf_file(&Elf {
            machine: AARCH64,
            ..Elf::default()
        });
        let fs = fs_of(&[layer(&[
            ("opt/env/bin/arm", Entry::File(&arm, 0o755)),
            (
                "opt/env/bin/odd",
                Entry::File(
                    &elf_file(&Elf {
                        machine: 9999,
                        ..Elf::default()
                    }),
                    0o755,
                ),
            ),
        ])]);
        let (problems, _) = fs.findings(&context(None));
        assert_eq!(kinds(&problems), ["architecture"]);
        assert!(
            problems[0]
                .message
                .starts_with("2 ELF files are built for AArch64, machine 9999, not amd64"),
            "{}",
            problems[0].message
        );
        let fs = fs_of(&[layer(&[("opt/env/bin/arm", Entry::File(&arm, 0o755))])]);
        assert!(
            fs.findings(&context(None)).0[0]
                .message
                .starts_with("1 ELF file is built for AArch64")
        );
        let fs = fs_of(&[layer(&[(
            "opt/env/bin/broken",
            Entry::File(b"\x7fELF\x02\x01\x01", 0o755),
        )])]);
        assert_eq!(kinds(&fs.findings(&context(None)).0), ["elf"]);
        assert_eq!(machine_name(X86_64), "x86-64");
        assert!(expected_machine("mips").is_none());
    }

    #[test]
    fn scripts_need_their_interpreters() {
        let scripts = layer(&[
            ("opt/env/bin/run", Entry::File(b"#!/bin/sh -e\necho hi\n", 0o755)),
            ("opt/env/bin/py", Entry::File(b"#!/usr/bin/env python3\n", 0o755)),
            ("opt/env/bin/ok", Entry::File(b"#!/opt/env/bin/python3\n", 0o755)),
            ("opt/env/share/not-run.py", Entry::File(b"#!/usr/bin/python9\n", 0o644)),
            ("opt/env/bin/python3", Entry::File(b"\x7fELF", 0o755)),
        ]);
        let fs = fs_of(std::slice::from_ref(&scripts));
        let (problems, _) = fs.findings(&context(None));
        assert_eq!(kinds(&problems), ["elf", "shebang", "shebang"]);
        assert_eq!(
            problems[1].message,
            "1 script start with #!/bin/sh, which is not in the image"
        );
        assert_eq!(problems[2].paths, ["/opt/env/bin/py"]);
        assert_eq!(fs.counts().scripts, 3);

        // With /bin/sh and env present, env still has to find python3 on PATH.
        let fs = fs_of(&[
            glibc(),
            scripts,
            layer(&[("opt/env/bin/.wh.python3", Entry::File(b"", 0o644))]),
        ]);
        let (problems, _) = fs.findings(&context(None));
        assert_eq!(kinds(&problems), ["shebang", "shebang"]);
        assert!(
            problems.iter().any(|p| p.message.contains("#!/usr/bin/env python3")),
            "{problems:#?}"
        );
    }

    #[test]
    fn shebang_lines_are_split_as_the_kernel_and_env_split_them() {
        let words = |line: &[u8]| shebang(line);
        assert_eq!(words(b"#!/bin/sh -e\n"), Some(vec!["/bin/sh".into(), "-e".into()]));
        assert_eq!(
            words(b"#! /usr/bin/env python3\n"),
            Some(vec!["/usr/bin/env".into(), "python3".into()])
        );
        assert_eq!(
            words(b"#!/usr/bin/env -S python3 -u\n"),
            Some(vec!["/usr/bin/env".into(), "python3".into()])
        );
        assert_eq!(words(b"#!/usr/bin/env FOO=1 bash\n"), Some(vec!["/usr/bin/env".into()]));
        assert_eq!(words(b"#!\n"), None);
    }

    #[test]
    fn placeholders_left_behind_are_found_even_across_reads() {
        let mut scanner = Scanner {
            tail: Vec::new(),
            found: false,
        };
        scanner.feed(b"prefix=/opt/anaconda1ana");
        assert!(!scanner.found);
        scanner.feed(b"conda2anaconda3/bin\n");
        assert!(scanner.found);
        assert!(contains(b"abc", b""));
        assert!(!contains(b"ab", b"abc"));
        assert!(!contains(b"xxplacehold_", b"placehold_placehold_placehold"));

        let left = format!(
            "x=/build/_h_env_placehold_placehold_placehold/lib\n{}",
            "y".repeat(100_000)
        );
        let fs = fs_of(&[layer(&[
            ("opt/env/share/left.txt", Entry::File(left.as_bytes(), 0o644)),
            (
                "opt/env/conda-meta/pkg.json",
                Entry::File(b"{\"prefix_placeholder\": \"/placehold_placehold_placehold\"}", 0o644),
            ),
            (
                "opt/env/lib/python3.13/__pycache__/_sysconfigdata.cpython-313.pyc",
                Entry::File(b"\x00/build/_h_env_placehold_placehold_placehold", 0o644),
            ),
        ])]);
        let (problems, _) = fs.findings(&context(None));
        assert_eq!(kinds(&problems), ["placeholder"]);
        assert_eq!(problems[0].paths, ["/opt/env/share/left.txt"]);
    }

    #[test]
    fn the_entrypoint_must_resolve_to_an_executable() {
        let fs = fs_of(&[layer(&[
            ("opt/env/bin/tool", Entry::File(b"#!/opt/env/bin/tool\n", 0o755)),
            ("opt/env/share/data", Entry::File(b"data", 0o644)),
            ("work/run", Entry::Link("/opt/env/bin/tool")),
        ])]);
        assert!(fs.findings(&context(Some("tool"))).0.is_empty());
        assert!(fs.findings(&context(Some("/work/run"))).0.is_empty());
        let mut relative = context(Some("./run"));
        relative.workdir = "/work".to_string();
        assert!(fs.findings(&relative).0.is_empty());
        let (problems, _) = fs.findings(&context(Some("missing")));
        assert_eq!(
            problems[0].message,
            "missing is not in the image on its PATH (/opt/env/bin:/usr/bin:/bin)"
        );
        let (problems, _) = fs.findings(&context(Some("/opt/env/share/data")));
        assert_eq!(
            problems[0].message,
            "/opt/env/share/data is /opt/env/share/data, which is not executable"
        );
        let (problems, notes) = fs.findings(&context(None));
        assert!(problems.is_empty());
        assert_eq!(kinds(&notes), ["entrypoint"]);
        let (problems, _) = fs.findings(&context(Some("/nowhere/tool")));
        assert_eq!(problems[0].message, "/nowhere/tool is not in the image");
    }

    #[test]
    fn layers_apply_in_order_with_whiteouts_and_links() {
        let fs = fs_of(&[
            layer(&[
                ("./a/one", Entry::File(b"1", 0o644)),
                ("a/two", Entry::File(b"2", 0o644)),
                ("b/keep", Entry::File(b"k", 0o644)),
                ("c/x", Entry::File(b"x", 0o644)),
                ("d", Entry::Dir),
            ]),
            layer(&[
                ("a/.wh..wh..opq", Entry::File(b"", 0o644)),
                ("a/three", Entry::File(b"3", 0o644)),
                ("b/.wh.keep", Entry::File(b"", 0o644)),
                ("c", Entry::File(b"now a file", 0o755)),
                ("d", Entry::Dir),
                ("e/hard", Entry::Hard("a/three")),
                ("e/dangling", Entry::Hard("a/nothing")),
            ]),
        ]);
        let paths: Vec<&str> = fs.nodes.keys().map(String::as_str).collect();
        assert_eq!(paths, ["/a/three", "/c", "/d", "/e/hard"]);
        assert!(fs.file("/e/hard").is_some());
    }

    #[test]
    fn symlinks_resolve_like_the_kernel() {
        let fs = fs_of(&[layer(&[
            ("usr/lib/libz.so.1.3", Entry::File(b"z", 0o644)),
            ("usr/lib/libz.so.1", Entry::Link("libz.so.1.3")),
            ("lib", Entry::Link("usr/lib")),
            ("abs", Entry::Link("/usr/lib/libz.so.1")),
            ("loop/a", Entry::Link("b")),
            ("loop/b", Entry::Link("a")),
            ("up", Entry::Link("../../../usr/lib/libz.so.1")),
        ])]);
        assert_eq!(fs.resolve("/lib/libz.so.1").as_deref(), Some("/usr/lib/libz.so.1.3"));
        assert_eq!(fs.resolve("/abs").as_deref(), Some("/usr/lib/libz.so.1.3"));
        assert_eq!(fs.resolve("/up").as_deref(), Some("/usr/lib/libz.so.1.3"));
        assert_eq!(fs.resolve("/usr").as_deref(), Some("/usr"));
        assert_eq!(fs.resolve("/loop/a"), None);
        assert_eq!(fs.resolve("/nothing"), None);
        assert!(fs.file("/usr/lib").is_none());
        assert_eq!(normalize("./a//b/../c/."), "/a/c");
        assert_eq!(parent("/a"), "/");
        assert_eq!(join("/a", "/b"), "/b");
    }

    #[test]
    fn a_large_elf_file_is_read_from_a_temporary_file() {
        let mut big = tool();
        big.resize(IN_MEMORY + (1 << 20), 0);
        let facts = examine(&mut big.as_slice(), 0o755, 0).unwrap();
        let elf_facts = facts.elf.unwrap().unwrap();
        assert_eq!(elf_facts.needed, ["libthing.so.1", "libc.so.6"]);
        assert!(facts.shebang.is_none());
        let tiny = examine(&mut &b"ab"[..], 0o644, 0).unwrap();
        assert!(tiny.elf.is_none() && !tiny.executable);
    }

    /// An image built from packages with a binary, its library, a shell script and a file whose placeholder was not
    /// declared, so installing leaves it.
    fn built(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let spec = testing::Spec::new("tool")
            .file("bin/tool", tool(), true, None)
            .file("bin/run", "#!/bin/sh\necho hi\n", true, None)
            .file(
                "share/left.txt",
                "prefix=/opt/anaconda1anaconda2anaconda3\n",
                false,
                None,
            );
        let lib = testing::Spec::new("libthing").file("lib/libthing.so.1", libthing(), true, None);
        let plan = testing::plan_of(vec![testing::package(dir, spec), testing::package(dir, lib)], 100);
        let output = dir.join(name);
        testing::build_image(dir, &plan, &output);
        output
    }

    use crate::testing;

    #[test]
    fn check_reads_a_built_image() {
        for name in ["layout", "image.tar"] {
            let dir = tempfile::tempdir().unwrap();
            let report = check(&built(dir.path(), name), None).unwrap();
            assert!(!report.ok);
            assert_eq!(
                kinds(&report.problems),
                ["glibc", "shebang", "placeholder"],
                "{}",
                report.text()
            );
            assert!(report.notes.is_empty(), "{:#?}", report.notes);
            assert_eq!(report.platform.as_deref(), Some("linux/amd64"));
            assert_eq!(report.checked.elf, 2);
            assert!(report.checked.files >= 4);
            assert_eq!(
                report.problems[0].paths,
                ["/opt/env/bin/tool", "/opt/env/lib/libthing.so.1"]
            );
            let text = report.text();
            assert!(text.contains(": 3 problems in "), "{text}");
            assert!(
                text.contains("  glibc: glibc is not in the image: 2 ELF files need "),
                "{text}"
            );
            assert!(
                text.contains("    help: conda packages use the system's glibc"),
                "{text}"
            );
            let json: serde_json::Value = serde_json::from_str(&report.json()).unwrap();
            assert_eq!(json["ok"], false);
            assert_eq!(json["problems"][1]["paths"][0], "/opt/env/bin/run");
        }
    }

    #[test]
    fn a_clean_image_reports_no_problems() {
        let dir = tempfile::tempdir().unwrap();
        let spec = testing::Spec::new("data").file("share/data.txt", "hello\n", false, None);
        let plan = testing::plan_of(vec![testing::package(dir.path(), spec)], 100);
        let image = dir.path().join("image.tar");
        testing::build_image(dir.path(), &plan, &image);
        let report = check(&image, None).unwrap();
        // The entrypoint `tool` is not in this image.
        assert_eq!(kinds(&report.problems), ["entrypoint"]);
        let mut fine = report.clone();
        fine.problems.clear();
        fine.ok = true;
        assert!(fine.text().contains(": no problems in "), "{}", fine.text());
        let many: Vec<String> = (0..5).map(|i| format!("/p{i}")).collect();
        fine.notes.push(Finding {
            kind: "x",
            message: "m".to_string(),
            paths: many,
            help: None,
        });
        assert!(
            fine.text().contains("    /p0, /p1, /p2, and 2 more\n"),
            "{}",
            fine.text()
        );
    }

    #[test]
    fn an_invalid_image_is_not_looked_into() {
        let dir = tempfile::tempdir().unwrap();
        let report = check(dir.path(), None).unwrap();
        assert!(!report.ok);
        assert_eq!(kinds(&report.problems), ["spec", "spec"]);
        assert_eq!(kinds(&report.notes), ["skipped"]);
        assert!(check(&dir.path().join("missing"), None).is_err());
    }
}

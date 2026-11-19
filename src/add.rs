//! `--add SRC:DEST`: files from the host in one application layer above the environment, as a Dockerfile's `COPY`.
//!
//! A directory is walked with git's ignore rules (every `.gitignore` from the directory up, and
//! `.git/info/exclude`), `.pixi-oci-ignore` files read the same way, and the workspace's `.dockerignore`, whose
//! patterns are relative to the workspace as docker's are to its build context. `.git` and `.pixi` are always left
//! out. A file named by itself is added whatever the ignore files say.
//!
//! Only content reaches the layer: every entry is owned by root and stamped with [`crate::build::LAYER_MTIME`], and
//! a file is 0755 when it starts with `#!` or is an ELF file, 0644 otherwise. The host's mode bits are not read, so
//! a checkout on Windows gives the same layer as one on Linux.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::layer::{Blob, Writer};

/// One `--add`: a file or directory on the host and where it goes in the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Add {
    pub source: PathBuf,
    /// An absolute path in the image, as given; a trailing `/` puts a file inside it.
    pub dest: String,
}

/// Read `SRC:DEST`. The split is at the last `:/`, so a Windows source such as `C:\app\src:/app` keeps its drive.
pub fn parse_add(value: &str) -> Result<Add, String> {
    let Some(split) = value.rfind(":/") else {
        return Err("expected SRC:DEST, with DEST an absolute path in the image such as src:/app".to_string());
    };
    let (source, dest) = (&value[..split], &value[split + 1..]);
    if source.is_empty() {
        return Err("the source is empty".to_string());
    }
    if dest.split('/').any(|part| part == "..") {
        return Err(format!("`{dest}` leaves the directory it names"));
    }
    Ok(Add {
        source: PathBuf::from(source),
        dest: dest.to_string(),
    })
}

/// What one path in the application layer is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Dir,
    File {
        source: PathBuf,
        size: u64,
        executable: bool,
    },
    Symlink {
        target: String,
    },
}

/// Everything the `--add`s put in the layer, by path in the image without the leading `/`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct App {
    pub items: BTreeMap<String, Item>,
    /// The destinations, in the order given: the layer's history line names these, never a host path.
    pub dests: Vec<String>,
}

impl App {
    pub fn files(&self) -> usize {
        self.items
            .values()
            .filter(|item| matches!(item, Item::File { .. }))
            .count()
    }

    pub fn size(&self) -> u64 {
        self.items
            .values()
            .map(|item| match item {
                Item::File { size, .. } => *size,
                _ => 0,
            })
            .sum()
    }

    /// The layer's history line.
    pub fn created_by(&self) -> String {
        format!("pixi-oci add: {}", self.dests.join(", "))
    }
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum AddError {
    #[error("--add {}: {} does not exist", .add, .path.display())]
    Missing { add: String, path: PathBuf },

    #[error("--add {add} adds nothing: every file in it is ignored")]
    #[diagnostic(help("check .gitignore, .dockerignore and .pixi-oci-ignore; a file named by itself is always added"))]
    Empty { add: String },

    #[error("--add {}: {} is not UTF-8", .add, .path.display())]
    NotUtf8 { add: String, path: PathBuf },

    #[error("could not read {}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("could not read the ignore rules for {}: {message}", .path.display())]
    Ignore { path: PathBuf, message: String },
}

/// Collect every `add` in order, a later one replacing what an earlier one put at the same path. `workspace` is
/// where `.dockerignore` is read from.
pub fn collect(adds: &[Add], workspace: Option<&Path>) -> Result<App, AddError> {
    let dockerignore = workspace.map(dockerignore).transpose()?.flatten().map(Arc::new);
    let mut app = App::default();
    for add in adds {
        let shown = format!("{}:{}", add.source.display(), add.dest);
        let metadata = fs::symlink_metadata(&add.source).map_err(|_| AddError::Missing {
            add: shown.clone(),
            path: add.source.clone(),
        })?;
        let dest = add.dest.trim_matches('/').to_string();
        if metadata.is_dir() {
            if walk(&add.source, &dest, &shown, dockerignore.clone(), &mut app.items)? == 0 {
                return Err(AddError::Empty { add: shown });
            }
        } else {
            let path = if add.dest.ends_with('/') {
                let name = add
                    .source
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| AddError::NotUtf8 {
                        add: shown.clone(),
                        path: add.source.clone(),
                    })?;
                join(&dest, name)
            } else {
                dest.clone()
            };
            app.items.insert(path, item(&add.source, &metadata)?);
        }
        app.dests.push(add.dest.clone());
    }
    Ok(app)
}

/// Write `app` as one layer into `blobs_dir`.
pub fn write_layer(app: &App, blobs_dir: &Path, mtime: u64) -> io::Result<Blob> {
    let mut writer = Writer::new(blobs_dir, mtime)?;
    for (path, item) in &app.items {
        match item {
            Item::Dir => writer.dir(path)?,
            Item::File {
                source,
                size,
                executable,
            } => {
                let file = fs::File::open(source)?;
                writer.file(path, *executable, *size, file.take(*size))?;
            }
            Item::Symlink { target } => writer.symlink(path, Path::new(target))?,
        }
    }
    writer.finish()
}

fn join(dest: &str, rest: &str) -> String {
    if dest.is_empty() {
        rest.to_string()
    } else if rest.is_empty() {
        dest.to_string()
    } else {
        format!("{dest}/{rest}")
    }
}

fn item(path: &Path, metadata: &fs::Metadata) -> Result<Item, AddError> {
    let read = |source| AddError::Read {
        path: path.to_path_buf(),
        source,
    };
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(read)?;
        return Ok(Item::Symlink {
            target: target.to_string_lossy().replace('\\', "/"),
        });
    }
    if metadata.is_dir() {
        return Ok(Item::Dir);
    }
    let mut start = [0u8; 4];
    let mut file = fs::File::open(path).map_err(read)?;
    let mut filled = 0;
    while filled < start.len() {
        match file.read(&mut start[filled..]).map_err(read)? {
            0 => break,
            n => filled += n,
        }
    }
    let start = &start[..filled];
    Ok(Item::File {
        source: path.to_path_buf(),
        size: metadata.len(),
        executable: start.starts_with(b"#!") || start == b"\x7fELF",
    })
}

/// The workspace's `.dockerignore`, its patterns anchored at the workspace as docker anchors them at the context.
fn dockerignore(workspace: &Path) -> Result<Option<(PathBuf, ignore::gitignore::Gitignore)>, AddError> {
    let file = workspace.join(".dockerignore");
    let Ok(text) = fs::read_to_string(&file) else {
        return Ok(None);
    };
    let root = fs::canonicalize(workspace).map_err(|source| AddError::Read {
        path: workspace.to_path_buf(),
        source,
    })?;
    let mut builder = ignore::gitignore::GitignoreBuilder::new(&root);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (negate, pattern) = match line.strip_prefix('!') {
            Some(rest) => ("!", rest),
            None => ("", line),
        };
        let pattern = pattern.trim_start_matches("./").trim_start_matches('/');
        builder
            .add_line(Some(file.clone()), &format!("{negate}/{pattern}"))
            .map_err(|err| AddError::Ignore {
                path: file.clone(),
                message: err.to_string(),
            })?;
    }
    let matcher = builder.build().map_err(|err| AddError::Ignore {
        path: file.clone(),
        message: err.to_string(),
    })?;
    Ok(Some((root, matcher)))
}

fn walk(
    source: &Path,
    dest: &str,
    shown: &str,
    dockerignore: Option<Arc<(PathBuf, ignore::gitignore::Gitignore)>>,
    items: &mut BTreeMap<String, Item>,
) -> Result<usize, AddError> {
    let root = fs::canonicalize(source).map_err(|source_err| AddError::Read {
        path: source.to_path_buf(),
        source: source_err,
    })?;
    let mut builder = ignore::WalkBuilder::new(&root);
    builder
        .hidden(false)
        .ignore(false)
        .git_global(false)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .parents(true)
        .follow_links(false)
        .add_custom_ignore_filename(".pixi-oci-ignore")
        .sort_by_file_path(|a, b| a.cmp(b));
    builder.filter_entry(move |entry| {
        let name = entry.file_name();
        if entry.depth() > 0 && (name == ".git" || name == ".pixi") {
            return false;
        }
        if let Some(ignore) = &dockerignore {
            let (base, matcher) = ignore.as_ref();
            let path = entry.path();
            if path != base && path.starts_with(base) {
                let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
                return !matcher.matched_path_or_any_parents(path, is_dir).is_ignore();
            }
        }
        true
    });
    let mut added = 0;
    for entry in builder.build() {
        let entry = entry.map_err(|err| AddError::Ignore {
            path: source.to_path_buf(),
            message: err.to_string(),
        })?;
        let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        let mut parts = Vec::new();
        for part in relative.components() {
            parts.push(part.as_os_str().to_str().ok_or_else(|| AddError::NotUtf8 {
                add: shown.to_string(),
                path: entry.path().to_path_buf(),
            })?);
        }
        let path = join(dest, &parts.join("/"));
        if path.is_empty() {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|source| AddError::Read {
            path: entry.path().to_path_buf(),
            source,
        })?;
        let item = item(entry.path(), &metadata)?;
        if item != Item::Dir {
            added += 1;
        }
        items.insert(path, item);
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, content: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn add(source: &Path, dest: &str) -> Add {
        Add {
            source: source.to_path_buf(),
            dest: dest.to_string(),
        }
    }

    #[test]
    fn sources_and_destinations_are_read() {
        assert_eq!(
            parse_add("src:/app").unwrap(),
            Add {
                source: "src".into(),
                dest: "/app".into()
            }
        );
        let windows = parse_add(r"C:\work\api\src:/app/").unwrap();
        assert_eq!(windows.source, PathBuf::from(r"C:\work\api\src"));
        assert_eq!(windows.dest, "/app/");
        assert_eq!(parse_add("C:/work/src:/").unwrap().dest, "/");
        for bad in ["src", "src:app", ":/app", "src:/app/../etc"] {
            assert!(parse_add(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_directory_follows_every_ignore_file() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        write(workspace, ".gitignore", "*.log\nbuild/\n");
        write(
            workspace,
            ".dockerignore",
            "# docker's\nsrc/secrets.env\nsrc/*.tmp\n!src/keep.tmp\n",
        );
        write(workspace, "src/.pixi-oci-ignore", "notebooks/\n");
        write(workspace, "src/main.py", "print('hi')\n");
        write(workspace, "src/run.sh", "#!/bin/sh\necho run\n");
        write(workspace, "src/debug.log", "noise");
        write(workspace, "src/build/out.txt", "built");
        write(workspace, "src/secrets.env", "TOKEN=x");
        write(workspace, "src/scratch.tmp", "x");
        write(workspace, "src/keep.tmp", "x");
        write(workspace, "src/notebooks/a.ipynb", "{}");
        write(workspace, "src/pkg/.gitignore", "local.txt\n");
        write(workspace, "src/pkg/local.txt", "x");
        write(workspace, "src/pkg/mod.py", "x = 1\n");
        write(workspace, "src/.git/config", "[core]");
        write(workspace, "src/.pixi/envs/x", "x");
        write(workspace, "src/.env.example", "X=");

        let app = collect(&[add(&workspace.join("src"), "/app")], Some(workspace)).unwrap();
        let paths: Vec<&str> = app.items.keys().map(String::as_str).collect();
        assert_eq!(
            paths,
            [
                "app",
                "app/.env.example",
                "app/.pixi-oci-ignore",
                "app/keep.tmp",
                "app/main.py",
                "app/pkg",
                "app/pkg/.gitignore",
                "app/pkg/mod.py",
                "app/run.sh",
            ]
        );
        assert!(matches!(app.items["app/run.sh"], Item::File { executable: true, .. }));
        assert!(matches!(
            app.items["app/main.py"],
            Item::File {
                executable: false,
                size: 12,
                ..
            }
        ));
        assert_eq!(app.created_by(), "pixi-oci add: /app");
        assert_eq!(app.files(), 7);
    }

    #[test]
    fn a_file_goes_where_it_is_told_and_a_later_add_wins() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "*.toml\n");
        write(dir.path(), "conf/api.toml", "port = 1\n");
        write(dir.path(), "conf/other.toml", "port = 2\n");
        write(dir.path(), "tools/tool", "\x7fELF....");
        let app = collect(
            &[
                // A file named by itself is added though .gitignore matches it.
                add(&dir.path().join("conf/api.toml"), "/etc/api/"),
                add(&dir.path().join("conf/api.toml"), "/etc/api.toml"),
                add(&dir.path().join("tools/tool"), "/usr/local/bin/tool"),
                add(&dir.path().join("conf/other.toml"), "/etc/api.toml"),
            ],
            None,
        )
        .unwrap();
        let paths: Vec<&str> = app.items.keys().map(String::as_str).collect();
        assert_eq!(paths, ["etc/api.toml", "etc/api/api.toml", "usr/local/bin/tool"]);
        assert!(matches!(&app.items["etc/api.toml"], Item::File { source, .. } if source.ends_with("other.toml")));
        assert!(matches!(
            app.items["usr/local/bin/tool"],
            Item::File { executable: true, .. }
        ));
        assert_eq!(
            app.created_by(),
            "pixi-oci add: /etc/api/, /etc/api.toml, /usr/local/bin/tool, /etc/api.toml"
        );
    }

    #[test]
    fn nothing_to_add_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/.gitignore", "*\n");
        write(dir.path(), "src/a.py", "x");
        let err = collect(&[add(&dir.path().join("src"), "/app")], None).unwrap_err();
        assert!(matches!(err, AddError::Empty { .. }), "{err}");
        let err = collect(&[add(&dir.path().join("absent"), "/app")], None).unwrap_err();
        assert!(matches!(err, AddError::Missing { .. }), "{err}");
    }

    #[test]
    fn the_layer_holds_content_only() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/main.py", "print('hi')\n");
        write(dir.path(), "src/run.sh", "#!/bin/sh\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The host's bits are not read: an executable bit on a plain file stays off.
            fs::set_permissions(dir.path().join("src/main.py"), fs::Permissions::from_mode(0o775)).unwrap();
            std::os::unix::fs::symlink("main.py", dir.path().join("src/link.py")).unwrap();
        }
        let app = collect(&[add(&dir.path().join("src"), "/srv/app")], None).unwrap();
        let first = write_layer(&app, &dir.path().join("blobs"), 0).unwrap();
        let again = write_layer(&app, &dir.path().join("blobs2"), 0).unwrap();
        assert_eq!(first, again);

        let blob = dir
            .path()
            .join("blobs")
            .join(first.digest.trim_start_matches("sha256:"));
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(blob).unwrap()));
        let mut seen = BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let header = entry.header();
            assert_eq!(
                (header.uid().unwrap(), header.gid().unwrap(), header.mtime().unwrap()),
                (0, 0, 0)
            );
            let link = entry.link_name().unwrap().map(|l| l.to_string_lossy().into_owned());
            seen.insert(
                entry.path().unwrap().to_string_lossy().into_owned(),
                (header.mode().unwrap(), link),
            );
        }
        assert_eq!(seen["srv/"].0, 0o755);
        assert_eq!(seen["srv/app/main.py"].0, 0o644);
        assert_eq!(seen["srv/app/run.sh"].0, 0o755);
        if cfg!(unix) {
            assert_eq!(seen["srv/app/link.py"].1.as_deref(), Some("main.py"));
        }
    }
}

# How it works

```text
pixi.lock ─▶ plan ─▶ fetch ─▶ stream each archive ─▶ tar ─▶ gzip ─▶ blob    (one per layer, several at once)
                       │       rewrite paths, place                         │
                       │       noarch python, add                           ▼
                cache of archives   conda-meta            config ─▶ manifest ─▶ OCI layout, or a .tar of it
                named by sha256
```

## Planning the layers

`plan` reads one environment of `pixi.lock` for one platform and decides the layers.

**By default the environment is one layer.** The image is the base image's layers and one layer holding the whole
environment, conda and PyPI; the application's own layer will go on top
([#40](https://github.com/millsks/pixi-oci/issues/40)). Layers are named by their sha256, so an unchanged
environment keeps its digest from build to build and is neither uploaded nor pulled again. When `pixi.lock` changes, the whole environment layer is uploaded once more,
which for most environments is a few hundred megabytes at most.

**`--layers grouped` splits a large environment.** When the environment is large and its packages change often, a
package upgrade that re-uploads gigabytes costs more than extra layers do. Grouping re-uploads only the layers that
hold the changed packages:

1. **Large packages alone, small ones grouped.** A conda package of 4 MiB or more gets a layer of its own: those are
   the ones worth re-uploading alone. Smaller ones are grouped, from about 1 MiB to 8 MiB a group, so a few hundred
   packages do not each spend a layer on a few kilobytes. A group ends after a package whose name hashes to a cut
   point (about one name in four) once it holds 1 MiB, or at 8 MiB whatever the names. Because the cut points belong
   to names, not positions, adding or removing a package changes only the group it lands in, and the layers above
   keep their digests. A group still under 1 MiB when a large package comes along stays open past it, rather than
   become a tiny layer of its own.
2. **Dependencies first.** The packages everything rests on (`libgcc`, `openssl`, `python`) sit at the bottom, where
   they change least often. Ties go by name, so the order is the same on every run; a dependency cycle (`python` and
   `pip`) is broken at its alphabetically first package.
3. **Capped.** Overlay file systems limit the layer count (Docker's overlay2 refuses an image of more than about
   128), so past `--max-layers` (100) the layers at the top of the order, the ones fewest others depend on, are
   merged into the last conda layer. Grouping keeps even a few hundred packages well under it: a 385-package
   data-science and Jupyter environment plans into 84 layers.
4. **PyPI on top.** All PyPI packages share the top layer: they install into one site-packages and are usually what
   changes most.

Grouped layers also keep each upload small, which helps where a proxy in front of the registry limits the size of a
request. `--layers per-package` gives every conda package a layer of its own, as long as the cap allows.
`plan --packages` lists what each shared layer holds.

Conda packages pixi builds from source have no archive yet, so an environment with them is refused, by name; so are
PyPI packages that are not wheels (sdists, git and path checkouts), which would have to be built.

## Fetching

Every archive of the plan is fetched before any layer is written, one at a time, so a failed download stops the build
early. Each is checked against the sha256 the lockfile recorded, and kept in the cache named by that hash. A file
whose bytes are not the ones locked never reaches the cache, and two lockfiles that lock the same file share it.
Local archives (a `file://` URL or a path) are read where they are, and still checked.

## Writing a layer from the archives

A layer is written straight from its packages' archives, never unpacked on the host. For each package, its
`info/index.json`, `info/paths.json` and (for `noarch: python`) `info/link.json` are read first, then its content is
streamed in the archive's own order: the `pkg-*.tar.zst` inside a `.conda`, or the whole `.tar.bz2`. Only what
`paths.json` lists is installed.

Each entry is placed as rattler, pixi's own installer, would place it, under the image's prefix (`/opt/env`):

| Entry | In the layer |
|---|---|
| A file with no placeholder | copied into the tar in 64 KiB chunks |
| A file with the build-time placeholder prefix | rewritten for the image's prefix with rattler's own replacement functions, text or binary as `paths.json` says |
| A symlink | the archive's link target, as is |
| An empty directory | a directory entry |
| A `noarch: python` file | moved from `site-packages/` to `lib/python3.X/site-packages/`, and `python-scripts/` to `bin/` |

After the content come the package's entry-point scripts (for `noarch: python`) and its `conda-meta/<name>.json`
record, built the way rattler builds it; the bottom layer also carries the environment's empty `conda-meta/history`.
Where two packages in one layer install the same path, the later one wins, as it would installing one after the
other.

A package that would place a file outside the prefix (a path with `..`, a root or a drive) is refused before any of
its layer is written.

### Checked against rattler

The test suite keeps rattler's own installer as a reference: it installs every test plan into a directory with
`rattler::install::link_package_sync`, builds the same plan the streamed way, and requires the same files, modes,
symlink targets and `conda-meta` records. It does so for generated packages that cover text and binary placeholders,
symlinks, case-only names, both archive formats, entry points and very large files, and for real conda-forge
environments. A numpy, pandas, scipy and scikit-learn environment matches too: 15,888 entries, none different.


## Installing PyPI wheels

A wheel is a zip archive, read in place and written into the layer as pip and uv install one into a conda
environment's python:

| In the wheel | In the image |
|---|---|
| its root, `.data/purelib`, `.data/platlib` | `lib/pythonX.Y/site-packages` |
| `.data/scripts` | `bin`, executable, with a `#!python` first line pointed at the environment's python |
| `.data/headers` | `include/pythonX.Y/<name>` |
| `.data/data` | the prefix |
| `console_scripts` and `gui_scripts` entry points | a launcher each in `bin`, as pip writes them |

Every file is checked against the wheel's own `RECORD` as it is written, and every path must stay inside the prefix.
The installed `RECORD` lists where each file went, relative to `site-packages`, and `INSTALLER` says `pixi-oci`.
Where a wheel installs a path a conda package of the same layer also installs, the wheel's file wins, as it would
installing the wheels after the conda packages. Bytecode is not compiled into the image; python writes it on first
import where it can. CI installs the same lockfile with `pixi install` and checks that site-packages holds the same
files with the same bytes.

## Reproducible bytes

The same lockfile gives the same image digest on any machine:

- every entry is owned by root (uid and gid 0) and stamped with one time, the Unix epoch, so a layer's digest
  depends only on its packages: a build at a later commit, or with `SOURCE_DATE_EPOCH` set to the commit's time,
  reuses every layer whose packages did not change
- every mode is 0755 (directories, symlinks, executables) or 0644: only whether a file is executable is kept, so no
  umask reaches the image
- entries keep the archives' order, which the lockfile's hashes fix
- the gzip header carries no time and no file name
- the config's `created` time, and its history's, is `SOURCE_DATE_EPOCH`, or 0; it changes the config's digest,
  never a layer's

Layers are tar archives in GNU format, compressed with gzip, so every container runtime and registry reads them. CI
builds a test image on Linux, macOS and Windows on every change and fails if the digests differ; `pixi oci validate`,
`crane validate`, `umoci unpack` and `skopeo copy` all accept the result.

## Memory and disk

"In memory" means one entry at a time, not the whole image. Entries flow from the archive through the tar and gzip
writers to a blob on disk:

- A file without the placeholder passes through in 64 KiB chunks, however large.
- A file with the placeholder is held whole while it is rewritten, up to a limit: an eighth of `--max-memory`,
  between 1 and 16 MiB.
- A larger one is copied to a temporary file and rewritten in pieces of up to 4 MiB, each cut where no replacement can
  cross: right after a NUL byte in a binary file (a placeholder sits in a C string, which a NUL ends), right after a
  newline in a text file (but not before a line starting `#!`). The bytes are the same as a whole-file rewrite, and
  the tests check it.

`--max-memory` (512M by default) also sets how many layers are written at once: as many as fit, allowing for the
in-memory limit twice per layer and about 16 MiB of decompression and compression buffers, and no more than the
machine has cores. It never changes the image.

Measured with a warm cache on 2 cores:

| linux-64 image | Packages | Unpacked | Time | Peak memory |
|---|---|---|---|---|
| python and six | 23 | 317 MiB | 2.9 s | 48 MiB; 28 MiB with `--max-memory 16M` |
| python, numpy, pandas, scipy, scikit-learn | 39 | 460 MiB | 5.3 s | 45 MiB |

On disk, plan for the cache (the compressed archives, about the size of the download), the output (the layers,
somewhat larger, since gzip compresses less than conda's zstd), and while a `.tar` is assembled, the layers twice.

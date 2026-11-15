# pixi-oci

A [pixi](https://pixi.sh) extension that builds OCI container images straight from `pixi.lock`: no Dockerfile, no
Docker daemon, and no pixi inside the image.

> **Status: early.** `pixi oci plan` and `pixi oci build` work today for environments of conda packages, on Linux,
> macOS and Windows. Images
> have no base yet, so a dynamically linked program (python, most of conda-forge) needs the glibc a base image
> provides; base images and pushing are the next milestone. See the [roadmap](#roadmap).

## Why

The usual way to containerize a pixi environment is a multi-stage Dockerfile that installs pixi, runs
`pixi install`, and copies the environment into a slimmer stage. That needs a Docker daemon (or a privileged
builder), downloads everything again on every cache miss, and produces an image whose layers say nothing about which
packages they hold.

The lockfile already names every package, its exact URL and its hash. pixi-oci builds the image from that:

- **Daemonless.** It writes the image layers itself, so it runs on a plain CI runner or an OpenShift build pod.
- **Reproducible.** The same lockfile gives the same layers and the same image digest.
- **Cache-friendly.** Large packages get layers of their own and small ones are grouped, dependencies at the bottom,
  so changing one package re-uploads one small layer, and images built from the same packages share layers in the
  registry.
- **Private-channel ready.** Channels and registries are whatever your lockfile and your credentials point at.

## Install

Not released yet. From a checkout:

```sh
pixi run build
cp target/release/pixi-oci ~/.local/bin/   # anywhere on PATH
pixi oci --help
```

pixi finds any `pixi-*` executable on `PATH` and runs it as a subcommand, so `pixi-oci` becomes `pixi oci`.

## Usage

### `pixi oci build`

Builds an image of an environment.

```console
$ pixi oci build --environment app --platform linux-64 --tag app:1.0 --output app.tar
[1/7] 4 conda packages (1.8 MiB)
[2/7] libpython 3.13.18 (9.3 MiB)
...
[7/7] six 1.17.0 (18.0 KiB)
Wrote app:1.0 to app.tar
  manifest sha256:9ec8407e4f1860919cd8dd7af6027a9bdcb91a5a008b2c5ea657ed3f300044ef
  7 layers, 62.6 MiB for linux/amd64

$ pixi oci validate app.tar
app.tar is a valid OCI image
  manifest sha256:9ec8407e4f18 for linux/amd64: 7 layers, 62.6 MiB

$ podman load -i app.tar
```

Each package is downloaded once and checked against the sha256 the lockfile recorded. Its layer is then written
straight from the archive, never unpacked on the host: entries are read one at a time, paths are rewritten for
where the environment lives in the image (`/opt/env` by default) with rattler's own replacement functions,
`noarch: python` packages go under `lib/python3.X/site-packages` with their entry points, and a `conda-meta`
record is added. The image sets `PATH` and `CONDA_PREFIX` to the prefix.

Because the host never holds the layer's files, **it builds on Windows, macOS and Linux alike**, with no admin
rights, WSL or VM: Unix modes, symlinks and names that differ only in case go from the archive into the layer
without touching the host's file system. Tests compare the result with rattler's own installer file for file.

Builds are reproducible: every file is owned by root and stamped with one time (`SOURCE_DATE_EPOCH`, or the Unix
epoch), modes are 0755 or 0644, entries keep the archives' order, and gzip headers carry no time, so the same
lockfile gives the same image digest on any machine. CI builds the same image on Linux, macOS and Windows and
requires one digest.

#### Memory

Files pass through in 64 KiB chunks. A file whose paths are rewritten is held whole up to a limit (16 MiB by
default) and above it is rewritten in 4 MiB pieces from a temporary file, so memory stays near the piece size however
large the file. `--max-memory` sets the limit and how many layers are written at once; it never changes the image.

| linux-64 image | Packages | Unpacked | Time | Peak memory |
|---|---|---|---|---|
| python and six | 23 | 317 MiB | 2.9 s | 48 MiB (28 MiB with `--max-memory 16M`) |
| python, numpy, pandas, scipy, scikit-learn | 39 | 460 MiB | 5.3 s | 45 MiB |

Measured on 2 cores with a warm cache. The build that installed each package into a folder on disk first took 4.1 s
and 15.9 s for the same two images.

| Option | Default | |
|---|---|---|
| `--lockfile`, `-e`, `-p`, `--max-layers` | as for `plan` | which environment, and how it is layered |
| `-o, --output PATH` | `image.tar` | a `.tar` archive (for `docker load`, `podman load`, `skopeo copy oci-archive:`), or any other path for an OCI image layout directory |
| `--force` | | replace the output if it exists |
| `-t, --tag NAME[:TAG]` | the environment's name, `:latest` | the image's name |
| `--prefix PATH` | `/opt/env` | where the environment lives in the image |
| `--env KEY=VALUE` | | an environment variable; repeatable |
| `--entrypoint`, `--cmd` | | a JSON array (`["python", "-m", "http.server"]`) or words |
| `--workdir PATH` | | the working directory |
| `--max-memory SIZE` | `512M` | how much memory the build's buffers may use, such as `128M` or `2G`; at least `16M` |
| `--cache-dir PATH` | `PIXI_OCI_CACHE_DIR`, else the platform's cache | downloaded package archives, named by their sha256 |

Not yet: PyPI packages (an environment with them is refused, by name), conda packages built from source, and
private channels that need credentials.

### `pixi oci plan`

Shows the layers an image of an environment would be built from, without downloading anything.

```console
$ pixi oci plan --environment default --platform linux-64
Environment default on linux-64 (image platform linux/amd64)
22 conda packages, 1 PyPI package, 7 layers, download size unknown
3 conda packages in layers of their own, 19 sharing 3 layers

  #  Layer              Download
  1  4 conda packages   1.8 MiB
  2  libpython 3.13.18  9.3 MiB
  3  12 conda packages  5.9 MiB
  4  tk 8.6.13          3.4 MiB
  5  3 conda packages   1.4 MiB
  6  python 3.13.18     23.2 MiB
  7  1 PyPI package     unknown
```

| Option | Default | |
|---|---|---|
| `--lockfile PATH` | `pixi.lock` | the lockfile to read |
| `-e, --environment NAME` | `default` | the environment to put in the image |
| `-p, --platform NAME` | Linux on the host's architecture | a `linux-*` conda platform |
| `--layers grouped\|per-package\|single` | `grouped` | large packages alone and small ones grouped, a layer per package, or one layer |
| `--max-layers N` | `100` | past it, the layers of the packages fewest others depend on are merged into the last |
| `--packages` | | list the packages of every layer that holds several |
| `--format text\|json` | `text` | JSON carries every package's URL, hash and size |

Sizes are of the compressed archives the lockfile records, which is what gets downloaded. The lockfile does not record
wheel sizes, so a plan with PyPI packages has no total.

### `pixi oci validate`

Checks an image against the OCI image spec, running nothing: every blob present with the digest and size its
descriptor names, every layer's uncompressed tar matching its `diff_id`, media types, schema versions, the platform,
and for an archive the `manifest.json` `docker load` reads. It works on a layout directory or a `.tar`, on any OS, and
reports every problem it finds before exiting `1`.

```console
$ pixi oci validate app.tar --format json
```

## How layers are planned

1. A conda package of 4 MiB or more gets its own layer; smaller ones are grouped, about 1 to 8 MiB a group. A group
   ends after a package whose name hashes to a cut point, so adding or removing a package changes only its own group.
   `--layers per-package` gives every package a layer, and `--layers single` puts the environment in one.
2. Layers are ordered dependencies first, ties broken by name, so `libgcc` and `openssl` sit under `python` and the
   order is the same on every run. Dependency cycles (`python` and `pip`) are broken at their alphabetically first
   package.
3. Overlay filesystems cap the layer count (Docker's overlay2 refuses more than 128), so past `--max-layers` the
   layers at the top of the order are merged into one.
4. All PyPI packages share the top layer: they install into the same site-packages and are usually what changes most.

Conda packages pixi builds from source have no archive to extract yet, so environments with them are refused with
their names.

## Roadmap

Every item has an issue labelled [`roadmap`](https://github.com/millsks/pixi-oci/issues?q=label%3Aroadmap); the
[roadmap page](https://millsks.github.io/pixi-oci/dev/roadmap/) has the detail.

- [x] `plan` ([#6](https://github.com/millsks/pixi-oci/issues/6)) and `build`
      ([#7](https://github.com/millsks/pixi-oci/issues/7))
- [x] Layers streamed from the archives, so builds run on Windows, within `--max-memory`
      ([#8](https://github.com/millsks/pixi-oci/issues/8))
- [x] Documentation site ([#9](https://github.com/millsks/pixi-oci/issues/9))
- [x] Grouped layers and `--layers` ([#10](https://github.com/millsks/pixi-oci/issues/10))
- [x] `validate`: an image checked against the OCI spec ([#25](https://github.com/millsks/pixi-oci/issues/25))
- [ ] `inspect`: an image's config, layers and packages ([#24](https://github.com/millsks/pixi-oci/issues/24))
- [ ] `check`: static image checks on any OS ([#11](https://github.com/millsks/pixi-oci/issues/11))
- [ ] Base images with `--base` ([#12](https://github.com/millsks/pixi-oci/issues/12))
- [ ] `push` to any registry ([#13](https://github.com/millsks/pixi-oci/issues/13))
- [ ] `run`: rootless containers on Linux and in CI ([#14](https://github.com/millsks/pixi-oci/issues/14))
- [ ] `diff`: two images compared layer by layer ([#26](https://github.com/millsks/pixi-oci/issues/26))
- [ ] Examples of real pixi workspaces, small to large ([#22](https://github.com/millsks/pixi-oci/issues/22))
- [ ] Multi-platform image indexes ([#15](https://github.com/millsks/pixi-oci/issues/15))
- [ ] pixi's activation variables and an entrypoint from a task
      ([#16](https://github.com/millsks/pixi-oci/issues/16))
- [ ] PyPI wheels in the top layer ([#17](https://github.com/millsks/pixi-oci/issues/17))
- [ ] Private channels with pixi's credentials ([#18](https://github.com/millsks/pixi-oci/issues/18))
- [ ] A [pixi-sbom](https://github.com/aesir-oss/pixi-sbom) SBOM attached, and
      [pixi-policy](https://github.com/aesir-oss/pixi-policy) before pushing
      ([#19](https://github.com/millsks/pixi-oci/issues/19))
- [ ] A release and a conda-forge package ([#20](https://github.com/millsks/pixi-oci/issues/20))

## Related projects

- [conda-docker](https://github.com/conda-incubator/conda-docker) built images from conda environments without
  Docker, and is this project's closest ancestor.
- [apko](https://github.com/chainguard-dev/apko) and [ko](https://github.com/ko-build/ko) do the same for Alpine
  packages and Go binaries; pixi-oci aims to be that for `pixi.lock`.
- [Pixitainer](https://github.com/RaphaelRibes/pixitainer) (`pixi containerize`) builds Apptainer, Singularity and
  Docker images through those tools.

## Development

Everything runs through pixi:

| | |
|---|---|
| `pixi run fmt`, `pixi run lint`, `pixi run check` | format, clippy with `-D warnings`, check |
| `pixi run test` | unit and CLI tests; `PIXI_OCI_NETWORK_TESTS=1` adds a real build from conda-forge (run on Linux) and parity with rattler on conda-forge packages |
| `pixi run cov` | coverage, failing under 90% of lines |
| `pixi run -e lint deny` | cargo-deny: advisories, licenses, sources |
| `pixi run -e msrv msrv-check` | builds on the minimum supported Rust |
| `pixi run ci` | everything CI runs |
| `pixi run fixture` | re-lock the test fixture in `tests/fixtures/workspace` |

## License

Apache-2.0

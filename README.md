# pixi-oci

A [pixi](https://pixi.sh) extension that builds OCI container images straight from `pixi.lock`: no Dockerfile, no
Docker daemon, and no pixi inside the image.

> **Status: early.** `pixi oci plan` works today. Building and pushing images is the next milestone; see the
> [roadmap](#roadmap).

## Why

The usual way to containerize a pixi environment is a multi-stage Dockerfile that installs pixi, runs
`pixi install`, and copies the environment into a slimmer stage. That needs a Docker daemon (or a privileged
builder), downloads everything again on every cache miss, and produces an image whose layers say nothing about which
packages they hold.

The lockfile already names every package, its exact URL and its hash. pixi-oci builds the image from that:

- **Daemonless.** It writes the image layers itself, so it runs on a plain CI runner or an OpenShift build pod.
- **Reproducible.** The same lockfile gives the same layers and the same image digest.
- **Cache-friendly.** One layer per conda package, dependencies at the bottom, so changing one package re-uploads
  one layer, and images built from the same packages share layers in the registry.
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

### `pixi oci plan`

Shows the layers an image of an environment would be built from, without downloading anything.

```console
$ pixi oci plan --environment default --platform linux-64
Environment default on linux-64 (image platform linux/amd64)
22 conda packages, 1 PyPI package, 23 layers, download size unknown

   #  Layer                       Download
   1  ca-certificates 2026.10.23  127.0 KiB
   2  libgomp 16.2.0              625.6 KiB
   3  _openmp_mutex 4.5           28.3 KiB
   4  libgcc 16.2.0               1.0 MiB
   ...
  22  python 3.13.18              23.2 MiB
  23  1 PyPI package              unknown
```

| Option | Default | |
|---|---|---|
| `--lockfile PATH` | `pixi.lock` | the lockfile to read |
| `-e, --environment NAME` | `default` | the environment to put in the image |
| `-p, --platform NAME` | Linux on the host's architecture | a `linux-*` conda platform |
| `--max-layers N` | `100` | past it, the packages fewest others depend on share the last conda layer |
| `--format text\|json` | `text` | JSON carries every package's URL, hash and size |

Sizes are of the compressed archives the lockfile records, which is what gets downloaded. The lockfile does not record
wheel sizes, so a plan with PyPI packages has no total.

## How layers are planned

1. Each conda package gets its own layer.
2. Layers are ordered dependencies first, ties broken by name, so `libgcc` and `openssl` sit under `python` and the
   order is the same on every run. Dependency cycles (`python` and `pip`) are broken at their alphabetically first
   package.
3. Overlay filesystems cap the layer count (Docker's overlay2 refuses more than 128), so past `--max-layers` the
   packages at the top of the order share one layer.
4. All PyPI packages share the top layer: they install into the same site-packages and are usually what changes most.

Conda packages pixi builds from source have no archive to extract yet, so environments with them are refused with
their names.

## Roadmap

- [x] `plan`: read `pixi.lock`, map conda platforms to OCI platforms, order and cap layers
- [ ] `build`: download and verify packages against the lockfile's hashes, extract each into a layer, and write an
      OCI image layout
- [ ] Image config from pixi's activation: environment variables, `PATH`, entrypoint from a pixi task
- [ ] PyPI packages: install wheels into the top layer
- [ ] `push`: to any OCI registry, with the usual credential helpers
- [ ] Base images (`--base`), and `scratch` by default
- [ ] Multi-platform image indexes
- [ ] Attach a [pixi-sbom](https://github.com/aesir-oss/pixi-sbom) SBOM to the image, and gate on
      [pixi-policy](https://github.com/aesir-oss/pixi-policy) before pushing
- [ ] Packaging on conda-forge

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
| `pixi run test` | unit and CLI tests |
| `pixi run cov` | coverage, failing under 90% of lines |
| `pixi run -e lint deny` | cargo-deny: advisories, licenses, sources |
| `pixi run -e msrv msrv-check` | builds on the minimum supported Rust |
| `pixi run ci` | everything CI runs |
| `pixi run fixture` | re-lock the test fixture in `tests/fixtures/workspace` |

## License

Apache-2.0

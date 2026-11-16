# Architecture

pixi-oci is one Rust binary, organized as a short pipeline. Each stage has one module and one job.

```text
 pixi.lock ─▶ lock.rs ─▶ plan.rs ─▶ build.rs ─┬─▶ fetch.rs ─▶ cache: archives/<sha256>.conda
                │           │          │      │
                │           │          │      └─▶ stream.rs ─▶ layer.rs ─▶ blobs/sha256/<digest>   (per layer, in parallel)
                │           │          │            ▲
                │           │          │            └── target.rs: the prefix, the platform, python
                │           │          └──────────▶ image.rs ─▶ config, manifest, index.json, .tar
                │           └─ platform.rs: conda platform to OCI platform
                └─ report.rs: a plan as text or JSON          cli.rs and lib.rs: the command line around it
```

| Module | Job |
|---|---|
| `cli.rs` | The command line (clap). pixi runs `pixi-oci` for `pixi oci` and passes everything after `oci`. |
| `lib.rs` | Runs a parsed command line and returns what to print. |
| `lock.rs` | Reads `pixi.lock` (rattler_lock) into the packages of one environment and platform, each with its URL, sha256, size, dependencies and repodata record. |
| `plan.rs` | Splits those packages into layers as `--layers` says: large packages alone and small ones grouped at cut points their names fix (the default), one per package, or one for everything; dependencies first with ties by name, capped by `--max-layers`, PyPI on top. |
| `platform.rs` | Maps `linux-*` conda platforms to OCI platforms (`linux-aarch64` to `linux/arm64/v8`), and picks the default. |
| `report.rs` | A plan as a table or JSON. |
| `build.rs` | `build`: checks the request, fetches every archive, writes the planned layers in parallel within `--max-memory`, puts a pulled base's layers underneath, merges its config, then writes the image. |
| `sources.rs` | pixi's config as pixi reads it (`mirrors`, `s3-options`, through rattler_config), each location's mirrors longest match first, and a reqwest client with rattler's S3, GCS and OCI middleware for those schemes. |
| `fetch.rs` | Downloads conda archives and wheels (ureq) with pixi's credentials for each host, applied as rattler applies them and kept out of errors; checks each against the lockfile's sha256, and keeps it in a cache named by that hash. |
| `stream.rs` | Writes one layer straight from its packages' archives: placeholder replacement, `noarch: python` placement and entry points, `conda-meta` records, the prefix guard, and the piecewise rewrite of large files. |
| `target.rs` | The install target: the prefix in the image, the platform, and python for noarch packages. |
| `layer.rs` | The layer writer: a GNU tar, gzip-compressed, every entry root-owned with one time and one of three modes, hashed twice on the way (diff_id and digest). |
| `image.rs` | The image config, manifest, OCI layout, docker-compatible `manifest.json`, and the `.tar` archive. |
| `check.rs` | `check`: the layers applied into a map of facts (modes, ELF dynamic sections, shebangs, placeholders), then the loader's and the shell's lookups run inside it. |
| `inspect.rs` | `inspect`: an image's settings and layers, with each layer's packages read from its history line. |
| `registry.rs` | A base image pulled from a registry, pinned by digest: the platform's manifest from an index, its config and layers, each checked against its digest and cached by it. Anonymous or with docker's and podman's credentials, through bearer tokens or basic auth. |
| `wheel.rs` | A PyPI wheel installed into a layer as pip and uv install one: site-packages, scripts, headers, data, entry point launchers, every file checked against the wheel's RECORD and a new RECORD written. |
| `digest.rs` | `digest`: a tag resolved to its digest, or a platform's manifest digest, from manifests (and a config) alone. |
| `pull.rs` | `pull`: a tag resolved to its digest, the platform's image fetched through the blob cache, written as a layout or `.tar` with the registry's digest. |
| `push.rs` | `push`: validate, then each blob the registry lacks uploaded (or mounted from the base's repository), the manifest last, its digest checked. |
| `read.rs` | An image read back: a layout directory, or a `.tar` read in place by each file's offset, never unpacked. |
| `validate.rs` | `validate`: every blob, digest, size, media type and `diff_id` of an image checked against the OCI image spec, with every problem collected. |

## Decisions that shape the code

**Nothing of a layer touches the host.** `stream.rs` reads archive entries and writes them into the layer's tar
directly; the only files a build writes are archives, blobs and spill files, all named by hashes. This is what makes
builds work on Windows, and it keeps a crafted archive's member names from ever reaching the host's file system.

**Install like rattler.** `stream.rs` follows `rattler::install::link_package_sync` step for step and uses rattler's
public functions where they exist (placeholder replacement, noarch paths, the entry-point template, `PrefixRecord`).
`install.rs` keeps rattler's own linker as a test-only reference, and `parity.rs` holds the two to the same output.

**Reproducible bytes.** Anything that would bring a time, a host path, a user name or an unordered collection into a
layer or the config would change image digests. Entries keep the archives' order, modes are normalized, times come
from `SOURCE_DATE_EPOCH`, and the gzip header is empty.

**Bounded memory.** A file is held whole only up to the in-memory limit; larger rewrites go through a temporary file
in pieces cut where no replacement can cross. `build::budget` turns `--max-memory` into that limit and a number of
parallel layers.

**One rattler release set.** `rattler`, `rattler_conda_types`, `rattler_lock`, `rattler_package_streaming` and
`rattler_digest` are pinned exactly to one set, because they pin each other exactly; they move together.

## Tests

| Where | What |
|---|---|
| Unit tests in each module | the module's own rules: planning, platforms, tar determinism, the budget, cut points, errors |
| `src/parity.rs` (Unix) | every test plan built the streamed way and through rattler's linker, file for file; generated packages always, conda-forge's with `PIXI_OCI_NETWORK_TESTS=1` |
| `src/testing.rs` | conda packages made in tests (both archive formats, placeholders, symlinks, noarch python), images read back |
| `tests/cli.rs` | the binary end to end against `tests/fixtures/workspace/pixi.lock` |
| `tests/network.rs` (Linux, `PIXI_OCI_NETWORK_TESTS=1`) | a real image from conda-forge, unpacked, and its python run |
| CI's **Image digest** jobs | the fixture image built on Linux, macOS and Windows; one digest required |

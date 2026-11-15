# CLAUDE.md

Project guidance for millsks/pixi-oci. This project is **Rust**.

## Architecture

One crate, `pixi-oci`, a binary pixi runs as `pixi oci`: it builds OCI container images from `pixi.lock`.

| Path | What it is |
|---|---|
| `src/cli.rs` | the command line (clap) |
| `src/lock.rs` | reads `pixi.lock` (rattler_lock) into the packages of one environment and platform |
| `src/plan.rs` | splits those packages into image layers: one per conda package, dependencies first, capped |
| `src/platform.rs` | conda platform to OCI platform |
| `src/report.rs` | a plan as text or JSON |
| `src/build.rs` | `build`: fetch every archive, then write the planned layers in parallel within `--max-memory` |
| `src/fetch.rs` | downloads conda archives, checks the lockfile's sha256, keeps them in a cache keyed by it |
| `src/stream.rs` | a layer written straight from package archives: prefix replacement, noarch python, conda-meta |
| `src/target.rs` | the install target: prefix in the image, platform, python |
| `src/layer.rs` | the reproducible tar+gzip layer writer, with its digest and diff_id |
| `src/install.rs` | test only: rattler's own linker into a directory, the reference the streamed build is checked against |
| `src/parity.rs` | test only, Unix: every plan built both ways must give the same files, modes, links and records |
| `src/testing.rs` | test only: conda packages made in tests, images read back |
| `src/image.rs` | config, manifest, OCI layout and archive |
| `tests/cli.rs` | the binary end to end, against `tests/fixtures/workspace/pixi.lock` |
| `tests/network.rs` | a real build from conda-forge, unpacked and run; only with `PIXI_OCI_NETWORK_TESTS=1` on Linux |

## Commands

Everything runs through pixi.

| Task | Command |
|---|---|
| Format, lint, check | `pixi run fmt`, `pixi run lint` (clippy `-D warnings`), `pixi run check` |
| Tests | `pixi run test`; one test: `pixi run cargo test --test cli <name>` |
| Full gate | `pixi run ci` (pre-commit, build, check, clippy, coverage ≥ 90%) |
| Dependencies | `pixi run -e lint deny` |

## Workflow

- Feature branch and pull request for every change; work is not finished until the PR's checks pass.
- Conventional commit messages (enforced by the commit-msg hook).

## Off-limits and traps

- `tests/fixtures/workspace/pixi.lock` is written by pixi: regenerate it with `pixi run fixture`, never edit it.
  Tests assert on its package counts, so re-locking can mean updating them.
- The four rattler crates (`rattler`, `rattler_conda_types`, `rattler_lock`, `rattler_package_streaming`) are pinned
  exactly (`=`) to one release set, because each pins the others exactly; bump all four together. They are on the
  `rattler_conda_types` 0.58 line, ahead of aesir-oss/pixi-sbom's 0.53, because `rattler_package_streaming` 0.27
  pulls `zip` 8 with open advisories.
- `stream.rs` follows `rattler::install::link_package_sync`; when rattler changes how it installs, the parity
  tests fail first. Match rattler rather than editing the tests, and keep the reference install in `install.rs`.
- Nothing in a build may create a layer's files on the host: that is what makes Windows work. Only archives,
  blobs and spill files (all named by hashes) touch the host's file system.
- Reproducibility is a feature: anything that adds a time, a host path, a user name or an unordered collection to
  a layer or to the config changes image digests. `layer.rs` and `image.rs` tests check it.

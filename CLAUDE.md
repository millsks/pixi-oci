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
| `tests/cli.rs` | the binary end to end, against `tests/fixtures/workspace/pixi.lock` |

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
- Keep rattler_lock and rattler_conda_types on the same versions as aesir-oss/pixi-sbom where practical, so both
  extensions read the same lockfile versions.

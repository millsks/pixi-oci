# Development guide

This page is how the code is built, checked, tested and documented. For what the code does, see
[the architecture page](architecture.md).

## Toolchain

Everything runs through [pixi](https://pixi.sh). `pixi.toml` pins the Rust toolchain from conda-forge and defines
every task; `Cargo.toml` describes the crate. You do not need rustup or a system Rust.

```sh
git clone https://github.com/millsks/pixi-oci
cd pixi-oci
pixi install          # rust, cargo-llvm-cov, pre-commit, taplo, typos, actionlint, ...
pixi run bootstrap    # installs the git pre-commit and commit-msg hooks
```

| Task | Runs | Purpose |
|---|---|---|
| `fmt` | `cargo fmt` | Format (`rustfmt.toml`: edition 2024, 120 columns) |
| `lint` | `cargo clippy --all-targets -- -D warnings` | Lint; warnings are errors |
| `check` | `cargo check --all-targets` | Type-check without building |
| `test` | `cargo test` | Unit, parity and end-to-end tests |
| `cov` | `cargo llvm-cov --all-targets --fail-under-lines 90` | Full suite with a 90% line-coverage gate |
| `build` | `cargo build --release` | Optimized binary at `target/release/pixi-oci` |
| `pre-commit-run` | `pre-commit run --all-files` | All hooks over the tree |
| `ci` | `pre-commit-run` → `build` → `check` → `lint` → `cov` | The gate; must pass before work is done |
| `fixture` | `pixi lock` in `tests/fixtures/workspace` | Re-lock the test fixture; never edit its lockfile by hand |
| `bootstrap` | `pre-commit install ...` | One-time hook installation |
| `deny` (`lint` environment) | `cargo deny check` | Advisories, licenses, banned crates and sources, against `deny.toml` |
| `msrv-check` (`msrv` environment) | `cargo check --all-targets --locked` | Builds on the minimum supported Rust (1.95) |
| `docs-serve` (`docs` environment) | `mkdocs serve` | Live preview of this site at http://127.0.0.1:8000 |
| `docs-build` (`docs` environment) | `mkdocs build --strict` | Build the site into `site/`; a broken link or a page missing from the nav fails |

## Tests that use the network

`PIXI_OCI_NETWORK_TESTS=1 pixi run test` adds two tests that download from conda-forge: the parity test on real
packages, and `tests/network.rs`, which builds an image, unpacks its layers and runs its python (Linux only). CI's
Linux test job sets the variable. `PIXI_OCI_CACHE_DIR` keeps their downloads between runs.

## Workflow

- A feature branch and a pull request for every change. Work is done when the PR's checks pass; `CI result` is the
  one check branch protection requires.
- Conventional commit messages, which the commit-msg hook enforces.
- When rattler changes how it installs, the parity tests fail first. Match rattler in `stream.rs` rather than
  changing the tests.

## CI

| Job | What it proves |
|---|---|
| Lint | pre-commit, rustfmt, clippy and `cargo check` are clean |
| Test (Linux, macOS, Windows) | the suite passes on each; on Linux with the network tests |
| Coverage gate | at least 90% of lines are covered |
| Dependency advisories and licenses | cargo-deny is clean |
| Builds on the MSRV | the crate builds on Rust 1.95 |
| Image digest (Linux, macOS, Windows), Same image on every host | the fixture image has one digest on every host; the release binaries are kept as artifacts |
| Docs | this site builds strictly |
| CI result | every job above passed |

## The documentation site

The site is [MkDocs](https://www.mkdocs.org) with the Material theme, configured in `mkdocs.yml`; its colours and
heading font are in `docs/overrides/pixi-oci.css`. The pages are the Markdown files in `docs/`, plus `SECURITY.md`,
which `mkdocs_hooks.py` publishes as the security page at build time from `extra.repo_pages`. The `docs` pixi
environment is separate from the Rust one, so `pixi run -e docs docs-serve` does not pull in a Rust toolchain.

The **Docs site** workflow publishes the site to GitHub Pages, versioned with [mike](https://github.com/jimporter/mike)
as pixi-sbom's is: a push to `main` that touches the docs deploys `dev`, a published release deploys its version
(`v0.2.0`) and moves `latest` to it, and the site root opens `latest`, or `dev` before the first release. GitHub Pages
must be set to deploy from **GitHub Actions** (Settings → Pages → Build and deployment → Source).

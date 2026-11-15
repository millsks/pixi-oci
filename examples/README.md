# Examples

Real pixi workspaces, small to large, for planning, building, checking and running images of different kinds and
sizes, by hand and in CI. They follow [pixi-sbom's examples](https://github.com/aesir-oss/pixi-sbom/tree/main/examples)
in layout and naming, with two differences: pixi-oci reads only `pixi.lock`, so only pixi projects, and these lock
current versions, to make realistic images (pixi-sbom's pin old ones on purpose, for advisories to find).

```
examples/projects/pixi/NN-name/
  pixi.toml    what the environment asks for
  pixi.lock    what pixi locked, for linux-64 and linux-aarch64: written by `pixi lock`, never edited
  README.md    what it shows, how to build, check and run it, and what `check` reports
```

| Example | What it shows | Packages (linux-64) | Layers | Download | Entrypoint |
|---|---|---|---|---|---|
| [`01-hello-python`](projects/pixi/01-hello-python) | python alone | 27 conda | 7 | 46.1 MiB | `python` |
| [`02-cli-tool`](projects/pixi/02-cli-tool) | a noarch python CLI with entry points (HTTPie) | 46 conda | 8 | 50.0 MiB | `http` |
| [`03-flask`](projects/pixi/03-flask) | Flask served by gunicorn | 37 conda | 7 | 47.9 MiB | `gunicorn` |
| [`04-fastapi`](projects/pixi/04-fastapi) | FastAPI run by uvicorn | 102 conda | 15 | 63.8 MiB | `uvicorn` |
| [`05-native-binary`](projects/pixi/05-native-binary) | compiled tools only, no python (ripgrep, jq) | 6 conda | 2 | 4.0 MiB | `rg` |
| [`06-r-analysis`](projects/pixi/06-r-analysis) | R 4.5 with dplyr, ggplot2, readr | 129 conda | 35 | 299.5 MiB | `Rscript` |
| [`07-node-tools`](projects/pixi/07-node-tools) | Node.js 22, npm, pnpm | 11 conda | 6 | 51.8 MiB | `node` |
| [`08-data-analysis`](projects/pixi/08-data-analysis) | pandas, polars, scipy, scikit-learn, matplotlib | 149 conda | 37 | 367.5 MiB | `python` |
| [`09-streamlit-dashboard`](projects/pixi/09-streamlit-dashboard) | Streamlit with altair | 156 conda | 34 | 162.4 MiB | `streamlit` |
| [`10-jupyter`](projects/pixi/10-jupyter) | JupyterLab with the data stack | 246 conda | 44 | 337.2 MiB | `jupyter-lab` |
| [`11-machine-learning`](projects/pixi/11-machine-learning) | xgboost, lightgbm, scikit-learn | 53 conda | 17 | 138.2 MiB | `python` |
| [`12-deep-learning`](projects/pixi/12-deep-learning) | PyTorch on the CPU, torchvision | 84 conda | 25 | 328.0 MiB | `python` |
| [`13-pypi-mix`](projects/pixi/13-pypi-mix) | conda python with PyPI wheels on top (httpx, rich-cli, ruff) | 35 conda + 14 PyPI | 9 | 46.8 MiB | `ruff` |
| [`14-large`](projects/pixi/14-large) | data science, ML, geospatial and Jupyter together | 385 conda | 84 | 764.0 MiB | `jupyter-lab` |

Download sizes are the conda archives the lockfile records; it records no wheel sizes, so PyPI wheels are not
counted.

## Tasks

| Task | |
|---|---|
| `pixi run examples-plan` | plan every example for both platforms, offline; CI runs it on every pull request |
| `pixi run examples-build NAME [ENTRYPOINT] [PLATFORM] [BASE]` | build one on a base (Debian 12 slim by default, which has the perl a few conda scripts use) into `target/examples/`, then `check` it |
| `pixi run examples-lock NAME` | re-lock one after changing its `pixi.toml` |

CI also checks every lockfile against its manifest (`pixi lock --check`) and builds `01-hello-python`,
`05-native-binary` and `13-pypi-mix` on Debian, validating and checking each.

## What `check` reports for them

On the default base, `debian:bookworm-slim`, the examples `check` clean, except `06-r-analysis` and
`07-node-tools`, whose packages ship a few things `check` rightly names. Every python example includes
`xorg-libx11`: conda-forge's python depends on tk, which links to X11, and without it `check` would report
`libX11.so.6`. Each example's README says what `check` finds and why.

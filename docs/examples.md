# Examples

The repository's [`examples/`](https://github.com/millsks/pixi-oci/tree/main/examples) directory holds real pixi
workspaces, small to large, each with its lockfile and a README on how to build, check and run it.

| Example | What it shows | Packages (linux-64) | Grouped layers | Download | Entrypoint |
|---|---|---|---|---|---|
| [`01-hello-python`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/01-hello-python) | python alone | 27 conda | 7 | 46.1 MiB | `python` |
| [`02-cli-tool`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/02-cli-tool) | a noarch python CLI with entry points (HTTPie) | 46 conda | 8 | 50.0 MiB | `http` |
| [`03-flask`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/03-flask) | Flask served by gunicorn | 37 conda | 7 | 47.9 MiB | `gunicorn` |
| [`04-fastapi`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/04-fastapi) | FastAPI run by uvicorn, its app added with `--add` | 102 conda | 15 | 63.8 MiB | `uvicorn` |
| [`05-native-binary`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/05-native-binary) | compiled tools only, no python (ripgrep, jq) | 6 conda | 2 | 4.0 MiB | `rg` |
| [`06-r-analysis`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/06-r-analysis) | R 4.5 with dplyr, ggplot2, readr | 129 conda | 35 | 299.5 MiB | `Rscript` |
| [`07-node-tools`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/07-node-tools) | Node.js 22, npm, pnpm | 11 conda | 6 | 51.8 MiB | `node` |
| [`08-data-analysis`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/08-data-analysis) | pandas, polars, scipy, scikit-learn, matplotlib | 149 conda | 37 | 367.5 MiB | `python` |
| [`09-streamlit-dashboard`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/09-streamlit-dashboard) | Streamlit with altair | 156 conda | 34 | 162.4 MiB | `streamlit` |
| [`10-jupyter`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/10-jupyter) | JupyterLab with the data stack | 246 conda | 44 | 337.2 MiB | `jupyter-lab` |
| [`11-machine-learning`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/11-machine-learning) | xgboost, lightgbm, scikit-learn | 53 conda | 17 | 138.2 MiB | `python` |
| [`12-deep-learning`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/12-deep-learning) | PyTorch on the CPU, torchvision | 84 conda | 25 | 328.0 MiB | `python` |
| [`13-pypi-mix`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/13-pypi-mix) | conda python with PyPI wheels on top (httpx, rich-cli, ruff) | 35 conda + 14 PyPI | 9 | 46.8 MiB | `ruff` |
| [`14-large`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/14-large) | data science, ML, geospatial and Jupyter together | 385 conda | 84 | 764.0 MiB | `jupyter-lab` |

By default each environment is one layer on top of the base image's. The counts are for `--layers grouped`, without
a base image: a base adds its own (one for UBI 9 minimal and Debian 12 slim). Even the largest, 385 packages, plans
into 84 grouped layers, well under the limit.

From a clone, plan them all, or build one on a base and check it:

```sh
pixi run examples-plan
pixi run examples-build 03-flask gunicorn
```

On `debian:bookworm-slim`, the default base of `examples-build`, the examples [`check`](cli.md#pixi-oci-check) clean,
except the R and Node.js ones, whose packages ship a few things `check` rightly names; each example's README says
what. Every python example includes `xorg-libx11`, since conda-forge's python depends on tk, which links to X11.

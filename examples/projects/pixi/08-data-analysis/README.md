# Data analysis

The everyday data analysis stack (pandas, polars, scipy, scikit-learn and matplotlib, with OpenBLAS and the compiled libraries under them) and a report package that runs on it, its config and its data, each added to its own place.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 149 conda packages, 367.5 MiB to download: one layer by default, 37 with `--layers grouped`.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/08-data-analysis/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/08-data-analysis/pixi.lock -p linux-64 \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --add examples/projects/pixi/08-data-analysis/src:/app \
  --add examples/projects/pixi/08-data-analysis/config/report.toml:/etc/report/ \
  --add examples/projects/pixi/08-data-analysis/data:/data --workdir /app \
  --entrypoint python --cmd "-m report --config /etc/report/report.toml" \
  -t 08-data-analysis:latest -o 08-data-analysis.tar
pixi oci check 08-data-analysis.tar
pixi oci inspect 08-data-analysis.tar
```

## The application

Three `--add`s into one application layer:

| Source | In the image | Left out |
|---|---|---|
| `src/` | `/app/report/` | `src/report/notebooks/`, by `.pixi-oci-ignore`: git keeps the notebooks, the image does not |
| `config/report.toml` | `/etc/report/report.toml` | |
| `data/` | `/data/sales.csv` | `data/raw/`, by the workspace's `.dockerignore`, whose patterns are relative to this directory |

`python -m report` reads the config, sums revenue and units by region with polars, prints the table and writes it to `/tmp/report/summary.csv`. `.gitignore` keeps `__pycache__` out.

`registry.access.redhat.com/ubi9/ubi-minimal` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 08-data-analysis.tar
podman run --rm 08-data-analysis:latest
```

## What `check` reports

Nothing in the application layer. In the environment, on `debian:bookworm-slim`, 25 problems the report never touches:

- **24 `library` problems from PySide6.** `matplotlib`, unlike `matplotlib-base`, brings the Qt backend, and PySide6's bindings for optional Qt modules (Qt 3D, Charts, Graphs, Multimedia, PDF, WebEngine and others) link to libraries `qt6-main` does not ship. Only `import PySide6.Qt3DCore` and the like would fail. Depending on `matplotlib-base` instead leaves PySide6 out.
- **A `placeholder` problem:** 58 files in scipy and scikit-learn still hold a conda build placeholder their packages do not declare, so installing does not replace it. They include `scipy/__config__.py` and some compiled modules; a placeholder matters only where a file's path text is read at run time.

The environment includes `xorg-libx11`: conda-forge's python depends on tk, which links to X11, and without it `check` would also report `libX11.so.6`.

Built on `ubi9/ubi-minimal` instead, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

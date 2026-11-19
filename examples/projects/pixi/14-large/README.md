# A large environment

Data science, machine learning, geospatial and Jupyter in one environment: pandas, polars, scikit-learn, pytorch, dask, gdal, jupyterlab and more, a few hundred packages. It shows `--layers grouped` at scale: the big packages each on a layer, the rest grouped, well under the layer limit, so a package upgrade re-uploads only the layers that hold it.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 385 conda packages, 764.0 MiB to download: one layer by default, 84 with `--layers grouped`.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/14-large/pixi.lock -p linux-64 --layers grouped
pixi oci build --lockfile examples/projects/pixi/14-large/pixi.lock -p linux-64 --layers grouped \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --entrypoint jupyter-lab -t 14-large:latest -o 14-large.tar
pixi oci check 14-large.tar
```

`registry.access.redhat.com/ubi9/ubi-minimal` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 14-large.tar
podman run --rm 14-large:latest --version
```

## What `check` reports

On `debian:bookworm-slim`, 28 problems, all in optional parts of the environment:

- **24 `library` problems from PySide6.** `matplotlib`, unlike `matplotlib-base`, brings the Qt backend, and PySide6's bindings for optional Qt modules (Qt 3D, Charts, Graphs, Multimedia, PDF, WebEngine and others) link to libraries `qt6-main` does not ship. Only `import PySide6.Qt3DCore` and the like would fail.
- **pyarrow's Flight and Gandiva modules** link to `libarrow_flight.so` and `libgandiva.so`, which conda-forge ships as their own packages (`libarrow-flight`, `libgandiva`); the environment does not include them. Only `import pyarrow.flight` and `import pyarrow.gandiva` would fail.
- **`libzmq.so.5`, for pyzmq: a false positive** ([#62](https://github.com/millsks/pixi-oci/issues/62)). The library is in `/opt/env/lib`, and the loader finds it through python's own RPATH; `check` does not follow that yet.
- **A `placeholder` problem:** 70 files in scipy (42), scikit-learn (15) and statsmodels (11) still hold a conda build placeholder their packages do not declare; a placeholder matters only where a file's path text is read at run time.

The environment includes `xorg-libx11`: conda-forge's python depends on tk, which links to X11, and without it `check` would also report `libX11.so.6`.

Built on `ubi9/ubi-minimal` instead, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

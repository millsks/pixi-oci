# A large environment

Data science, machine learning, geospatial and Jupyter in one environment: pandas, polars, scikit-learn, pytorch, dask, gdal, jupyterlab and more, a few hundred packages. It shows grouping at scale: the big packages each on a layer, the rest grouped, well under the layer limit.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 385 conda packages, 764.0 MiB to download, planned into 84 layers.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/14-large/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/14-large/pixi.lock -p linux-64 \
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

Nothing, on `debian:bookworm-slim`. The environment includes `xorg-libx11`: conda-forge's python depends on tk, which links to X11, and without it `check` would report `libX11.so.6`.

Built on `ubi9/ubi-minimal` instead, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

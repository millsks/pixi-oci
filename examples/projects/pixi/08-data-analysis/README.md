# Data analysis

The everyday data analysis stack: pandas, polars, scipy, scikit-learn and matplotlib, with OpenBLAS and the compiled libraries under them.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 149 conda packages, 367.5 MiB to download: one layer by default, 37 with `--layers grouped`.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/08-data-analysis/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/08-data-analysis/pixi.lock -p linux-64 \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --entrypoint python -t 08-data-analysis:latest -o 08-data-analysis.tar
pixi oci check 08-data-analysis.tar
```

`registry.access.redhat.com/ubi9/ubi-minimal` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 08-data-analysis.tar
podman run --rm 08-data-analysis:latest -c "import pandas, polars, scipy, sklearn, matplotlib; print('ok')"
```

## What `check` reports

Nothing, on `debian:bookworm-slim`. The environment includes `xorg-libx11`: conda-forge's python depends on tk, which links to X11, and without it `check` would report `libX11.so.6`.

Built on `ubi9/ubi-minimal` instead, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

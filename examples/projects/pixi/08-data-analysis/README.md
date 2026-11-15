# Data analysis

The everyday data analysis stack: pandas, polars, scipy, scikit-learn and matplotlib, with OpenBLAS and the compiled libraries under them.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 149 conda packages, 367.5 MiB to download, planned into 37 layers.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/08-data-analysis/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/08-data-analysis/pixi.lock -p linux-64 \
  --base registry.access.redhat.com/ubi9/ubi-minimal@sha256:baafce2d34783a28b4a472779f304e97e00f9d35c99553bc53b068b14344d9a9 \
  --entrypoint python -t 08-data-analysis:latest -o 08-data-analysis.tar
pixi oci check 08-data-analysis.tar
```

`debian:bookworm-slim` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 08-data-analysis.tar
podman run --rm 08-data-analysis:latest -c "import pandas, polars, scipy, sklearn, matplotlib; print('ok')"
```

## What `check` reports

conda-forge's python depends on tk, which links to `libX11.so.6`: `check` reports it as a `library` problem. It matters only to `tkinter`; add `xorg-libx11` to the environment if you use it.

On `ubi9/ubi-minimal`, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

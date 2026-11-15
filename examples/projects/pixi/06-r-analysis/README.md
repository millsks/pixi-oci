# R analysis

R 4.5 with dplyr, ggplot2 and readr from conda-forge: R itself, its compiled packages and the system libraries they link to.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 129 conda packages, 299.5 MiB to download, planned into 35 layers.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/06-r-analysis/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/06-r-analysis/pixi.lock -p linux-64 \
  --base registry.access.redhat.com/ubi9/ubi-minimal@sha256:baafce2d34783a28b4a472779f304e97e00f9d35c99553bc53b068b14344d9a9 \
  --entrypoint Rscript -t 06-r-analysis:latest -o 06-r-analysis.tar
pixi oci check 06-r-analysis.tar
```

`debian:bookworm-slim` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 06-r-analysis.tar
podman run --rm 06-r-analysis:latest -e 'library(dplyr); cat("ok\n")'
```

## What `check` reports

r-base brings conda-forge's C, C++ and Fortran compilers, so R can build packages from source. Those compiler binaries
keep their build paths in their embedded strings, undeclared for replacement, and `check` reports them as
`placeholder` problems; they do not affect running R. On `ubi9/ubi-minimal`, which has no perl, a few perl scripts
are reported as `shebang` problems too; `debian:bookworm-slim` has perl.

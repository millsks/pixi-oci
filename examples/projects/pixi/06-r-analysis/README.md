# R analysis

R 4.5 with dplyr, ggplot2 and readr from conda-forge, and an analysis script with its data: the code goes to `/app` and the data to `/data`, as two `--add`s.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 129 conda packages, 299.5 MiB to download: one layer by default, 35 with `--layers grouped`.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/06-r-analysis/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/06-r-analysis/pixi.lock -p linux-64 \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --add examples/projects/pixi/06-r-analysis/analysis:/app --add examples/projects/pixi/06-r-analysis/data:/data \
  --entrypoint Rscript --cmd /app/analysis.R \
  -t 06-r-analysis:latest -o 06-r-analysis.tar
pixi oci check 06-r-analysis.tar
pixi oci inspect 06-r-analysis.tar
```

## The application

`analysis/analysis.R` reads `/data/sales.csv`, prints revenue and units by region, and writes a bar chart to `/tmp/sales.png`. Code and data are separate `--add`s into the same application layer, so the script can be pointed at other data: `Rscript /app/analysis.R other.csv out.png`.

`registry.access.redhat.com/ubi9/ubi-minimal` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 06-r-analysis.tar
podman run --rm 06-r-analysis:latest
```

## What `check` reports

r-base brings conda-forge's C, C++ and Fortran compilers, so R can build packages from source. Those compiler binaries
keep their build paths in their embedded strings, undeclared for replacement, and `check` reports them as
`placeholder` problems; they do not affect running R. On `ubi9/ubi-minimal`, which has no perl, a few perl scripts
are reported as `shebang` problems too; `debian:bookworm-slim` has perl.

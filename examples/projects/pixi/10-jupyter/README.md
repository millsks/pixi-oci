# JupyterLab

JupyterLab with pandas, matplotlib and ipywidgets: a few hundred small packages, most of them noarch python, which grouping keeps to a few dozen layers.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 246 conda packages, 337.2 MiB to download, planned into 44 layers.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/10-jupyter/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/10-jupyter/pixi.lock -p linux-64 \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --entrypoint jupyter-lab -t 10-jupyter:latest -o 10-jupyter.tar
pixi oci check 10-jupyter.tar
```

`registry.access.redhat.com/ubi9/ubi-minimal` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 10-jupyter.tar
podman run --rm 10-jupyter:latest --version
```

## What `check` reports

Nothing, on `debian:bookworm-slim`. The environment includes `xorg-libx11`: conda-forge's python depends on tk, which links to X11, and without it `check` would report `libX11.so.6`.

Built on `ubi9/ubi-minimal` instead, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

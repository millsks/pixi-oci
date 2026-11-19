# JupyterLab

JupyterLab with pandas, matplotlib and ipywidgets: a few hundred small packages, most of them noarch python, which grouping keeps to a few dozen layers.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 246 conda packages, 337.2 MiB to download: one layer by default, 44 with `--layers grouped`.

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

On `debian:bookworm-slim`, 26 problems, none of them in what JupyterLab needs to start:

- **24 `library` problems from PySide6.** `matplotlib`, unlike `matplotlib-base`, brings the Qt backend, and PySide6's bindings for optional Qt modules (Qt 3D, Charts, Graphs, Multimedia, PDF, WebEngine and others) link to libraries `qt6-main` does not ship. Only `import PySide6.Qt3DCore` and the like would fail.
- **`libzmq.so.5`, for pyzmq: a false positive** ([#62](https://github.com/millsks/pixi-oci/issues/62)). The library is in `/opt/env/lib`, and the loader finds it through python's own RPATH; `check` does not follow that yet.
- **A `placeholder` problem:** `lib/libfontconfig.a`, a static library nothing loads at run time, still holds a conda build placeholder its package does not declare.

The environment includes `xorg-libx11`: conda-forge's python depends on tk, which links to X11, and without it `check` would also report `libX11.so.6`.

Built on `ubi9/ubi-minimal` instead, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

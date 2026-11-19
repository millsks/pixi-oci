# Deep learning

[PyTorch](https://pytorch.org) on the CPU with torchvision: a few very large packages (libtorch, the BLAS behind it), each on a layer of its own.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 84 conda packages, 328.0 MiB to download: one layer by default, 25 with `--layers grouped`.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/12-deep-learning/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/12-deep-learning/pixi.lock -p linux-64 \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --entrypoint python -t 12-deep-learning:latest -o 12-deep-learning.tar
pixi oci check 12-deep-learning.tar
```

`registry.access.redhat.com/ubi9/ubi-minimal` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 12-deep-learning.tar
podman run --rm 12-deep-learning:latest -c "import torch; print(torch.__version__)"
```

## What `check` reports

On `debian:bookworm-slim`, one `placeholder` problem: `lib/libtorch_cpu.so` still holds a conda build placeholder its package does not declare, so installing does not replace it. It is a string inside the library; a placeholder matters only where a file's path text is used at run time.

The environment includes `xorg-libx11`: conda-forge's python depends on tk, which links to X11, and without it `check` would also report `libX11.so.6`.

Built on `ubi9/ubi-minimal` instead, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

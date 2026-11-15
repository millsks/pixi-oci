# Deep learning

[PyTorch](https://pytorch.org) on the CPU with torchvision: a few very large packages (libtorch, the BLAS behind it), each on a layer of its own.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 83 conda packages, 327.2 MiB to download, planned into 25 layers.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/12-deep-learning/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/12-deep-learning/pixi.lock -p linux-64 \
  --base registry.access.redhat.com/ubi9/ubi-minimal@sha256:baafce2d34783a28b4a472779f304e97e00f9d35c99553bc53b068b14344d9a9 \
  --entrypoint python -t 12-deep-learning:latest -o 12-deep-learning.tar
pixi oci check 12-deep-learning.tar
```

`debian:bookworm-slim` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 12-deep-learning.tar
podman run --rm 12-deep-learning:latest -c "import torch; print(torch.__version__)"
```

## What `check` reports

conda-forge's python depends on tk, which links to `libX11.so.6`: `check` reports it as a `library` problem. It matters only to `tkinter`; add `xorg-libx11` to the environment if you use it.

On `ubi9/ubi-minimal`, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

# Hello, Python

Python 3.13 and nothing else: the interpreter, its standard library and the libraries it links to. The smallest useful image, and the base the other Python examples grow from.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 27 conda packages, 46.1 MiB to download: one layer by default, 7 with `--layers grouped`.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/01-hello-python/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/01-hello-python/pixi.lock -p linux-64 \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --entrypoint python -t 01-hello-python:latest -o 01-hello-python.tar
pixi oci check 01-hello-python.tar
```

`registry.access.redhat.com/ubi9/ubi-minimal` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 01-hello-python.tar
podman run --rm 01-hello-python:latest -c "import ssl; print(ssl.OPENSSL_VERSION)"
```

## What `check` reports

Nothing, on `debian:bookworm-slim`. The environment includes `xorg-libx11`: conda-forge's python depends on tk, which links to X11, and without it `check` would report `libX11.so.6`.

Built on `ubi9/ubi-minimal` instead, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

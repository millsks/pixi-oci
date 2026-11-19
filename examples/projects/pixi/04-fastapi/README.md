# FastAPI

A [FastAPI](https://fastapi.tiangolo.com) service, run by uvicorn: the environment (pydantic's compiled core, uvloop and the rest) and a small application in `app/`, added to the image with `--add`. The image is three layers: the base, the environment, and the application.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 102 conda packages, 63.8 MiB to download: one layer by default, 15 with `--layers grouped`.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/04-fastapi/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/04-fastapi/pixi.lock -p linux-64 \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --add examples/projects/pixi/04-fastapi/app:/app --workdir /app \
  --entrypoint uvicorn --cmd "main:app --host 0.0.0.0 --port 8000" --expose 8000 \
  -t 04-fastapi:latest -o 04-fastapi.tar
pixi oci check 04-fastapi.tar
pixi oci inspect 04-fastapi.tar
```

`app/.gitignore` keeps `__pycache__` out of the image. Change `app/main.py` and build again: only the application layer's digest changes, so a push uploads only that layer.

`registry.access.redhat.com/ubi9/ubi-minimal` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 04-fastapi.tar
podman run --rm -p 8000:8000 04-fastapi:latest
curl http://localhost:8000/
```

## What `check` reports

Nothing, on `debian:bookworm-slim`. The environment includes `xorg-libx11`: conda-forge's python depends on tk, which links to X11, and without it `check` would report `libX11.so.6`.

Built on `ubi9/ubi-minimal` instead, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

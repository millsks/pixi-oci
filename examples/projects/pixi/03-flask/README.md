# Flask

A [Flask](https://flask.palletsprojects.com) web application in `app/`, served by gunicorn: a package with its templates and static files, added to `/srv/app` with `--add`.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 37 conda packages, 47.9 MiB to download: one layer by default, 7 with `--layers grouped`.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/03-flask/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/03-flask/pixi.lock -p linux-64 \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --add examples/projects/pixi/03-flask/app:/srv/app --workdir /srv \
  --entrypoint gunicorn --cmd "--bind 0.0.0.0:8000 app:create_app()" --expose 8000 \
  -t 03-flask:latest -o 03-flask.tar
pixi oci check 03-flask.tar
pixi oci inspect 03-flask.tar
```

## The application

`app/` is a package: `__init__.py` with `create_app()`, `templates/index.html` and `static/style.css`. A directory goes in with its tree, so `--add app:/srv/app` gives `/srv/app/templates/index.html` and the rest, and `--workdir /srv` lets gunicorn import `app`. `app/.gitignore` keeps `__pycache__` out.

`registry.access.redhat.com/ubi9/ubi-minimal` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 03-flask.tar
podman run --rm -p 8000:8000 03-flask:latest
curl http://localhost:8000/health
```

## What `check` reports

Nothing, on `debian:bookworm-slim`. The environment includes `xorg-libx11`: conda-forge's python depends on tk, which links to X11, and without it `check` would report `libX11.so.6`.

Built on `ubi9/ubi-minimal` instead, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

# Streamlit dashboard

A [Streamlit](https://streamlit.io) dashboard with altair and pandas: `dashboard/` holds the app and its `.streamlit/config.toml`, added to `/app` with `--add`.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 156 conda packages, 162.4 MiB to download: one layer by default, 34 with `--layers grouped`.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/09-streamlit-dashboard/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/09-streamlit-dashboard/pixi.lock -p linux-64 \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --add examples/projects/pixi/09-streamlit-dashboard/dashboard:/app --workdir /app \
  --entrypoint streamlit --cmd "run dashboard.py" --expose 8501 \
  -t 09-streamlit-dashboard:latest -o 09-streamlit-dashboard.tar
pixi oci check 09-streamlit-dashboard.tar
pixi oci inspect 09-streamlit-dashboard.tar
```

## The application

`dashboard.py` charts eight weeks of signups. Streamlit reads `.streamlit/config.toml` from its working directory, which sets it headless on `0.0.0.0:8501`: `--add` keeps dot directories, and only `.git` and `.pixi` are always left out.

`registry.access.redhat.com/ubi9/ubi-minimal` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 09-streamlit-dashboard.tar
podman run --rm -p 8501:8501 09-streamlit-dashboard:latest
```

## What `check` reports

Nothing in the application layer. In the environment, on `debian:bookworm-slim`, two `library` problems the dashboard never touches: pyarrow's Flight and Gandiva modules link to `libarrow_flight.so` and `libgandiva.so`, which conda-forge ships as their own packages (`libarrow-flight`, `libgandiva`), and the environment does not include them. Only `import pyarrow.flight` and `import pyarrow.gandiva` would fail.

The environment includes `xorg-libx11`: conda-forge's python depends on tk, which links to X11, and without it `check` would also report `libX11.so.6`.

Built on `ubi9/ubi-minimal` instead, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

# Machine learning

Gradient boosting and classic machine learning (xgboost, lightgbm and scikit-learn) and a training script, a prediction script and their data, run as an unprivileged user with code it cannot change.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 53 conda packages, 138.2 MiB to download: one layer by default, 17 with `--layers grouped`.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/11-machine-learning/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/11-machine-learning/pixi.lock -p linux-64 \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --add examples/projects/pixi/11-machine-learning/ml:/app --add examples/projects/pixi/11-machine-learning/data:/app/data \
  --user 1000:1000 --entrypoint python --cmd /app/train.py \
  -t 11-machine-learning:latest -o 11-machine-learning.tar
pixi oci check 11-machine-learning.tar
pixi oci inspect 11-machine-learning.tar
```

## The application

`ml/train.py` trains an xgboost classifier on `data/flowers.csv` (90 synthetic rows, see `data/README.md`) and saves the model; `ml/predict.py` classifies one flower with it. `data/` goes inside `/app/data` with a second `--add`.

Files from `--add` are owned by root, and the image runs as `--user 1000:1000`, so the code cannot be changed by the process that runs it. The model goes where that user may write: `/tmp` by default, or a mounted directory.

`registry.access.redhat.com/ubi9/ubi-minimal` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 11-machine-learning.tar
mkdir -p out
podman run --rm -v "$PWD/out:/out" 11-machine-learning:latest --out /out/model.json
podman run --rm -v "$PWD/out:/out" --entrypoint python 11-machine-learning:latest /app/predict.py --model /out/model.json 6.6 3.0 5.6 2.0
```

## What `check` reports

Nothing in the application layer. In the environment, on `debian:bookworm-slim`, one `placeholder` problem: 57 files in scipy (42) and scikit-learn (15) still hold a conda build placeholder their packages do not declare, so installing does not replace it. They include `scipy/__config__.py` and some compiled modules; a placeholder matters only where a file's path text is read at run time.

The environment includes `xorg-libx11`: conda-forge's python depends on tk, which links to X11, and without it `check` would also report `libX11.so.6`.

Built on `ubi9/ubi-minimal` instead, which has no perl, openssl's perl scripts (`c_rehash`, `CA.pl`, `tsget.pl`) are reported as `shebang` problems too; `debian:bookworm-slim` has perl. They matter only if you run them.

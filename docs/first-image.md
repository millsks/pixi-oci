# Your first image

This walks from a `pixi.toml` to an image you can inspect and load. It works the same on Linux, macOS and Windows.

## 1. A workspace

An image is built from one environment of a lockfile, for one Linux platform. Any pixi workspace works, as long as
it locks a `linux-*` platform:

```toml title="pixi.toml"
[workspace]
name = "hello"
channels = ["conda-forge"]
platforms = ["linux-64", "linux-aarch64", "win-64", "osx-arm64"]

[dependencies]
python = "3.13.*"
six = "*"
```

```sh
pixi lock
```

Your own platform can stay in the list; only the `linux-*` ones can become images. PyPI dependencies are not
installed into images yet, so keep this environment to conda packages for now.

## 2. Plan it

`plan` reads the lockfile and shows the layers, without downloading anything:

```console
$ pixi oci plan -p linux-64
Environment default on linux-64 (image platform linux/amd64)
23 conda packages, 0 PyPI packages, 23 layers, 44.9 MiB to download

   #  Layer                       Download
   1  ca-certificates 2026.10.23  127.0 KiB
   2  libgomp 16.2.0              625.6 KiB
   ...
  22  python 3.13.18              23.2 MiB
  23  six 1.17.0                  18.0 KiB
```

One layer per package, the ones everything depends on first. `--format json` gives the same plan with every
package's URL, sha256 and size.

## 3. Build it

```console
$ pixi oci build -p linux-64 --tag hello:1.0 -o hello.tar
[1/23] ca-certificates 2026.10.23 (127.0 KiB)
...
[23/23] six 1.17.0 (18.0 KiB)
Wrote hello:1.0 to hello.tar
  manifest sha256:c41ba44878826e5db8f121aeaa113ff80aba1fac14283b5172491b08afa4c7a8
  23 layers, 62.6 MiB for linux/amd64
```

Each package is downloaded once into the cache, checked against the lockfile's sha256, and written into its own
layer with its paths rewritten for `/opt/env`, where the environment lives in the image. Build again and you get
the same manifest digest, on this machine or any other. (Your digest will differ from this one: it depends on
exactly what your lockfile locks.)

The layers come to more than the download: conda packages are compressed with zstd, and OCI layers with gzip, which
every container tool can read.

The output is a `.tar` archive here. Give `-o` a path that does not end in `.tar` (`-o hello-image`) for an OCI image
layout directory instead.

## 4. Look inside

Any OCI tool reads it, with no daemon:

```sh
skopeo inspect oci-archive:hello.tar            # platform, layers, environment variables
skopeo inspect --config oci-archive:hello.tar   # the full image config
crane validate --tarball hello.tar              # every digest checked
```

The image sets `PATH` to `/opt/env/bin:...` and `CONDA_PREFIX` to `/opt/env`. Add your own settings with `--env`,
`--entrypoint`, `--cmd` and `--workdir` ([reference](cli.md#pixi-oci-build)).

## 5. Load it

```sh
podman load -i hello.tar
docker load -i hello.tar
```

!!! warning "It does not run yet"

    The image holds the environment and nothing else: no base, so no glibc. `docker run hello:1.0 python` fails with
    `exec /opt/env/bin/python: no such file or directory`, which is the missing system loader. Base images
    (`--base`) are the next milestone on the [roadmap](roadmap.md); until then the image is for inspecting, scanning,
    and copying the environment onto a base of your own.

## Other platforms

Build for ARM servers from any machine, with no emulation, since nothing is run:

```sh
pixi oci build -p linux-aarch64 --tag hello:1.0-arm64 -o hello-arm64.tar
```

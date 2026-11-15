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
23 conda packages, 0 PyPI packages, 7 layers, 44.9 MiB to download
4 conda packages in layers of their own, 19 sharing 3 layers

  #  Layer              Download
  1  4 conda packages   1.8 MiB
  2  libpython 3.13.18  9.3 MiB
  3  12 conda packages  5.9 MiB
  4  tk 8.6.13          3.4 MiB
  5  3 conda packages   1.4 MiB
  6  python 3.13.18     23.2 MiB
  7  six 1.17.0         18.0 KiB
```

Packages of 4 MiB or more get a layer of their own, smaller ones share a few, and the ones everything depends on come
first. `--packages` lists what each shared layer holds, and `--format json` gives the same plan with every package's
URL, sha256 and size.

## 3. Build it

```console
$ pixi oci build -p linux-64 --tag hello:1.0 -o hello.tar
[1/7] 4 conda packages (1.8 MiB)
...
[7/7] six 1.17.0 (18.0 KiB)
Wrote hello:1.0 to hello.tar
  manifest sha256:9ec8407e4f1860919cd8dd7af6027a9bdcb91a5a008b2c5ea657ed3f300044ef
  7 layers, 62.6 MiB for linux/amd64
```

Each package is downloaded once into the cache, checked against the lockfile's sha256, and written into its
layer with its paths rewritten for `/opt/env`, where the environment lives in the image. Build again and you get
the same manifest digest, on this machine or any other. (Your digest will differ from this one: it depends on
exactly what your lockfile locks.)

The layers come to more than the download: conda packages are compressed with zstd, and OCI layers with gzip, which
every container tool can read.

The output is a `.tar` archive here. Give `-o` a path that does not end in `.tar` (`-o hello-image`) for an OCI image
layout directory instead.

That image holds the environment alone. conda packages use the system's glibc, so to run one, build it on a base
image that has glibc and a shell, pinned by its digest ([Base images](base-images.md)):

```sh
pixi oci build -p linux-64 --tag hello:1.0 -o hello.tar --entrypoint python \
  --base registry.access.redhat.com/ubi9/ubi-minimal@sha256:baafce2d34783a28b4a472779f304e97e00f9d35c99553bc53b068b14344d9a9
```

`debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc` works the same way.

## 4. Look inside

Check it against the OCI image spec, with nothing else installed:

```console
$ pixi oci validate hello.tar
hello.tar is a valid OCI image
  manifest sha256:9ec8407e4f18 for linux/amd64: 7 layers, 62.6 MiB
```

See what is in it, and which packages each layer holds:

```sh
pixi oci inspect hello.tar              # platform, settings, and a row per layer
pixi oci inspect hello.tar --packages   # every package of every shared layer
pixi oci inspect hello.tar --raw config # the full image config, exactly as stored
```

Check that it would run, without running it:

```sh
pixi oci check hello.tar
```

It reads every binary and script. Built without a base, the image gets a `glibc` problem: nothing in it provides the
loader every conda binary needs. Built with `--base`, it reports only what the environment itself lacks, if
anything.

It is a standard OCI image, so any OCI tool reads it as well, with no daemon:

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

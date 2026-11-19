# Native binaries

Compiled tools only, no python: [ripgrep](https://github.com/BurntSushi/ripgrep) and [jq](https://jqlang.org), and `search.sh`, a script that uses both. On a base image, nearly all of the image is the base.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 6 conda packages, 4.0 MiB to download: one layer by default, 2 with `--layers grouped`.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/05-native-binary/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/05-native-binary/pixi.lock -p linux-64 \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --add examples/projects/pixi/05-native-binary/search.sh:/usr/local/bin/search \
  --entrypoint search \
  -t 05-native-binary:latest -o 05-native-binary.tar
pixi oci check 05-native-binary.tar
pixi oci inspect 05-native-binary.tar
```

## The application

`search.sh` counts the lines matching a pattern in each file, as JSON: `rg --json` finds them and `jq` shapes them. It goes to `/usr/local/bin/search`, a file to a file. It is executable in the image because it starts with `#!`, whatever its mode in your checkout, so a Windows checkout gives the same layer. `#!/bin/sh` comes from the base image.

`registry.access.redhat.com/ubi9/ubi-minimal` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 05-native-binary.tar
podman run --rm -v "$PWD/docs:/work" -w /work 05-native-binary:latest pixi
```

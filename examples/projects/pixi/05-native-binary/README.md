# Native binaries

Compiled tools only, no python: [ripgrep](https://github.com/BurntSushi/ripgrep) and [jq](https://jqlang.org). The environment is a handful of packages and two layers; on a base image, nearly all of the image is the base.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 6 conda packages, 4.0 MiB to download, planned into 2 layers.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/05-native-binary/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/05-native-binary/pixi.lock -p linux-64 \
  --base registry.access.redhat.com/ubi9/ubi-minimal@sha256:baafce2d34783a28b4a472779f304e97e00f9d35c99553bc53b068b14344d9a9 \
  --entrypoint rg -t 05-native-binary:latest -o 05-native-binary.tar
pixi oci check 05-native-binary.tar
```

`debian:bookworm-slim` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 05-native-binary.tar
podman run --rm 05-native-binary:latest --version
```

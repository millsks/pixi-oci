# Node.js tools

Node.js 22 with npm and pnpm.

`pixi.toml` lists what the environment asks for; `pixi.lock` is what pixi locked for `linux-64` and `linux-aarch64`, written by `pixi lock` and never edited. On `linux-64` it is 11 conda packages, 51.8 MiB to download, planned into 6 layers.

## Build it

From the repository's root:

```sh
pixi oci plan --lockfile examples/projects/pixi/07-node-tools/pixi.lock -p linux-64
pixi oci build --lockfile examples/projects/pixi/07-node-tools/pixi.lock -p linux-64 \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --entrypoint node -t 07-node-tools:latest -o 07-node-tools.tar
pixi oci check 07-node-tools.tar
```

`registry.access.redhat.com/ubi9/ubi-minimal` works as the base too ([Base images](../../../../docs/base-images.md)). Then, where a container runtime is installed:

```sh
podman load -i 07-node-tools.tar
podman run --rm 07-node-tools:latest --version
```

## What `check` reports

npm's node-gyp ships python scripts (`#!/usr/bin/env python3`) and corepack ships PowerShell shims (`#!/usr/bin/env pwsh`); `check` reports both as `shebang` problems, which matter only if you run them. `check` also finds a conda build path left in `libnode.so`'s embedded strings, which the package does not declare for replacement: harmless at run time. On `ubi9/ubi-minimal`, which has no perl, openssl's perl scripts are reported too; `debian:bookworm-slim` has perl.

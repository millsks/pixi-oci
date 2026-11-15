# Base images

conda packages bring almost everything a program needs, but not the C library: conda-forge builds against the
system's glibc and expects the image to have one, along with its loader (`/lib64/ld-linux-x86-64.so.2`) and usually
`/bin/sh` and `/usr/bin/env` for scripts. An image built without a base has none of these, which is why
[`pixi oci check`](cli.md#pixi-oci-check) reports a `glibc` problem for it.

`--base` builds the environment on an image that has them:

```sh
pixi oci build --base registry.access.redhat.com/ubi9/ubi-minimal@sha256:baafce2d34783a28b4a472779f304e97e00f9d35c99553bc53b068b14344d9a9 \
  --entrypoint python -o app.tar
```

Any image works, from any registry. These two are the ones the project documents and tests in CI, built on and run
for `linux-64` and `linux-aarch64`:

| | Red Hat UBI 9 minimal | Debian 12 slim |
|---|---|---|
| Image | `registry.access.redhat.com/ubi9/ubi-minimal` | `debian:bookworm-slim` |
| glibc | 2.34 | 2.36 |
| Shell, `env` | yes | yes |
| Package manager | `microdnf` | `apt` |
| Perl | no | `perl-base` |
| Size, compressed | about 38 MiB | about 28 MiB |
| Pinned in CI | `sha256:baafce2d34783a28b4a472779f304e97e00f9d35c99553bc53b068b14344d9a9` | `sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc` |

UBI images are freely redistributable and supported by Red Hat, which suits places where RHEL and OpenShift are the
standard; Debian slim is the common base for Python images. Both have a newer glibc than conda-forge needs (2.17, or
2.28 for its newer builds). Smaller bases work too: `ubi9/ubi-micro` (no package manager) or
`gcr.io/distroless/cc-debian12` (no shell, so `check` will name the scripts that need one).

## Pinned by digest

A base is always given with its digest, so the same lockfile and the same base build the same image, today and in a
year. A tag alone is refused. Find a tag's digest with any of:

```sh
crane digest debian:bookworm-slim
skopeo inspect docker://debian:bookworm-slim --format '{{.Digest}}'
docker buildx imagetools inspect debian:bookworm-slim
```

The digest can be an index's, covering several platforms, or one platform's manifest. pixi-oci follows an index to
the manifest for the platform being built, and refuses a base that has none for it.

## What the image gets from the base

The base's layers go first, as they were pulled, and the environment's on top. The config starts from the base's,
as a Dockerfile's `FROM` does:

| | In the image |
|---|---|
| `PATH` | the environment's `bin`, then the base's `PATH` |
| other variables | the base's, with any that pixi-oci sets (`CONDA_PREFIX`, `--env`) replacing the base's of the same name |
| entrypoint and command | `--entrypoint` drops the base's command; `--cmd` alone keeps the base's entrypoint; neither keeps both |
| working directory | `--workdir`, else the base's |
| user, ports, stop signal | the base's |
| labels | the base's, then `dev.pixi.environment`, `dev.pixi.platform` and `dev.pixi.base` (the base's reference) |
| history | the base's, then a line per environment layer |

The manifest records the base with the image spec's own annotations, `org.opencontainers.image.base.name` and
`org.opencontainers.image.base.digest`. The base's layers count toward `--max-layers`.

## Registries and credentials

Bases are pulled over the registry API with no Docker or Podman, anonymously when the registry allows it. For a
private registry, pixi-oci uses the credentials `docker login` and `podman login` keep, in this order:
`REGISTRY_AUTH_FILE`, podman's `auth.json`, then docker's `config.json`, including its credential helpers
(`credHelpers`, `credsStore`). Proxies come from `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY`.

Every manifest, config and layer is checked against its digest and kept in the cache (`--cache-dir`) under
`blobs/sha256`, so a base is downloaded once.

# Common use cases

Recipes for the jobs pixi-oci is built for, each one complete enough to copy. They assume a pixi workspace with a
`pixi.lock` in the current directory and pixi-oci [installed](installation.md). The base images are pinned to the
digests CI tests; `pixi oci digest IMAGE:TAG --pinned` prints a current one ([Base images](base-images.md)).

| I want to | Recipe |
|---|---|
| Run a Python app in a container | [An application image](#an-application-image) |
| Keep the image's settings beside the code | [An image defined in pixi.toml](#an-image-defined-in-pixitoml) |
| Start the app the way `pixi run` does | [A pixi task as the command](#a-pixi-task-as-the-command) |
| Try a base without looking up its digest | [Trying a base by tag](#trying-a-base-by-tag) |
| Run it locally with Docker or Podman | [Load and run it](#load-and-run-it) |
| Know it will run before shipping it | [Check it without running it](#check-it-without-running-it) |
| Build and push in CI | [GitHub Actions](#github-actions) and [GitLab CI](#gitlab-ci) |
| Push to Artifactory, Nexus or Harbor | [A private registry](#a-private-registry) |
| Build on a Windows laptop without admin rights | [Windows](#windows) |
| Install from a private conda channel | [Private channels](#private-channels) |
| Keep pushes small for a large environment | [A large environment](#a-large-environment) |
| Build for ARM servers | [Another architecture](#another-architecture) |
| Look at an image someone else built | [Inspect any image](#inspect-any-image) |
| Prove a build is reproducible | [The same digest twice](#the-same-digest-twice) |

## An application image

The common case: a base image for glibc and a shell, the pixi environment in one layer, and your code in one layer
above it. A commit that changes only code changes only the top layer.

```sh
pixi oci build -e prod -p linux-64 \
  --base debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc \
  --add src:/app \
  --workdir /app --entrypoint python --cmd "main.py" \
  --expose 8000 --user 1000:1000 \
  -t myapp:1.0 -o myapp.tar
```

- `-e prod` picks the environment: keep test and lint tools in other features, so they stay out of the image.
- `--add src:/app` leaves out what `.gitignore`, `.dockerignore` and `.pixi-oci-ignore` ignore, and `.git` and
  `.pixi` always ([Your application's files](cli.md#your-applications-files)).
- The code is owned by root and the app runs as `--user 1000:1000`, so it cannot rewrite itself.

## An image defined in pixi.toml

Once the flags settle, move them into the manifest. The image is reviewed with the code, and a laptop and CI build
it the same way:

```toml title="pixi.toml"
[tool.pixi-oci.images.api]
environment = "prod"
platform = "linux-64"
base = "debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc"
add = ["src:/app"]
task = "serve"
expose = [8000]
user = "1000:1000"
tag = "ghcr.io/team/api:latest"
```

```sh
pixi oci build api                         # writes api.tar
pixi oci build api -t ghcr.io/team/api:1.4.0 # a flag wins over its key
pixi oci build                             # every image the file defines
```

Prefer a file of its own, or run pixi-oci outside a pixi workspace? The same keys go in `pixi-oci.toml` as
`[images.api]` ([Config files](config.md)).

## A pixi task as the command

If `pixi run serve` starts the app, `--task serve` makes it the image's entrypoint, with the task's `cwd` as the
working directory and its `env` set:

```toml title="pixi.toml"
[tasks]
serve = { cmd = "uvicorn main:app --host 0.0.0.0 --port 8000", cwd = "app" }

[activation.env]
PYTHONUNBUFFERED = "1"
```

```sh
pixi oci build -p linux-64 --base debian:bookworm-slim@sha256:734a... --add app:/app --task serve -t api:1.0 -o api.tar
```

`[activation.env]` variables are set in every image, as `pixi run` sets them. A task with `depends-on` is refused,
since nothing runs before an entrypoint ([A pixi task as the entrypoint](cli.md#a-pixi-task-as-the-entrypoint)).

## Trying a base by tag

While choosing a base, skip the digest and let pixi-oci look the tag up:

```sh
pixi oci build -p linux-64 --base debian:bookworm-slim --allow-unpinned --add src:/app --entrypoint python -o try.tar
```

It warns, and prints the digest it used; pin that once you settle on the base ([A base by tag](base-images.md#a-base-by-tag)).

## Load and run it

The `.tar` is an OCI archive every container tool reads:

```sh
docker load -i myapp.tar && docker run --rm -p 8000:8000 myapp:1.0
podman load -i myapp.tar && podman run --rm -p 8000:8000 myapp:1.0
skopeo copy oci-archive:myapp.tar docker-daemon:myapp:1.0
```

pixi-oci itself never needs Docker or Podman; they are only one way to run the result.

## Check it without running it

On any OS, before a container runtime is anywhere near it:

```sh
pixi oci validate myapp.tar   # the OCI spec: every blob, digest and size
pixi oci check myapp.tar      # will it run: architecture, glibc, libraries, shebangs, the entrypoint
pixi oci inspect myapp.tar    # settings and layers, with the packages each one holds
```

`check` exits `1` on a problem, so it gates a pipeline. The usual one is `glibc`: the image has no base
([Troubleshooting](troubleshooting.md)).

## GitHub Actions

Build, check, and push to GHCR with the workflow's own token. No Docker setup step, no privileged runner:

```yaml title=".github/workflows/image.yml"
on:
  push:
    branches: [main]

jobs:
  image:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4
      - uses: prefix-dev/setup-pixi@v0.8.1
      - name: Install pixi-oci
        run: |
          gh release download v0.1.0-rc.2 --repo millsks/pixi-oci --pattern 'pixi-oci-*-x86_64-unknown-linux-musl.tar.gz' --output pixi-oci.tar.gz
          tar -xzf pixi-oci.tar.gz --strip-components 1 -C ~/.pixi/bin --wildcards '*/pixi-oci'
        env:
          GH_TOKEN: ${{ github.token }}
      - run: pixi oci build api -t ghcr.io/${{ github.repository }}:${{ github.sha }} -o api.tar
      - run: pixi oci check api.tar
      - run: pixi oci push api.tar ghcr.io/${{ github.repository }}:${{ github.sha }}
        env:
          PIXI_OCI_REGISTRY_USERNAME: ${{ github.actor }}
          PIXI_OCI_REGISTRY_PASSWORD: ${{ secrets.GITHUB_TOKEN }}
```

Without `SOURCE_DATE_EPOCH` the image is dated 1970 and its digest depends only on its inputs. Set it to the commit's
time (`SOURCE_DATE_EPOCH=$(git log -1 --format=%ct)`) to date the config; the layers keep their digests either way.

## GitLab CI

The same on a plain runner image, pushing to GitLab's registry:

```yaml title=".gitlab-ci.yml"
image:
  name: ghcr.io/prefix-dev/pixi:latest

build-image:
  script:
    - curl -fsSL https://github.com/millsks/pixi-oci/releases/download/$PIXI_OCI_VERSION/pixi-oci-${PIXI_OCI_VERSION#v}-x86_64-unknown-linux-musl.tar.gz
        | tar -xz -C /tmp
    - mv /tmp/pixi-oci-*/pixi-oci /usr/local/bin/
    - pixi oci build api -t $CI_REGISTRY_IMAGE:$CI_COMMIT_SHA -o api.tar
    - pixi oci check api.tar
    - pixi oci push api.tar $CI_REGISTRY_IMAGE:$CI_COMMIT_SHA
  variables:
    PIXI_OCI_VERSION: v0.1.0-rc.2
    PIXI_OCI_REGISTRY_USERNAME: $CI_REGISTRY_USER
    PIXI_OCI_REGISTRY_PASSWORD: $CI_REGISTRY_PASSWORD
```

The Linux binaries are static, so they run on any runner image, Alpine included; the image needs `curl` and `tar`.

## A private registry

Log in once with pixi's credential store, then push. Artifactory, Nexus, Harbor, Quay, ECR and ACR all speak the
same registry API:

```sh
pixi auth login artifactory.example.com --username me --password "$TOKEN"
pixi oci push myapp.tar artifactory.example.com/docker-local/myapp:1.0
```

When a proxy in front of the registry caps a request's size (nginx's `client_max_body_size`, a corporate gateway),
large layers fail while small ones go through. Send them in pieces:

```sh
pixi oci push myapp.tar artifactory.example.com/docker-local/myapp:1.0 --chunk-size 64M
```

A base image from the same registry works too: `--base artifactory.example.com/docker-remote/debian:bookworm-slim@sha256:...`.
Its layers are then mounted on push rather than uploaded again ([`pixi oci push`](cli.md#pixi-oci-push)).

## Windows

From PowerShell, cmd or Git Bash, with no admin rights, WSL or VM. The image is byte for byte the one Linux builds:

```powershell
pixi oci build api
pixi oci check api.tar
pixi oci push api.tar ghcr.io/team/api:dev
```

Check scripts out with LF line endings (`* text=auto eol=lf` in `.gitattributes`): a `#!/bin/sh` line ending in
`\r` does not run, and `check` reports it ([Building on Windows](windows.md)).

## Private channels

Builds download with pixi's own credentials, so if `pixi install` works, so does `pixi oci build`. Nothing about
them reaches the image:

```sh
pixi auth login conda.example.com --token "$TOKEN"
pixi oci build api
```

Mirrors and `s3://` or `gcs://` channels configured for pixi are used as pixi uses them
([Private channels](private-channels.md)).

## A large environment

One environment layer is right for most images. For an environment of gigabytes that changes often (a PyTorch or
CUDA stack where one package moves at a time), group it so an upgrade re-uploads only the layers that hold the change:

```sh
pixi oci plan -e gpu -p linux-64 --layers grouped   # see the layers first, nothing downloaded
pixi oci build -e gpu -p linux-64 --layers grouped --base ... -t trainer:1.0 -o trainer.tar
```

Packages of 4 MiB or more get a layer each and small ones are grouped at points their names fix, so the same lockfile
always gives the same layers ([How layers are planned](how-it-works.md)). Give the build more memory with
`--max-memory 2G` to write more layers at once; the image is the same.

## Another architecture

Build for ARM from an x86 laptop, or the other way round. Nothing runs during a build, so no emulation is needed:

```sh
pixi oci build -p linux-aarch64 --base debian:bookworm-slim@sha256:734a... -t myapp:1.0-arm64 -o myapp-arm64.tar
pixi oci check myapp-arm64.tar
```

The lockfile must lock `linux-aarch64`: add it to `platforms` in `pixi.toml` and run `pixi lock`. A base pinned to
an index's digest serves every platform the index has.

## Inspect any image

`inspect`, `validate` and `check` take a registry reference as well as a file; `inspect` downloads no layer:

```sh
pixi oci inspect ghcr.io/team/api:1.4.0
pixi oci check ghcr.io/team/api:1.4.0 -p linux-aarch64
pixi oci pull ghcr.io/team/api:1.4.0 -o api.tar   # a file to keep, no image store
```

## The same digest twice

The same lockfile, base, files and flags give the same image digest on any machine, so a build on a laptop can be
compared with the one CI pushed:

```sh
pixi oci build api -o a.tar
pixi oci build api -o b.tar
pixi oci inspect a.tar --format json | jq -r .manifest.digest
pixi oci inspect b.tar --format json | jq -r .manifest.digest   # the same
```

Files in every layer are dated 1970 and owned by root; `SOURCE_DATE_EPOCH` dates only the image config, so the
environment and application layers keep their digests across days.

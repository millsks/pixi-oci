# Command-line reference

pixi runs `pixi-oci` for `pixi oci`, passing everything after `oci`, so `pixi oci build -e app` and
`pixi-oci build -e app` are the same command.

## `pixi oci plan`

Shows the layers an image of an environment would be built from, without downloading anything.

| Option | Default | |
|---|---|---|
| `--lockfile PATH` | `pixi.lock` | the lockfile to read |
| `-e, --environment NAME` | `default` | the environment to put in the image |
| `-p, --platform NAME` | Linux on the host's architecture | a `linux-*` conda platform: `linux-64`, `linux-aarch64`, `linux-ppc64le`, ... |
| `--layers grouped\|per-package\|single` | `grouped` | how to split the environment into layers: packages of 4 MiB or more alone and smaller ones grouped, a layer per package, or the whole environment in one layer. See [Planning the layers](how-it-works.md#planning-the-layers). |
| `--max-layers N` | `100` | the most layers to use; past it, the layers of the packages fewest others depend on are merged into the last conda layer. At least 2, except with `--layers single`. |
| `--packages` | | list the packages of every layer that holds several |
| `--format text\|json` | `text` | `json` carries every package's kind, URL, sha256 and size |

The default platform is `linux-aarch64` on an ARM machine (an Apple Silicon Mac, an ARM server) and `linux-64`
everywhere else.

## `pixi oci build`

Builds an image of an environment. It takes `plan`'s `--lockfile`, `-e`, `-p`, `--layers` and `--max-layers`, and:

| Option | Default | |
|---|---|---|
| `-o, --output PATH` | `image.tar` | a `.tar` archive, for `docker load`, `podman load` and `skopeo copy oci-archive:`; any other path is an OCI image layout directory |
| `--force` | | replace the output if it exists |
| `-t, --tag NAME[:TAG]` | the environment's name, `:latest` | the image's name: lowercase, optionally with a registry (`quay.io/team/app:1.0`) |
| `--prefix PATH` | `/opt/env` | where the environment lives in the image; an absolute path |
| `--env KEY=VALUE` | | an environment variable in the image; repeat for more |
| `--entrypoint COMMAND` | | the entrypoint: a JSON array (`'["python", "-m", "http.server"]'`) or words (`python -m http.server`) |
| `--cmd ARGS` | | the default arguments, in the same two forms |
| `--workdir PATH` | | the working directory processes start in |
| `--label KEY=VALUE` | | a label in the image config; repeat for more. pixi-oci's own `dev.pixi.*` labels win |
| `--annotation KEY=VALUE` | | an annotation on the manifest, such as `org.opencontainers.image.source`; repeat for more |
| `--user USER` | the base's, else root | the user processes run as: `NAME`, `UID`, `NAME:GROUP` or `UID:GID` |
| `--expose PORT[/PROTOCOL]` | | a port the image listens on, `tcp` unless `udp` or `sctp` is given; repeat for more; added to the base's |
| `--stop-signal SIGNAL` | the base's | the signal that stops the container: `SIGTERM`, `TERM` or a number |
| `--base IMAGE@DIGEST` | | an image to build on, pinned by digest, for glibc and a shell: its layers go under the environment's and its config is where the image's starts ([Base images](base-images.md)) |
| `--max-memory SIZE` | `512M` | roughly how much memory the build's buffers may use, such as `128M` or `2G`; at least `16M`. Less means fewer layers at once and large files waiting on disk; the image is the same. |
| `--cache-dir PATH` | see below | where downloaded package archives are kept, named by their sha256 |

What a Dockerfile would say, `build` takes as options:

| Dockerfile | `pixi oci build` |
|---|---|
| `FROM image@sha256:...` | `--base image@sha256:...` |
| `ENV KEY=VALUE` | `--env KEY=VALUE` |
| `ENTRYPOINT`, `CMD` | `--entrypoint`, `--cmd` |
| `WORKDIR` | `--workdir` |
| `LABEL KEY=VALUE` | `--label KEY=VALUE` |
| `USER` | `--user` |
| `EXPOSE 8000` | `--expose 8000` |
| `STOPSIGNAL` | `--stop-signal` |
| (manifest annotations, from `docker buildx --annotation`) | `--annotation KEY=VALUE` |

`HEALTHCHECK`, `SHELL` and `ONBUILD` are Docker's own, not part of the OCI image spec, and container runtimes such
as podman and Kubernetes ignore them. `RUN`, `COPY` and `ADD` have no equivalent: the image is the environment the
lockfile describes ([#40](https://github.com/millsks/pixi-oci/issues/40) is about adding application files).

GHCR links a package to its repository by the `org.opencontainers.image.source` label or annotation, and shows
`org.opencontainers.image.description`:

```sh
pixi oci build ... \
  --label org.opencontainers.image.source=https://github.com/team/app \
  --annotation org.opencontainers.image.description="The app's API"
```

The image always sets `PATH=<prefix>/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` and
`CONDA_PREFIX=<prefix>`; `--env` adds to them. It is labelled `dev.pixi.environment` and `dev.pixi.platform`.

Progress goes to stderr, one line per layer as it starts. The summary goes to stdout:

```text
Wrote app:1.0 to app.tar
  manifest sha256:9ec8407e4f1860919cd8dd7af6027a9bdcb91a5a008b2c5ea657ed3f300044ef
  7 layers, 62.6 MiB for linux/amd64
```

## `pixi oci validate`

Checks an image against the [OCI image spec](https://github.com/opencontainers/image-spec), on any OS, running nothing.

```console
$ pixi oci validate app.tar
app.tar is a valid OCI image
  manifest sha256:9ec8407e4f18 for linux/amd64: 7 layers, 62.6 MiB
```

| Argument or option | Default | |
|---|---|---|
| `IMAGE` | | an OCI image layout directory, or a `.tar` archive of one, read in place |
| `--format text\|json` | `text` | `json` gives `valid`, each manifest checked, and each problem as `at` and `message` |

It checks:

- `oci-layout` and `index.json`, and any index nested in it
- every manifest and config: schema version, media types, and that the index lists each with its own media type
- every blob: present, and its sha256 and size the ones its descriptor names
- every layer: a layer media type, and for gzip and uncompressed layers, that its uncompressed tar hashes to the
  config's `diff_id` for it
- the config: as many `diff_ids` and history entries as layers, and the same platform as the index
- `org.opencontainers.image.ref.name`, by the spec's grammar
- for an archive, that the `manifest.json` `docker load` reads names the same config and layers

It reports every problem it finds, not just the first, and exits `1` if there is any. zstd layers are hashed but
their `diff_id` is not checked.

The image is a standard OCI image, so other tools check it the same way: `crane validate --tarball app.tar`, or
`skopeo copy oci-archive:app.tar ...` as a smoke test.

## `pixi oci check`

Checks that an image will run, by Linux's rules, without running it, so it works on Windows and macOS too. It runs
[`validate`](#pixi-oci-validate) first, and looks at the files only if the image is valid.

```console
$ pixi oci check app.tar
app.tar: 4 problems in 5195 files (134 ELF files, 56 scripts)
  glibc: glibc is not in the image: 129 ELF files need /lib64/ld-linux-x86-64.so.2, ld-linux-x86-64.so.2, libc.so.6, libdl.so.2, libm.so.6, libpthread.so.0, librt.so.1, libutil.so.1
    /opt/env/bin/bunzip2, /opt/env/bin/bzcat, /opt/env/bin/bzip2, and 126 more
    help: conda packages use the system's glibc: build on a base image that has it (`--base`, #12), or add the loader and libraries in a layer below the environment
  library: libX11.so.6 is needed by 1 ELF file, but the loader would not find it
    /opt/env/lib/libtk8.6.so
  shebang: 20 scripts start with #!/bin/sh, which is not in the image
    /opt/env/bin/bzdiff, /opt/env/bin/bzgrep, /opt/env/bin/bzmore, and 17 more
  ...
  note: entrypoint: the image sets no entrypoint or cmd, so a container needs a command
```

| Argument or option | Default | |
|---|---|---|
| `IMAGE` | | an OCI image layout directory, or a `.tar` archive of one, read in place |
| `-p, --platform NAME` | the only one | for an image of several platforms, which one: `linux/arm64/v8`, or `linux-aarch64` |
| `--format text\|json` | `text` | `json` gives `ok`, what was checked, and each problem and note with every path it affects |

It applies the layers in order, whiteouts included, and then:

| Problem | What it means |
|---|---|
| `architecture` | an ELF file is for another machine than the image's platform |
| `glibc` | ELF files need a loader or glibc libraries no layer provides: an image without a base has none |
| `library` | a library an ELF file needs (`DT_NEEDED`) is not where the loader would look: its `RPATH` or `RUNPATH` (with `$ORIGIN`), the image's `LD_LIBRARY_PATH`, then `/lib`, `/usr/lib`, `/lib64`, `/usr/lib64` and the multiarch directories |
| `shebang` | an executable script's `#!` interpreter is not in the image; for `#!/usr/bin/env prog`, `prog` is looked for on the image's `PATH` |
| `placeholder` | a file still holds a conda build placeholder, so it was not rewritten for the image's prefix (`conda-meta` records and `__pycache__` caches excepted) |
| `entrypoint` | the entrypoint, or cmd when there is none, is not on the image's `PATH`, or is not executable |
| `elf` | a file starts like an ELF file but cannot be read as one |
| `spec` | `validate` found a problem; the files were not looked at |

Symlinks are followed as the kernel follows them. It exits `1` if there is any problem; a note, such as an image
with no entrypoint, does not fail it. ELF files are parsed in memory up to 64 MiB, and from a temporary file beyond.

## `pixi oci pull`

Downloads an image from a registry into a `.tar` archive or an OCI image layout directory, with no Docker, Podman
or local registry: pixi-oci keeps no image store, so a pulled image is a file, like a built one.

```console
$ pixi oci pull debian:bookworm-slim
Pulled docker.io/library/debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc to debian.tar
  manifest sha256:ec8534cda2cb30f29f44f23e135297af7a1496f2990fd8e4e84aa36b081a757a
  1 layer, 26.8 MiB for linux/arm64/v8
```

| Argument or option | Default | |
|---|---|---|
| `REFERENCE` | | `[registry/]repository[:tag][@digest]`; a tag is resolved to the digest the registry serves |
| `-o, --output PATH` | `<repository>.tar` | a `.tar` archive, or any other path for an OCI image layout directory |
| `--force` | | replace the output if it exists |
| `-p, --platform NAME` | Linux on the host's architecture | which image of an index to pull: `linux-64`, `linux-aarch64`, ... |
| `--cache-dir PATH` | as for `build` | where downloaded blobs are kept |
| `--format text\|json` | `text` | `json` gives the pinned reference, digest, platform, layers and size |

Every manifest, config and layer is checked against its digest, and the manifest and config are written as they
were served, so the pulled image keeps the registry's digest. Blobs come through the same cache as `--base`, so a
base already used for a build is not downloaded again. Credentials are as for `push`. `skopeo copy docker://...
oci-archive:...` and `crane pull` do the same, where installed.

## `pixi oci push`

Sends an image to a registry, with no Docker or Podman. It runs [`validate`](#pixi-oci-validate) first, then asks the
registry for each blob the image needs (its config, then its layers) and sends only the ones it lacks; the manifest
goes last, under the tag, and the digest the registry reports must be the image's.

```console
$ pixi oci push app.tar ghcr.io/team/app:1.0
[1/9] sha256:3f2c09a1b4de already there
[2/9] sha256:9c1e77a0e5f2 uploading (2.1 MiB)
...
Pushed ghcr.io/team/app:1.0
  digest sha256:e79ba52d8f3013771e01802686c9e7ff54726be037a96e9b0e7d7424165273dc
  9 blobs: 1 uploaded (2.1 MiB), 0 mounted, 8 already there
```

| Argument or option | Default | |
|---|---|---|
| `IMAGE` | | an OCI image layout directory, or a `.tar` archive of one |
| `DESTINATION` | | `[registry/]repository[:tag]`, `latest` when no tag is given; or `repository@sha256:...` to push by digest, which must be the image's |
| `-p, --platform NAME` | the only one | for an image of several platforms, which one to push |
| `--format text\|json` | `text` | `json` gives the reference, digest, and how many blobs were uploaded, mounted and already there |

Because layers are named by their digest, pushing a rebuilt image sends only the layers that changed: change one
package and a push sends its layer and the config, nothing else. When the image was built on a base that lives on
the same registry, the base's layers are mounted from its repository rather than uploaded.

**Credentials.** pixi-oci uses pixi's own: log in once with `pixi auth login`, which stores them in the operating
system's keychain (or `~/.rattler/credentials.json`), on Windows, macOS and Linux alike. No Docker or Podman is needed
or read.

```sh
pixi auth login ghcr.io --username <your-user> --password <token>
pixi oci push app.tar ghcr.io/<your-user>/app:1.0
```

For GHCR the token is a GitHub personal access token with `write:packages`; `pixi auth login ghcr.io --token <token>`
works as well. In CI, set `PIXI_OCI_REGISTRY_USERNAME` and `PIXI_OCI_REGISTRY_PASSWORD` instead: they are given only
to the push's destination, or to `PIXI_OCI_REGISTRY` when that is set, never to another registry, such as a base
image's. In GitHub Actions:

```yaml
permissions:
  contents: read
  packages: write
steps:
  - run: pixi oci push app.tar ghcr.io/${{ github.repository }}:${{ github.sha }}
    env:
      PIXI_OCI_REGISTRY_USERNAME: ${{ github.actor }}
      PIXI_OCI_REGISTRY_PASSWORD: ${{ secrets.GITHUB_TOKEN }}
```

## `pixi oci inspect`

Shows what an image holds, read from its index, manifest and config, without opening a layer or needing skopeo or
crane.

```console
$ pixi oci inspect app.tar
Image app:1.0 (app.tar)
  platform    linux/amd64
  manifest    sha256:9ec8407e4f1860919cd8dd7af6027a9bdcb91a5a008b2c5ea657ed3f300044ef
  config      sha256:6ca92a53b6bf0bf0fc05e09b3e22b662af269f9136ff892344073894a1ad1312
  created     1970-01-01T00:00:00Z
  env         PATH=/opt/env/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
              CONDA_PREFIX=/opt/env
  labels      dev.pixi.environment=app
              dev.pixi.platform=linux-64

7 layers, 62.6 MiB

  #  Layer              Size      Digest
  1  4 packages         2.1 MiB   sha256:960cbb64893f
  2  libpython 3.13.18  13.9 MiB  sha256:12f0c952f0ec
  3  12 packages        8.3 MiB   sha256:cbef1ccd8e85
  4  tk 8.6.13          4.0 MiB   sha256:74ddf65a7bef
  5  3 packages         1.7 MiB   sha256:a8fb83f0c699
  6  python 3.13.18     32.6 MiB  sha256:a6cd4f82fa1e
  7  six 1.17.0         11.5 KiB  sha256:f0d9c9d4dea1
```

| Argument or option | Default | |
|---|---|---|
| `IMAGE` | | an OCI image layout directory, or a `.tar` archive of one, read in place |
| `-p, --platform NAME` | the only one | for an image of several platforms, which one: `linux/arm64/v8`, or the conda name `linux-aarch64` |
| `--packages` | | list the packages of every layer that holds several |
| `--format text\|json` | `text` | `json` gives every field, and each layer's digest, size, media type, `diff_id`, history line and packages |
| `--raw index\|manifest\|config` | | print that stored document exactly, instead of the summary; its sha256 is its digest |

Layer sizes are the compressed sizes the registry stores. The packages of each layer come from the history line
pixi-oci writes for it; a layer another tool built shows its own history line instead.

Other OCI tools read the same image: `skopeo inspect oci-archive:app.tar`, `skopeo inspect --config
oci-archive:app.tar`, or `crane config` and `crane manifest` on a pushed image.

## Environment variables

| Variable | |
|---|---|
| `PIXI_OCI_CACHE_DIR` | the package cache, when `--cache-dir` is not given. Otherwise `$XDG_CACHE_HOME/pixi-oci`, `~/.cache/pixi-oci` on Linux, `~/Library/Caches/pixi-oci` on macOS, `%LOCALAPPDATA%\pixi-oci` on Windows |
| `SOURCE_DATE_EPOCH` | seconds since the Unix epoch, stamped on every file and on the image's `created` time; `0` when unset. The [reproducible-builds](https://reproducible-builds.org/docs/source-date-epoch/) convention. |
| `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY` | the proxy for downloads |
| `PIXI_OCI_REGISTRY_USERNAME`, `PIXI_OCI_REGISTRY_PASSWORD` | credentials for one registry, for CI: `push`'s destination, or `PIXI_OCI_REGISTRY` |
| `PIXI_OCI_REGISTRY` | the registry those credentials are for, such as `ghcr.io`; set it when a build should also use them to pull its base |
| `PIXI_OCI_NETWORK_TESTS` | for development only: `1` adds the tests that download from conda-forge |

## Exit codes

| Code | |
|---|---|
| `0` | success |
| `1` | the command failed, or `validate` found a problem; the message says why and usually what to do ([Troubleshooting](troubleshooting.md)) |
| `2` | the command line was wrong: an unknown option or a value that does not parse |

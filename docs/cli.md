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
| `--max-memory SIZE` | `512M` | roughly how much memory the build's buffers may use, such as `128M` or `2G`; at least `16M`. Less means fewer layers at once and large files waiting on disk; the image is the same. |
| `--cache-dir PATH` | see below | where downloaded package archives are kept, named by their sha256 |

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
| `PIXI_OCI_NETWORK_TESTS` | for development only: `1` adds the tests that download from conda-forge |

## Exit codes

| Code | |
|---|---|
| `0` | success |
| `1` | the command failed, or `validate` found a problem; the message says why and usually what to do ([Troubleshooting](troubleshooting.md)) |
| `2` | the command line was wrong: an unknown option or a value that does not parse |

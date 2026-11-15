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
| `1` | the command failed; the message says why and usually what to do ([Troubleshooting](troubleshooting.md)) |
| `2` | the command line was wrong: an unknown option or a value that does not parse |

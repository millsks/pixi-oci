# Config files

Every `build` option can live in a file instead of on the command line, as a named image. The image is then defined
beside `pixi.lock`, reviewed with the code, and built the same way on a laptop and in CI:

```toml title="pixi.toml"
[tool.pixi-oci.images.api]
environment = "prod"
platform = "linux-64"
base = "debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc"
tag = "ghcr.io/team/api:1.4.0"
add = ["app:/app", { src = "config/api.toml", dest = "/etc/api/" }]
task = "serve"
env = { LOG_LEVEL = "info" }
expose = [8000]
user = "1000:1000"
labels = { "org.opencontainers.image.source" = "https://github.com/team/api" }
```

```sh
pixi oci build api        # the image named api
pixi oci build            # every image the file defines
pixi oci plan api         # its plan
```

## Where pixi-oci looks

pixi-oci runs as `pixi oci` and as a `pixi-oci` binary of its own, so the settings can live in pixi's manifest or in
a file of their own:

1. **`--config PATH`**: a pixi-oci file named on the command line. When given, it is the only file read.
2. **The default:** a `[tool.pixi-oci]` table in the workspace manifest beside the lockfile: `pixi.toml`, or
   `pyproject.toml` (beside its `[tool.pixi]`) when there is no `pixi.toml`. pixi leaves `[tool.*]` tables to other
   tools.
3. **The fallback:** `pixi-oci.toml` beside the lockfile, read when the manifest defines no images. It has the same
   keys without the prefix: `[images.api]`.

Beside the lockfile means the directory of `--lockfile`, the current directory by default. When the manifest and
`pixi-oci.toml` both define images, the build stops and names both files, rather than quietly use one. With neither,
there is no config: flags and the built-in defaults, as always. A build says which file it read, on stderr:
`image api from pixi.toml`.

## Keys

Every key is a `build` option, under its name without the dashes in front; a few take a table or a list where the
option is repeated:

| Key | Option | Value |
|---|---|---|
| `lockfile` | `--lockfile` | a path; `pixi.lock` beside the file by default |
| `environment` | `-e` | `"prod"` |
| `platform` | `-p` | `"linux-64"`, `"linux-aarch64"` |
| `tag` | `-t` | `"ghcr.io/team/api:1.4.0"`; the image's name by default |
| `output` | `-o` | a path; `NAME.tar` beside the file by default |
| `force` | `--force` | `true` |
| `base` | `--base` | an image pinned by digest |
| `prefix` | `--prefix` | `"/opt/env"` |
| `env` | `--env` | a table: `{ LOG_LEVEL = "info" }` |
| `entrypoint`, `cmd` | `--entrypoint`, `--cmd` | a list of words, or one command line |
| `workdir` | `--workdir` | `"/app"` |
| `task` | `--task` | a task's name |
| `add` | `--add` | a list of `"SRC:DEST"`, or `{ src = "...", dest = "..." }` |
| `labels`, `annotations` | `--label`, `--annotation` | a table |
| `user` | `--user` | `"1000:1000"`, `1000`, `"app"` |
| `expose` | `--expose` | a list: `[8000, "9000/udp"]` |
| `stop-signal` | `--stop-signal` | `"TERM"`, `15` |
| `layers`, `max-layers` | `--layers`, `--max-layers` | `"single"`, `"grouped"`, `"per-package"`; a number |
| `max-memory` | `--max-memory` | `"1G"` |
| `cache-dir` | `--cache-dir` | a path |

An unknown key or a wrong type stops the build, naming the key and the file, so a typo never goes unnoticed. Relative
paths (`lockfile`, `output`, `add`, `cache-dir`) are relative to the file's directory, wherever the build runs.

There are no credentials in a config file: registries and channels authenticate through `pixi auth login`, as
without one.

## Flags win

A flag given on the command line wins over its key, so one image can be built a little differently without editing
the file:

```sh
pixi oci build api --tag ghcr.io/team/api:dev --layers grouped
```

Lists add up: the file's `env`, `labels`, `annotations`, `expose` and `add` come first and the flags' after, and a
variable or label set twice takes the flag's value. `--output` and `--tag` name one image, so they need an image's
name when the file defines several.

The same settings give the same image: a build from a config file and the same build from flags have one digest.
The [`04-fastapi`](https://github.com/millsks/pixi-oci/tree/main/examples/projects/pixi/04-fastapi) example defines
its image in `pixi.toml`, and a test builds it both ways and compares the digests.

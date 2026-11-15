# Troubleshooting

Every error names what went wrong and, where there is one, what to do; this page has the rest. Messages are shown as
pixi-oci prints them, with the variable parts in angle brackets.

## Choosing what to build

**`the lockfile has no environment named <name>`**
:   The help line lists the environments the lockfile has. Pick one with `-e`; without `-e` it is `default`.

**`environment <name> is not locked for <platform>`**
:   Add the platform to the workspace and lock again: `pixi workspace platform add linux-64`, then `pixi lock`. The
    help line lists the platforms that are locked.

**`<platform> is not a Linux platform, so no container image can be built for it`**
:   Images carry Linux programs. Pass a `linux-*` platform with `-p`: `linux-64` for x86 servers, `linux-aarch64` for
    ARM ones. Without `-p`, pixi-oci picks Linux on your machine's architecture, so this appears only when `-p` named
    `osx-*` or `win-*`.

**`environment <name> has PyPI packages that are not wheels: <names>`**
:   pixi-oci installs wheels and builds nothing. These were locked as an sdist, a git checkout or a local path. Lock a
    version that publishes wheels for the platform (most do), get the package from conda-forge instead, or build the
    wheel and depend on it.

**`<name>'s wheel holds <path>, which its RECORD ...`**
:   The wheel's files do not match the list of hashes inside it. The wheel matched the lockfile's hash, so it was
    published this way: report it to the package.

**`environment <name> builds <n> packages from source: <names>`**
:   pixi-oci installs built conda packages. Build those with `pixi build`, publish them to a channel, and lock the
    published packages.

**`<path> is not a lockfile pixi-oci can read`**
:   Either it is not a `pixi.lock`, or a newer pixi wrote it in a format this pixi-oci predates. Update pixi-oci.

**`could not read <path>`**
:   No lockfile there. Run `pixi lock` in the workspace, or pass `--lockfile`.

## Downloading

**`could not download <url>`**
:   The cause follows the message. Behind a proxy, set `HTTPS_PROXY`. A channel that needs credentials is not
    supported yet.

**`<name> downloaded from <url> does not match the lockfile's sha256`**
:   The bytes the channel served are not the ones that were locked: the channel replaced the file, or something
    between you and it changed it. Nothing was cached or used. Lock again to take the channel's current file, but
    find out why it changed first.

**`the lockfile records no sha256 for <name>, so its download cannot be checked`**
:   A current pixi records one for every conda package. Lock again with it.

**`could not use the package cache at <path>`**
:   The cache directory is not writable, or the disk is full. Point `--cache-dir` (or `PIXI_OCI_CACHE_DIR`) somewhere
    else.

## Building

**`<path> already exists`**
:   pixi-oci does not overwrite an image unless asked: add `--force`, or pick another `--output`.

**`the image prefix must be an absolute path, not <prefix>`**
:   `--prefix` is a Linux path inside the image, such as `/opt/env`.

**`<name> is a noarch Python package, but the environment has no python to install it for`**
:   The package needs a `python` in the same environment to know where `site-packages` is. Add python to the
    environment.

**`<name>'s location in the lockfile is not a URL, so it has no repodata record to install from`**
:   The package was locked from a local path. Publish it to a channel (a local one served over `file://` works) and
    lock from there.

**`<name> would install <path>, which is outside the environment's prefix`**
:   The archive's `paths.json` names a path with `..` or a root. A real conda package never does; treat the archive
    as damaged or crafted and check where it came from.

**`<name>'s paths.json lists <path>, but its archive has no such file`** and **`<name>'s archive has no readable <file>`**
:   The archive is damaged or was built by a tool that does not follow the conda package format. Rebuild or
    re-download it.

**`<name>'s archive holds <path> as a hard link, which pixi-oci cannot place yet`**
:   A tar hard link inside a package is rare and not supported yet. Please open an issue naming the package.

**`could not write <path>`** or **`could not write the layer`**
:   Usually a full disk; the cause follows the message. A build needs room for the layers, and while a `.tar` is
    assembled, the layers twice. A failed build leaves no `.tar` at the output path, and a layout directory without
    its `index.json`, so nothing half-written passes for an image.

## Running the image

**`exec /opt/env/bin/python: no such file or directory`** from `docker run` or `podman run`
:   The program exists; its loader does not: the image has no glibc. Build it on a base image with `--base`
    ([Base images](base-images.md)). `pixi oci check` reports this, on any OS, before you load the image.

**`... is not pinned to a digest`**
:   `--base` takes an image with its digest, so the build stays reproducible. `crane digest <image>` or
    `skopeo inspect docker://<image>` prints it ([Base images](base-images.md#pinned-by-digest)).

**`... refused access to ...`** when pulling a base
:   The registry wants credentials. Log in with `docker login` or `podman login` (or set up a credential helper):
    pixi-oci reads what they store.

**`... has no image for linux/arm64/v8`**
:   The base's index has no image for the platform being built. Pick a base built for it, or build for a platform
    the base has.

**`pixi oci check` reports `library: libX11.so.6 ...`, or another library**
:   A binary in the environment links to a library no package in the environment provides. conda-forge's `tk`, for
    one, links to `libX11.so.6`, so `tkinter` needs `xorg-libx11` in the environment (`pixi add xorg-libx11`). If
    nothing in the image uses that binary, the image runs without it.

**`pixi oci` is an unknown command**
:   pixi cannot see the executable: see [Installation](installation.md#check-it).

## Still stuck

Run with `RUST_BACKTRACE=1` if it crashed, and open an issue with the command, the full output, `pixi oci --version`
and, if you can share it, the lockfile.

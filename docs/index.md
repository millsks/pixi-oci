# pixi-oci

A [pixi](https://pixi.sh) extension that builds OCI container images straight from `pixi.lock`: no Dockerfile, no
Docker or Podman, no daemon and no admin rights. The lockfile already names every package, its URL and its sha256;
pixi-oci turns exactly those packages into an image that docker, podman, skopeo, crane and OpenShift all read.

```sh
pixi oci plan  -e app -p linux-64                       # the layers, without downloading anything
pixi oci build -e app -p linux-64 --tag app:1.0 -o app.tar
podman load -i app.tar                                  # or docker load, or skopeo copy oci-archive:app.tar ...
```

!!! warning "Early days"

    `plan` and `build` work today, for environments of conda packages, on Linux, macOS and Windows. Images have no
    base image yet, so a dynamically linked program (python, most of conda-forge) needs the glibc a base provides
    before it runs in a container. Base images and pushing to a registry are next: see the [roadmap](roadmap.md).

## Why

The usual way to containerize a pixi environment is a multi-stage Dockerfile that installs pixi, runs
`pixi install`, and copies the environment into a slimmer stage. That needs a Docker daemon or a privileged builder,
downloads everything again on every cache miss, and produces layers that say nothing about which packages they hold.

pixi-oci builds from the lockfile instead:

- **Daemonless and rootless.** It writes the image itself, on a laptop with no admin rights, a plain CI runner, or an
  OpenShift build pod.
- **Windows too.** Layers are written straight from the package archives, so the host's file system never has to hold
  Linux modes or symlinks.
- **Reproducible.** The same lockfile gives the same image digest on every machine; CI checks Linux, macOS and
  Windows against each other on every change.
- **One layer per package.** Dependencies sit at the bottom, so changing one package re-uploads one layer, and images
  built from the same packages share layers in the registry.
- **Checked.** Every archive must match the sha256 the lockfile recorded before anything reads it.

## Where to go

<div class="grid cards" markdown>

-   **[Installation](installation.md)**

    A binary on your `PATH`, and how pixi finds the extension.

-   **[Your first image](first-image.md)**

    From a `pixi.toml` to an image you can inspect and load, in five commands.

-   **[Building on Windows](windows.md)**

    What works from PowerShell with no admin rights, WSL or VM, and what needs a Linux machine.

-   **[Command-line reference](cli.md)**

    Every option of `plan` and `build`, environment variables and exit codes.

-   **[How it works](how-it-works.md)**

    How layers are planned, streamed from the archives, kept reproducible, and kept within memory.

-   **[Troubleshooting](troubleshooting.md)**

    Every error message, what it means, and what to do.

</div>

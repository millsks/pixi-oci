# Roadmap

The aim: build anywhere, check anywhere, run in CI. A developer on a Windows laptop with no admin rights builds and
checks an image; CI proves the very same image runs, because the digest matches across hosts.

Each item has an issue labelled [`roadmap`](https://github.com/millsks/pixi-oci/issues?q=label%3Aroadmap), where its
scope and acceptance criteria live and its pull requests are linked. Comment there before starting on one.

## Done

- [x] **`plan`**: read `pixi.lock`, map conda platforms to OCI platforms, order layers dependencies first, cap them
      ([#6](https://github.com/millsks/pixi-oci/issues/6))
- [x] **`build`**: download and verify packages against the lockfile's sha256, rewrite paths for the image's prefix,
      and write a reproducible OCI image layout or archive ([#7](https://github.com/millsks/pixi-oci/issues/7))
- [x] **Streamed layers**: written straight from the archives, so builds run on Windows and macOS file systems, with
      memory bounded by `--max-memory`; checked against rattler's own installer file for file
      ([#8](https://github.com/millsks/pixi-oci/issues/8))
- [x] **Documentation**: this site, versioned and published from `main`
      ([#9](https://github.com/millsks/pixi-oci/issues/9))
- [x] **Grouped layers**: packages of 4 MiB or more alone and small ones grouped at cut points their names fix, so a
      large environment keeps its layers for the packages worth re-uploading; `--layers per-package` and `single` too
      ([#10](https://github.com/millsks/pixi-oci/issues/10))
- [x] **`validate`**: an image checked against the OCI image spec on any OS, every blob, digest, size and `diff_id`
      ([#25](https://github.com/millsks/pixi-oci/issues/25))
- [x] **`inspect`**: an image's settings and layers, with the packages each layer holds, as text, JSON or the exact
      stored documents ([#24](https://github.com/millsks/pixi-oci/issues/24))
- [x] **`check`**: whether an image will run, without running it, on any OS: architecture, loader and glibc,
      libraries by the loader's own search, shebangs, placeholders, the entrypoint ([#11](https://github.com/millsks/pixi-oci/issues/11))
- [x] **Base images**: `--base` builds on any image pinned by digest, pulled from any registry with docker's and
      podman's credentials; UBI 9 minimal and Debian 12 slim documented and run in CI ([#12](https://github.com/millsks/pixi-oci/issues/12))
- [x] **PyPI wheels**: installed into the top layer as pip and uv install them, checked against `pixi install`
      ([#17](https://github.com/millsks/pixi-oci/issues/17))
- [x] **Examples**: 14 real pixi workspaces, from python alone to a few hundred packages, planned on every pull
      request and built and checked in CI ([#22](https://github.com/millsks/pixi-oci/issues/22))
- [x] **Push**: `pixi oci push` sends only the blobs a registry lacks, with docker's and podman's credentials or
      scoped ones for CI; tested against `registry:2` and GHCR ([#13](https://github.com/millsks/pixi-oci/issues/13))

## Next

- [ ] **`pixi oci run`.** Rootless containers on Linux through unprivileged user namespaces, with no Docker, Podman
      or root, so CI runs the built image as a normal user. ([#14](https://github.com/millsks/pixi-oci/issues/14))

| | Windows laptop (no admin, no VM) | macOS | Linux | CI on Linux |
|---|---|---|---|---|
| `build` | yes | yes | yes | yes |
| `validate`, `inspect` | yes | yes | yes | yes |
| `check` | yes | yes | yes | yes |
| `run` | no: needs a Linux kernel | no: needs a Linux kernel | where user namespaces are allowed | yes |

## After that

- [ ] `pixi oci diff`: two images compared layer by layer, with the packages behind each change and what a rebuild
      re-uploads ([#26](https://github.com/millsks/pixi-oci/issues/26))
- [ ] Multi-platform image indexes: `linux-64` and `linux-aarch64` under one tag
      ([#15](https://github.com/millsks/pixi-oci/issues/15))
- [ ] pixi's activation variables, and an entrypoint from a pixi task
      ([#16](https://github.com/millsks/pixi-oci/issues/16))
- [ ] Private channels, with pixi's own credentials ([#18](https://github.com/millsks/pixi-oci/issues/18))
- [ ] A [pixi-sbom](https://github.com/aesir-oss/pixi-sbom) SBOM attached to the image, and
      [pixi-policy](https://github.com/aesir-oss/pixi-policy) as a gate before pushing
      ([#19](https://github.com/millsks/pixi-oci/issues/19))
- [ ] A release, and a package on conda-forge: `pixi global install pixi-oci`
      ([#20](https://github.com/millsks/pixi-oci/issues/20))

## Related projects

- [conda-docker](https://github.com/conda-incubator/conda-docker) built images from conda environments without
  Docker, and is this project's closest ancestor.
- [apko](https://github.com/chainguard-dev/apko) and [ko](https://github.com/ko-build/ko) do the same for Alpine
  packages and Go binaries; pixi-oci aims to be that for `pixi.lock`.
- [Pixitainer](https://github.com/RaphaelRibes/pixitainer) (`pixi containerize`) builds Apptainer, Singularity and
  Docker images through those tools.
- prefix.dev's rattler-oci registry (in preview) builds images on the server when they are pulled.

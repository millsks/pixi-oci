# Roadmap

The aim: build anywhere, check anywhere, run in CI. A developer on a Windows laptop with no admin rights builds and
checks an image; CI proves the very same image runs, because the digest matches across hosts.

## Done

- [x] **`plan`**: read `pixi.lock`, map conda platforms to OCI platforms, order layers dependencies first, cap them
- [x] **`build`**: download and verify packages against the lockfile's sha256, rewrite paths for the image's prefix,
      and write a reproducible OCI image layout or archive
- [x] **Streamed layers**: written straight from the archives, so builds run on Windows and macOS file systems, with
      memory bounded by `--max-memory`; checked against rattler's own installer file for file
- [x] **Grouped layers**: packages of 4 MiB or more alone and small ones grouped at cut points their names fix, so a
      large environment keeps its layers for the packages worth re-uploading; `--layers per-package` and `single` too

## Next

- [ ] **Base images and push.** `--base` pulls a glibc base image (a distroless or UBI image) by digest, so python
      and the rest of conda-forge run; `pixi oci push` sends the image to any registry, with docker-style credentials.
- [ ] **`pixi oci check`.** Static tests on any OS, nothing executed: every binary is a Linux ELF for the image's
      architecture, its loader and libraries are in the image, shebangs name interpreters that exist, no placeholder
      is left, and the entrypoint and `PATH` are right. It would flag today's missing-glibc failure.
- [ ] **`pixi oci run`.** Rootless containers on Linux through unprivileged user namespaces, with no Docker, Podman
      or root, so CI runs the built image as a normal user.

| | Windows laptop (no admin, no VM) | macOS | Linux | CI on Linux |
|---|---|---|---|---|
| `build` | yes | yes | yes | yes |
| `check` | yes | yes | yes | yes |
| `run` | no: needs a Linux kernel | no: needs a Linux kernel | where user namespaces are allowed | yes |

## After that

- [ ] PyPI wheels, installed into the top layer
- [ ] Private channels, with pixi's own credentials
- [ ] pixi's activation variables, and an entrypoint from a pixi task
- [ ] A [pixi-sbom](https://github.com/aesir-oss/pixi-sbom) SBOM attached to the image, and
      [pixi-policy](https://github.com/aesir-oss/pixi-policy) as a gate before pushing
- [ ] Multi-platform image indexes: `linux-64` and `linux-aarch64` under one tag
- [ ] A release, and a package on conda-forge: `pixi global install pixi-oci`

## Related projects

- [conda-docker](https://github.com/conda-incubator/conda-docker) built images from conda environments without
  Docker, and is this project's closest ancestor.
- [apko](https://github.com/chainguard-dev/apko) and [ko](https://github.com/ko-build/ko) do the same for Alpine
  packages and Go binaries; pixi-oci aims to be that for `pixi.lock`.
- [Pixitainer](https://github.com/RaphaelRibes/pixitainer) (`pixi containerize`) builds Apptainer, Singularity and
  Docker images through those tools.
- prefix.dev's rattler-oci registry (in preview) builds images on the server when they are pulled.

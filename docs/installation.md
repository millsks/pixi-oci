# Installation

pixi-oci is one executable, `pixi-oci` (`pixi-oci.exe` on Windows). pixi runs any `pixi-<name>` executable it finds as
`pixi <name>`, so once `pixi-oci` is on your `PATH`, `pixi oci` works.

!!! note "Prereleases only"

    pixi-oci is in release candidates, so there is no `pixi global install pixi-oci` yet. Packaging it on conda-forge
    is on the [roadmap](roadmap.md). Until then, download a binary from a release.

## A binary from a release

Every [release](https://github.com/millsks/pixi-oci/releases) has a binary for six targets, with a `SHA256SUMS` file:

| System | Archive |
|---|---|
| Linux x86_64 (any distribution: static) | `pixi-oci-VERSION-x86_64-unknown-linux-musl.tar.gz` |
| Linux ARM64 | `pixi-oci-VERSION-aarch64-unknown-linux-musl.tar.gz` |
| macOS Apple silicon | `pixi-oci-VERSION-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `pixi-oci-VERSION-x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `pixi-oci-VERSION-x86_64-pc-windows-msvc.zip` |
| Windows ARM64 | `pixi-oci-VERSION-aarch64-pc-windows-msvc.zip` |

```sh
version=0.1.0-rc.3
curl -fsSLO https://github.com/millsks/pixi-oci/releases/download/v$version/pixi-oci-$version-x86_64-unknown-linux-musl.tar.gz
tar -xzf pixi-oci-$version-x86_64-unknown-linux-musl.tar.gz --strip-components 1 -C ~/.pixi/bin pixi-oci-$version-x86_64-unknown-linux-musl/pixi-oci
pixi oci --version
```

`~/.pixi/bin` is on your `PATH` when pixi is installed; any other folder on it works too. On Windows, unzip the
archive and put `pixi-oci.exe` in `%USERPROFILE%\.pixi\bin`, which needs no admin rights. macOS marks a downloaded
binary as quarantined; `xattr -d com.apple.quarantine ~/.pixi/bin/pixi-oci` lets it run.

## A binary from CI

Between releases, every CI run on `main` and on pull requests builds release binaries for Linux, macOS and Windows in the **Image
digest** jobs and keeps them as artifacts for a few days:

1. Open the repository's **Actions** tab, then a recent **CI** run on `main`.
2. Under **Artifacts**, download `pixi-oci-ubuntu-latest`, `pixi-oci-macos-latest` or `pixi-oci-windows-latest`.
3. Unzip it and put `pixi-oci` (or `pixi-oci.exe`) in a folder on your `PATH`.

On Windows, a folder you own on your `PATH` needs no admin rights: pixi's own `%USERPROFILE%\.pixi\bin` works if pixi
is installed, or add a folder to your user `PATH` under **Edit environment variables for your account**.

## From source

Everything builds through pixi, which brings its own Rust toolchain from conda-forge:

```sh
git clone https://github.com/millsks/pixi-oci
cd pixi-oci
pixi run build                        # target/release/pixi-oci
cp target/release/pixi-oci ~/.local/bin/   # any folder on your PATH
```

!!! warning "Building from source on Windows"

    Rust on Windows links with Microsoft's C++ build tools, which usually take an administrator to install. Without
    them, use a binary from CI instead: running pixi-oci needs nothing installed.

## Check it

```sh
pixi oci --version
pixi oci --help
```

If `pixi oci` says the command is unknown, pixi cannot see the executable: run `pixi-oci --version` directly, and if
that works, the folder it is in is not on the `PATH` pixi sees.

## What it needs at run time

- **The network**, to the channels your lockfile names (conda-forge, prefix.dev, a private channel), for the packages
  that are not in the cache yet. Proxies come from the usual `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` variables.
- **Disk** for the cache of package archives (about the size of the image) and the image itself. See
  [Memory and disk](how-it-works.md#memory-and-disk).
- Nothing else: no Docker, Podman, pixi, Python or admin rights. pixi is only needed to run it as `pixi oci`;
  `pixi-oci` works on its own.

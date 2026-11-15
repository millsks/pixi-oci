# Building on Windows

pixi-oci builds Linux images on Windows with no admin rights, no WSL and no VM. It runs from PowerShell, cmd or Git
Bash alike; the shell makes no difference.

```powershell
pixi oci plan  -p linux-64
pixi oci build -p linux-64 --tag app:1.0 -o app.tar
```

## Why it works

A Linux layer holds things a Windows file system cannot: Unix file modes (which files are executable), symlinks
(Windows needs admin rights or Developer Mode to make one), and names that differ only in case (NTFS treats `Makefile`
and `makefile` as one file). pixi-oci never puts a layer's files on the host. It reads each package archive one entry
at a time and writes the entry, with its mode, symlink target and exact name, straight into the layer's tar. The only
files it writes on Windows are package archives, layer blobs and temporary files, all named by hashes.

Git Bash does not change any of this: it is a shell on the same Windows file system. It is fine to use, and so are
PowerShell and cmd.

## The same image as Linux

An image built on Windows is the same image, byte for byte, as one built from the same lockfile on Linux or macOS.
CI builds a test image on all three on every change and fails if the digests differ. That is what lets you build on
Windows and trust what CI tests on Linux.

## What needs a Linux machine

A Linux image's programs are Linux binaries, so running them needs a Linux kernel, which Windows only has inside WSL 2
or a VM. Without those:

| | On Windows |
|---|---|
| `pixi oci plan` | Yes |
| `pixi oci build`, for `linux-64` or `linux-aarch64` | Yes |
| `pixi oci validate` | Yes |
| `pixi oci inspect` | Yes |
| Run the image | No: it needs a Linux kernel. CI runs it for you. |

`pixi oci check`, a static check of an image that runs nothing and so works on Windows, is on the
[roadmap](roadmap.md), as is a rootless `pixi oci run` for Linux and CI.

## Paths

- **Output**: `-o C:\work\app.tar` or `-o .\app.tar`; either kind of slash works.
- **Cache**: `%LOCALAPPDATA%\pixi-oci` by default; `--cache-dir` or `PIXI_OCI_CACHE_DIR` move it.
- **Inside the image** everything is a Linux path: `--prefix /opt/env`, `--workdir /work`.

## Corporate networks

Downloads honour `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY`, and TLS is checked against the Windows certificate store,
so a proxy whose certificate your IT department installed there works without extra settings. Channels that need
credentials are not supported yet.

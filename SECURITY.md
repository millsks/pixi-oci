# Security policy

## Reporting a vulnerability

Report it privately through GitHub:
**[Report a vulnerability](https://github.com/millsks/pixi-oci/security/advisories/new)**
(Security → Advisories → Report a vulnerability). The report is visible only to the maintainer until an advisory is
published.

Please do not open a public issue for a security problem, and please do not disclose it elsewhere before a fix is
out. If GitHub's form is not available to you, open a public issue asking for a private channel, with no details in
it, and one will be arranged.

**What to include:** the version (`pixi oci --version`), the platform you built on, the command you ran, and the
smallest lockfile or package that reproduces it. For anything involving a crafted archive, attach the archive. A crash
needs a backtrace (`RUST_BACKTRACE=1`).

## What to expect

| | |
|---|---|
| Acknowledgment | Within 3 business days. This is a small project with one maintainer; if you have heard nothing after that, ping the advisory thread. |
| Assessment | Within 10 business days: whether it is in scope, how severe, and a rough fix timeline. |
| Fix and disclosure | A patch release, then a published GitHub Security Advisory with a CVE where one is warranted. |
| Credit | You are named in the advisory unless you would rather not be. Say which you prefer. |

There is no bug bounty.

## Supported versions

pixi-oci has not had a release yet. Until it does, fixes land on `main`; once releases start, the newest one is the
supported one.

## Scope

pixi-oci reads input an attacker may control, and writes images other people will run, so both ends matter.

It reads:

- **Lockfiles**: `pixi.lock`, YAML from a repository you may not have written.
- **Conda archives**: `.conda` (zip and zstd) and `.tar.bz2` (tar and bzip2), their `info/` files and their content,
  downloaded from the channels the lockfile names or read from local paths.
- **HTTP responses** from those channels. A response is untrusted even when the host is; every archive is checked
  against the sha256 the lockfile recorded before anything reads it.

**In scope**, from any of those:

- A file written on the build host outside the output, cache and temporary directories. A build is designed to
  write nothing on the host from an archive's member names: layers are written from the archive's entries straight
  into the layer's tar.
- A file placed in the image outside the environment's prefix by a package's paths, or an archive whose content
  differs from what the lockfile's sha256 covers and is used anyway.
- A panic, a hang, or an allocation that grows with an attacker's input past `--max-memory`'s design, such as a
  decompression bomb that exhausts memory or disk.
- An image that is not what the lockfile describes in a way an attacker can arrange on purpose: a package silently
  dropped, swapped, or given different bytes.
- A network request to a host the lockfile does not name, or a credential sent to a host that should not see it.

**Out of scope:**

- **Vulnerabilities in the packages you put in an image.** pixi-oci builds what the lockfile locks; scanning it is
  the job of a scanner (and of [pixi-sbom](https://github.com/aesir-oss/pixi-sbom)).
- Anything that needs an attacker who can already write to the build machine, the cache or the output directory.

# Private channels

A lockfile can name packages from channels that need credentials: an Artifactory or Nexus conda repository, a
prefix.dev private channel, an anaconda.org token channel, a private PyPI index for wheels. `pixi oci build` downloads
them with the credentials pixi itself uses to lock them, so there is nothing more to set up: if `pixi install` works,
so does the build.

## Logging in

Log in once, as for pixi:

```sh
pixi auth login artifactory.example.com --token <token>
pixi auth login artifactory.example.com --username <user> --password <password>
pixi auth login conda.anaconda.org --conda-token <token>
```

pixi-oci reads them where pixi keeps them, on Windows, macOS and Linux alike:

1. the file `RATTLER_AUTH_FILE` names, if set
2. the operating system's keychain (Windows Credential Manager, macOS Keychain, the Secret Service on Linux)
3. `~/.rattler/credentials.json`
4. `~/.netrc`

The lookup is pixi's: the download's host, then wildcards above it (`*.example.com`). Each kind of credential is sent
as rattler sends it: a token as `Authorization: Bearer`, a user and password as `Authorization: Basic`, a conda token
in the path (`https://host/t/<token>/channel/...`). A redirect to another host, such as a repository's storage, is
not given the credential.

In CI, point `RATTLER_AUTH_FILE` at a credentials file written from a secret, or write a `.netrc`; both are what pixi
reads there too.

## What stays out of the image

The credential is used for the download and nowhere else:

- layers record each package's URL as the lockfile names it, which has no credential in it
- the config, manifest and history name packages, never URLs with credentials
- errors name the lockfile's URL; anything a credential would be in is replaced by `<credential>`

The tests build from a channel that needs a token and check every blob of the image, unpacked, for it.

## Mirrors

pixi's `mirrors` setting is used as pixi uses it, so a lockfile that names `https://conda.anaconda.org/conda-forge`
downloads from your mirror instead:

```toml
# ~/.pixi/config.toml, or the workspace's .pixi/config.toml
[mirrors]
"https://conda.anaconda.org/conda-forge" = [
    "https://artifactory.example.com/api/conda/conda-forge",
    "oci://ghcr.io/channel-mirrors/conda-forge",
]
```

pixi-oci reads the files pixi reads, later ones winning: `/etc/pixi/config.toml` (`C:\ProgramData\pixi` on Windows),
the user config directory's `pixi/config.toml`, `$PIXI_HOME/config.toml` or `~/.pixi/config.toml`, rattler's shared
`rattler/config.toml` under each, and then the workspace's `.pixi/config.toml` beside the lockfile.

The longest channel URL a mirror stands in for is matched first, as rattler matches them. A package is downloaded from
the first of its mirrors that serves it: one that fails is passed over for the next, and when all fail the error names
the last one tried. Credentials are those pixi has for the mirror's host. Mirrors can be `https://`, `s3://`, `gcs://`
or `oci://`.

The lockfile's sha256 is checked whichever mirror served the file, and the image is the same: layers record the
lockfile's URLs, not the mirror's.

## S3 and GCS channels

`s3://bucket/channel` and `gcs://bucket/channel` locations, in a lockfile or as a mirror, are downloaded through
rattler's own S3 and GCS support, as pixi downloads them:

- **S3** requests are presigned with the keys `pixi auth login s3://<bucket> --s3-access-key-id <id>
  --s3-secret-access-key <key>` stored, or else the AWS SDK's usual chain (`AWS_*` variables, `~/.aws`, SSO, an
  instance role). A bucket somewhere other than AWS, such as MinIO, takes its endpoint from pixi's `s3-options`:

  ```toml
  [s3-options.my-bucket]
  endpoint-url = "https://minio.example.com"
  region = "us-east-1"
  addressing-style = "path"
  ```

- **GCS** requests use Google's application default credentials (`gcloud auth application-default login`, or
  `GOOGLE_APPLICATION_CREDENTIALS`).

Presigned URLs carry a key id and a signature, so pixi-oci never prints one: an error names the `s3://` location.

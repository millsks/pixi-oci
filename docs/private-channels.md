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

## Not yet

pixi's `mirrors` setting, and `s3://` and `gcs://` channels, are
[#51](https://github.com/millsks/pixi-oci/issues/51). A build whose lockfile names an `s3://` or `gcs://` package
stops before downloading anything else and says so; an https mirror of the same channel works today, locked as such.

# gbandit CLI

The command-line client for the [gbandit](https://gbandit.com) game-hosting platform. Used to deploy projects, tail logs, run SQL against tenant databases, and manage env vars.

## Install

```sh
curl -fsSL https://github.com/HectorBjernersjo/gbandit-cli/releases/latest/download/install.sh | sh
```

Drops the binary in `$HOME/.local/bin/gbandit`. Override with `GBANDIT_INSTALL_DIR` or pin a version with `GBANDIT_VERSION=v0.2.0`.

Prebuilt binaries are published for:

- Linux x86_64 / aarch64
- macOS x86_64 / aarch64

## Usage

```sh
gbandit deploy --message "<what you just changed>"
gbandit logs [frontend|backend]
gbandit sql "SELECT ..."
gbandit env [set|list|delete]
```

`gbandit deploy` defaults to the `dev` environment. Pass `--environment prod` for prod.

## Building from source

```sh
cargo build --release
```

The binary lands at `target/release/gbandit`.

## Pointing at a non-prod platform

Set both env vars before invoking the CLI:

```sh
export GBANDIT_AUTH_ORIGIN=http://auth.gbandit.localhost
export GBANDIT_PLATFORM_API_ORIGIN=http://platform.gbandit.localhost/api
```

When `GBANDIT_AUTH_ORIGIN` resolves to a localhost URL the CLI keeps a separate `credentials-dev.json` so dev tokens don't clobber prod ones.

## Releases

Tagging `vX.Y.Z` on `main` triggers `.github/workflows/release.yml`, which cross-compiles for all four targets and attaches the tarballs (and `install.sh`) to a GitHub release.

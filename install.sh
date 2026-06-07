#!/usr/bin/env sh
# gbandit CLI installer
#
# Usage:
#   curl -fsSL https://github.com/HectorBjernersjo/gbandit-cli/releases/latest/download/install.sh | sh
#
# Env vars:
#   GBANDIT_VERSION    Pin a specific tag (e.g. v0.2.0). Defaults to "latest".
#   GBANDIT_INSTALL_DIR Where to drop the binary. Defaults to $HOME/.local/bin.

set -eu

REPO="HectorBjernersjo/gbandit-cli"
VERSION="${GBANDIT_VERSION:-latest}"
INSTALL_DIR="${GBANDIT_INSTALL_DIR:-$HOME/.local/bin}"

uname_s="$(uname -s)"
uname_m="$(uname -m)"

case "$uname_s" in
    Linux)  os="unknown-linux-musl" ;;
    Darwin) os="apple-darwin" ;;
    *) echo "unsupported OS: $uname_s" >&2; exit 1 ;;
esac

case "$uname_m" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) echo "unsupported arch: $uname_m" >&2; exit 1 ;;
esac

target="${arch}-${os}"
asset="gbandit-${target}.tar.gz"

if [ "$VERSION" = "latest" ]; then
    url="https://github.com/${REPO}/releases/latest/download/${asset}"
else
    url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
fi

echo "Installing gbandit (${VERSION}) for ${target} to ${INSTALL_DIR}"

mkdir -p "$INSTALL_DIR"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl -fsSL "$url" -o "$tmp/gbandit.tar.gz"
tar -xzf "$tmp/gbandit.tar.gz" -C "$tmp"
mv "$tmp/gbandit" "$INSTALL_DIR/gbandit"
chmod +x "$INSTALL_DIR/gbandit"

echo "Installed: $INSTALL_DIR/gbandit"

echo ""
echo "Get started:"
echo ""
echo "  gbandit login                  # authenticate this machine"
echo "  gbandit new my-game            # create a project, scaffolded into ./my-game"
echo "  cd my-game && gbandit deploy   # live on your dev URL in one command"
echo ""
echo "Already have a project on gbandit?"
echo ""
echo "  Link a git remote to it under your project's settings on"
echo "  platform.gbandit.com — then any clone of that repo can deploy:"
echo ""
echo "  git clone <your-remote> && cd <your-repo>"
echo "  gbandit deploy"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo ""; echo "NOTE: $INSTALL_DIR is not in your PATH. Add it to your shell rc:"; echo "  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac

#!/bin/sh
# Install the latest cleanmcp release on Linux (x86_64 or arm64).
#   curl -fsSL https://raw.githubusercontent.com/Errantial/cleanmcp/main/install.sh | sh
# Options (environment): CLEANMCP_VERSION=v0.1.0, CLEANMCP_INSTALL_DIR=~/.local/bin
set -eu

REPO="Errantial/cleanmcp"
[ "$(uname -s)" = "Linux" ] || { echo "This script is for Linux. On Windows use install.ps1." >&2; exit 1; }
case "$(uname -m)" in
  x86_64 | amd64) ARCH="x86_64" ;;
  aarch64 | arm64) ARCH="aarch64" ;;
  *) echo "Unsupported CPU: $(uname -m)" >&2; exit 1 ;;
esac

TAG="${CLEANMCP_VERSION:-}"
if [ -z "$TAG" ]; then
  TAG=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
fi
[ -n "$TAG" ] || { echo "Could not find the latest release." >&2; exit 1; }
TAG="v${TAG#v}" # accept 0.1.0 or v0.1.0

NAME="cleanmcp-${TAG#v}-$ARCH-unknown-linux-musl"
BASE="https://github.com/$REPO/releases/download/$TAG"
DEST="${CLEANMCP_INSTALL_DIR:-$HOME/.local/bin}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $NAME..."
curl -fsSL "$BASE/$NAME.tar.gz" -o "$TMP/$NAME.tar.gz"
curl -fsSL "$BASE/SHA256SUMS" -o "$TMP/SHA256SUMS"
(cd "$TMP" && grep " $NAME.tar.gz\$" SHA256SUMS | sha256sum -c - >/dev/null) || { echo "Checksum mismatch!" >&2; exit 1; }

tar -xzf "$TMP/$NAME.tar.gz" -C "$TMP"
mkdir -p "$DEST"
install -m 755 "$TMP/$NAME/cleanmcp" "$DEST/cleanmcp"
echo "Installed $("$DEST/cleanmcp" --version) to $DEST/cleanmcp"
case ":$PATH:" in
  *":$DEST:"*) ;;
  *) echo "Note: $DEST is not on your PATH. Add it, e.g.: echo 'export PATH=\"$DEST:\$PATH\"' >> ~/.bashrc" ;;
esac

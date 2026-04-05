#!/usr/bin/env bash
set -euo pipefail

REPO="JacobLinCool/gemini-live-rs"
BIN_NAME="gemini-live-cli"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# Detect OS
case "$(uname -s)" in
  Linux)  os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *)      echo "Error: unsupported OS '$(uname -s)'" >&2; exit 1 ;;
esac

# Detect architecture
case "$(uname -m)" in
  x86_64|amd64)  arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *)             echo "Error: unsupported architecture '$(uname -m)'" >&2; exit 1 ;;
esac

TARGET="${arch}-${os}"
ASSET_NAME="${BIN_NAME}-${TARGET}.tar.gz"

echo "Platform: ${TARGET}"

# Find download URL from latest release
echo "Fetching latest release..."
DOWNLOAD_URL="$(
  curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep "browser_download_url.*${ASSET_NAME}" \
    | cut -d '"' -f 4
)"

if [ -z "${DOWNLOAD_URL:-}" ]; then
  echo "Error: no pre-built binary for ${TARGET}" >&2
  echo "You can build from source: cargo install --git https://github.com/${REPO} gemini-live-cli" >&2
  exit 1
fi

# Download and extract
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "Downloading ${ASSET_NAME}..."
curl -fsSL "$DOWNLOAD_URL" | tar xz -C "$TMPDIR"

# Install
mkdir -p "$INSTALL_DIR"
mv "$TMPDIR/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
chmod +x "$INSTALL_DIR/$BIN_NAME"

echo "Installed ${BIN_NAME} to ${INSTALL_DIR}/${BIN_NAME}"

# Check PATH
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
  echo ""
  echo "Note: ${INSTALL_DIR} is not in your PATH. Add it with:"
  echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
fi

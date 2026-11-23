#!/usr/bin/env sh
# Shell installer for riffty (https://github.com/riffty-sh/riffty)
# Usage: curl -LsSf https://github.com/riffty-sh/riffty/releases/latest/download/riffty-installer.sh | sh
set -eu

REPO="${RIFFTY_GITHUB_REPO:-riffty-sh/riffty}"
VERSION="${RIFFTY_VERSION:-latest}"
INSTALL_DIR="${RIFFTY_INSTALL_DIR:-$HOME/.local/bin}"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)  OS_TRIPLE="unknown-linux-gnu" ;;
  Darwin) OS_TRIPLE="apple-darwin" ;;
  *)
    echo "Unsupported OS: $OS" >&2
    exit 1
    ;;
esac

case "$ARCH" in
  x86_64|amd64)  ARCH_TRIPLE="x86_64" ;;
  arm64|aarch64) ARCH_TRIPLE="aarch64" ;;
  *)
    echo "Unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

TARGET="${ARCH_TRIPLE}-${OS_TRIPLE}"
ARCHIVE="riffty-${TARGET}.tar.gz"

if [ "$VERSION" = "latest" ]; then
  URL="https://github.com/${REPO}/releases/latest/download/${ARCHIVE}"
else
  URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARCHIVE}"
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "Downloading riffty (${TARGET}) from ${URL}..."
curl -fL "$URL" -o "${TMP_DIR}/${ARCHIVE}"
tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "$TMP_DIR"

mkdir -p "$INSTALL_DIR"
install -m 0755 "${TMP_DIR}/riffty" "${INSTALL_DIR}/riffty"
if [ -f "${TMP_DIR}/riffty_mcp" ]; then
  install -m 0755 "${TMP_DIR}/riffty_mcp" "${INSTALL_DIR}/riffty_mcp"
fi
if [ -f "${TMP_DIR}/riffty-ab" ]; then
  install -m 0755 "${TMP_DIR}/riffty-ab" "${INSTALL_DIR}/riffty-ab"
fi
if [ -f "${TMP_DIR}/riffty-java-runner.jar" ]; then
  install -m 0644 "${TMP_DIR}/riffty-java-runner.jar" "${INSTALL_DIR}/riffty-java-runner.jar"
fi
if [ -f "${TMP_DIR}/riffty-java.sha256" ]; then
  install -m 0644 "${TMP_DIR}/riffty-java.sha256" "${INSTALL_DIR}/riffty-java.sha256"
fi

echo "Installed riffty (and companion binaries) to ${INSTALL_DIR}"
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "Note: add ${INSTALL_DIR} to your PATH: export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac


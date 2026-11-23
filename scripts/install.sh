#!/usr/bin/env sh
# Verified shell installer for riffty (https://github.com/riffty-sh/riffty)
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
SHA_FILE="${ARCHIVE}.sha256"

if [ "$VERSION" = "latest" ]; then
  BASE_URL="https://github.com/${REPO}/releases/latest/download"
else
  BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "Downloading riffty (${TARGET}) from ${BASE_URL}/${ARCHIVE}..."
curl -fL "${BASE_URL}/${ARCHIVE}" -o "${TMP_DIR}/${ARCHIVE}"
curl -fL "${BASE_URL}/${SHA_FILE}" -o "${TMP_DIR}/${SHA_FILE}"

EXPECTED_SHA="$(awk '{print $1}' "${TMP_DIR}/${SHA_FILE}" | tr '[:upper:]' '[:lower:]')"
if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL_SHA="$(sha256sum "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}' | tr '[:upper:]' '[:lower:]')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL_SHA="$(shasum -a 256 "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}' | tr '[:upper:]' '[:lower:]')"
else
  echo "ERROR: neither sha256sum nor shasum found; refusing unverified install" >&2
  exit 1
fi

if [ "$EXPECTED_SHA" != "$ACTUAL_SHA" ]; then
  echo "ERROR: SHA-256 verification failed for ${ARCHIVE}!" >&2
  echo "  expected: ${EXPECTED_SHA}" >&2
  echo "  actual:   ${ACTUAL_SHA}" >&2
  exit 1
fi
echo "Verified SHA-256 (${ACTUAL_SHA})."

tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "$TMP_DIR"

mkdir -p "$INSTALL_DIR"
install -m 0755 "${TMP_DIR}/riffty" "${INSTALL_DIR}/riffty"
if [ -f "${TMP_DIR}/riffty_mcp" ]; then
  install -m 0755 "${TMP_DIR}/riffty_mcp" "${INSTALL_DIR}/riffty_mcp"
fi
if [ -f "${TMP_DIR}/riffty-ab" ]; then
  install -m 0755 "${TMP_DIR}/riffty-ab" "${INSTALL_DIR}/riffty-ab"
fi
if [ -d "${TMP_DIR}/riffty-java" ]; then
  rm -rf "${INSTALL_DIR}/riffty-java"
  cp -R "${TMP_DIR}/riffty-java" "${INSTALL_DIR}/riffty-java"
fi

echo "Installed verified riffty to ${INSTALL_DIR} (Python & JS/TS built-in; Java Tier 2 auto-hydrates on first .java check or via 'riffty plugin install java')."
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "Note: add ${INSTALL_DIR} to your PATH: export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac

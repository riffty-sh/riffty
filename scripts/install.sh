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

if [ -n "${RIFFTY_BASE_URL:-}" ]; then
  BASE_URL="${RIFFTY_BASE_URL%/}"
elif [ "$VERSION" = "latest" ]; then
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

add_to_profile() {
  _profile="$1"
  _export_line="export PATH=\"${INSTALL_DIR}:\$PATH\""
  if [ -f "$_profile" ] && grep -Fq "$INSTALL_DIR" "$_profile" 2>/dev/null; then
    return 0
  fi
  if printf '\n# Added by riffty installer\n%s\n' "$_export_line" >> "$_profile" 2>/dev/null; then
    echo "Added ${INSTALL_DIR} to PATH in ${_profile}."
    return 0
  fi
  return 1
}

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    if [ "${RIFFTY_NO_MODIFY_PATH:-0}" != "1" ] && [ -n "${HOME:-}" ] && [ -d "${HOME}" ]; then
      _updated=0
      _shell_name="$(basename "${SHELL:-}")"
      if [ "$_shell_name" = "zsh" ] || [ -f "${HOME}/.zshrc" ]; then
        if add_to_profile "${HOME}/.zshrc"; then _updated=1; fi
      fi
      if [ "$_shell_name" = "bash" ] || [ -f "${HOME}/.bashrc" ]; then
        if add_to_profile "${HOME}/.bashrc"; then _updated=1; fi
      fi
      if [ "$_updated" = "0" ] || [ -f "${HOME}/.profile" ]; then
        if add_to_profile "${HOME}/.profile"; then _updated=1; fi
      fi
      if [ "$_updated" = "1" ]; then
        echo "Restart your shell or run: export PATH=\"${INSTALL_DIR}:\$PATH\""
      else
        echo "Note: add ${INSTALL_DIR} to your PATH: export PATH=\"${INSTALL_DIR}:\$PATH\""
      fi
    else
      echo "Note: add ${INSTALL_DIR} to your PATH: export PATH=\"${INSTALL_DIR}:\$PATH\""
    fi
    ;;
esac

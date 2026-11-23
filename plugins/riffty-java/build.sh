#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

mkdir -p dist

if command -v mvn >/dev/null 2>&1 && [[ -f pom.xml ]]; then
  echo "Building and testing shaded riffty-java-runner.jar with Error Prone + Picnic via Maven..."
  mvn -q package
else
  echo "ERROR: mvn not found; cannot build shaded Error Prone + Picnic runner" >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256sum dist/riffty-java-runner.jar | awk '{print $1}' > dist/riffty-java.sha256
else
  shasum -a 256 dist/riffty-java-runner.jar | awk '{print $1}' > dist/riffty-java.sha256
fi
echo "Built dist/riffty-java-runner.jar ($(du -sh dist/riffty-java-runner.jar | awk '{print $1}'), sha256: $(cat dist/riffty-java.sha256))"

#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

if [[ -x "/opt/homebrew/opt/openjdk@25/bin/javac" ]]; then
  JDK25_BIN="/opt/homebrew/opt/openjdk@25/bin"
elif [[ -x "$HOME/.sdkman/candidates/java/current/bin/javac" ]]; then
  JDK25_BIN="$HOME/.sdkman/candidates/java/current/bin"
else
  JDK25_BIN="$(dirname "$(command -v javac)")"
fi

rm -rf build dist
mkdir -p build dist

"$JDK25_BIN/javac" --release 25 \
  -d build \
  src/sh/riffty/java/MiniJson.java \
  src/sh/riffty/java/Utf16ToUtf8Index.java \
  src/sh/riffty/java/JavaAnalyzerEngine.java \
  src/sh/riffty/java/RifftyJavaMain.java

"$JDK25_BIN/jar" --create \
  --file dist/riffty-java-runner.jar \
  --main-class sh.riffty.java.RifftyJavaMain \
  -C build .

shasum -a 256 dist/riffty-java-runner.jar | awk '{print $1}' > dist/riffty-java.sha256
echo "Built dist/riffty-java-runner.jar (sha256: $(cat dist/riffty-java.sha256))"


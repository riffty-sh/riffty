#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

if [[ -x "/opt/homebrew/opt/openjdk@25/bin/jlink" ]]; then
  JDK25_BIN="/opt/homebrew/opt/openjdk@25/bin"
elif [[ -x "$HOME/.sdkman/candidates/java/current/bin/jlink" ]]; then
  JDK25_BIN="$HOME/.sdkman/candidates/java/current/bin"
else
  echo "ERROR: JDK 25 jlink not found" >&2
  exit 1
fi

# 1. Build runner JAR + SHA-256
bash "$SCRIPT_DIR/build.sh"

# 2. Build minimal jlink JDK 25 LTS runtime (RFC §4.1)
rm -rf dist/runtime dist/riffty-java.aot dist/riffty-java.aot.sha256
"$JDK25_BIN/jlink" \
  --add-modules java.base,java.compiler,jdk.compiler,java.logging,java.xml,java.management,java.net.http,java.desktop,jdk.zipfs,jdk.unsupported \
  --strip-debug \
  --no-man-pages \
  --no-header-files \
  --compress=zip-6 \
  --output dist/runtime

echo "Built minimal jlink JDK 25 runtime ($(du -sh dist/runtime | awk '{print $1}'))"

# 3. Train Project Leyden AOT Cache (JEP 514/515) with exact production flags
TRAINING_REQ='{"protocol":1,"id":"aot-train","workspace_root":".","jdk_release":25,"classpath":[],"files":[{"file_id":1,"relative_path":"Train.java","source":"import java.math.BigDecimal;\nclass Train { void f(double x) { BigDecimal b = new BigDecimal(x); ObjectMapper m = new ObjectMapper(); m.createArrayNode(); } }\n"}],"allow_unsafe":false,"architecture_forbid":[]}'

printf '%s\n' "$TRAINING_REQ" | dist/runtime/bin/java \
  -XX:+UseCompactObjectHeaders \
  -XX:+UseSerialGC \
  -Xmx1g \
  -XX:+ExitOnOutOfMemoryError \
  -XX:AOTCacheOutput=dist/riffty-java.aot \
  --add-exports=jdk.compiler/com.sun.tools.javac.api=ALL-UNNAMED \
  --add-exports=jdk.compiler/com.sun.tools.javac.file=ALL-UNNAMED \
  --add-exports=jdk.compiler/com.sun.tools.javac.main=ALL-UNNAMED \
  --add-exports=jdk.compiler/com.sun.tools.javac.model=ALL-UNNAMED \
  --add-exports=jdk.compiler/com.sun.tools.javac.parser=ALL-UNNAMED \
  --add-exports=jdk.compiler/com.sun.tools.javac.processing=ALL-UNNAMED \
  --add-exports=jdk.compiler/com.sun.tools.javac.tree=ALL-UNNAMED \
  --add-exports=jdk.compiler/com.sun.tools.javac.util=ALL-UNNAMED \
  --add-opens=jdk.compiler/com.sun.tools.javac.code=ALL-UNNAMED \
  --add-opens=jdk.compiler/com.sun.tools.javac.comp=ALL-UNNAMED \
  --add-opens=jdk.compiler/com.sun.tools.javac.tree=ALL-UNNAMED \
  --add-opens=jdk.compiler/com.sun.tools.javac.util=ALL-UNNAMED \
  -jar dist/riffty-java-runner.jar >/dev/null

shasum -a 256 dist/riffty-java.aot | awk '{print $1}' > dist/riffty-java.aot.sha256
echo "Trained Leyden AOT cache dist/riffty-java.aot ($(du -sh dist/riffty-java.aot | awk '{print $1}'), sha256: $(cat dist/riffty-java.aot.sha256))"


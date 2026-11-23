#!/usr/bin/env bash
# Single-command local & CI verification gate for riffty (Build/CI Parity)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

echo "=== [1/5] Rust formatting gate ==="
cargo fmt --all -- --check

echo "=== [2/5] Java Tier 2 sidecar build & test ==="
bash plugins/riffty-java/build.sh

echo "=== [3/5] Strict Clippy gate (all targets) ==="
cargo clippy --workspace --all-targets -- -D warnings

echo "=== [4/5] Full workspace test suite (zero warnings) ==="
RUSTFLAGS="-D warnings" cargo test --workspace

echo "=== [5/5] Boule 95% affirmation traceability gate ==="
node scripts/affirmation-tracer.mjs audit

echo "ALL VERIFICATION GATES PASSED."

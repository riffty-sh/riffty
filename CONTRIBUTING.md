# Contributing to `riffty`

Thank you for contributing to `riffty`!

## Architectural Boundaries (Hexagonal Ports & Adapters)
- `crates/riffty_core` is the pure domain core and **must not** depend on `ruff_*`, `ty_*`, `ast_grep*`, or `salsa`.
- All semantic queries go through `TypeOraclePort`; all edits use UTF-8 byte-offset `TextRange` replacements (`R02`).

## Local Verification Gates

Before opening a pull request, ensure all checks pass with zero warnings:

```bash
cargo fmt --all -- --check
RUSTFLAGS="-D warnings" cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
node scripts/affirmation-tracer.mjs audit
```


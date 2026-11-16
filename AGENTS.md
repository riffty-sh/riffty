# AGENTS.md — `riffty` (v2) Architectural & Ubiquitous Language Charter

## 1. Mission
`riffty` is a deterministic, hexagonal (Ports & Adapters), type-aware Python bug detector, byte-offset surgical auto-fixer, and MCP diff reviewer written in Rust. It bridges the gap between syntactic linters (`ruff`), type checkers (`ty`), and structural rewrite tools (`ast-grep`) with zero false positives, exact UTF-8 byte-offset edits preserving all comments/formatting, and zero LLM token burn for deterministic fixes.

## 2. Non-Negotiable Hexagonal Boundaries
- **`crates/riffty_core`**: Pure domain core (`ByteOffset`, `TextRange`, `CanonicalNode`, `CanonicalType`, `TriState`, `Purity`, `Applicability`, `Fix`, `Diagnostic`, and `ports`). MUST NOT depend on `ruff_*`, `ty_*`, `ast_grep*`, or `salsa`.
- **`crates/riffty_pipeline`**: Single-pass ($O(\text{nodes})$) rule dispatcher, `ImportManagerPort` resolver, conflict resolver, bottom-to-top byte patcher, 10-pass fixpoint loop, and `ReviewFacade` (git diff scoping + `reverse_dependents` expansion).
- **`crates/riffty_rules_builtin`**: Native Rust `RulePlugin` implementations (`RFT-004`, `RFT-015`, `RFT-118`, `RFT-202` + `RFT-301`).
- **`crates/adapters/*`**: Driven adapters (`riffty_adapter_ruff_ast`, `riffty_adapter_ty`, `riffty_adapter_ast_grep`, `riffty_adapter_refaster`, `riffty_adapter_fs`).
- **`apps/*`**: Thin driving adapters (`riffty_cli`, `riffty_mcp`).
- **`tests/conformance`**: Adapter contract tests and golden regression fixtures (SciPy #20068).

## 3. Boule Verification Gates
1. `RUSTFLAGS="-D warnings" cargo test --workspace` — 100% unit, conformance, golden fixture, CLI, and MCP E2E suites green with zero warnings.
2. `cargo clippy --workspace --all-targets -- -D warnings` — zero compiler or lint warnings.
3. `node scripts/affirmation-tracer.mjs audit` — >= 95% Affirmation Verification Gate with 0 orphans.


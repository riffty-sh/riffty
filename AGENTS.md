# AGENTS.md — `riffty` Architectural & Ubiquitous Language Charter

## 1. Mission
`riffty` is a deterministic, hexagonal (Ports & Adapters) software remediation and engineering-efficiency engine written in Rust. Across **Python, Java, and JavaScript/TypeScript**, it combines syntactic + semantic/type analysis with surgical UTF-8 byte-offset auto-fixes (failing closed to `ManualOnly` when preconditions cannot be proven), empirical before/after A/B verification (`riffty-ab`), and pluggable infrastructure cost intelligence (`riffty_infra_core`).

## 2. Non-Negotiable Hexagonal Boundaries
- **`crates/riffty_core`**: Pure remediation domain core (`ByteOffset`, `TextRange`, `CanonicalNode`, `CanonicalType`, `TriState`, `Purity`, `Applicability`, `Fix`, `Diagnostic`, `LanguageAnalyzerPort`, and driven ports). MUST NOT depend on `ruff_*`, `ty_*`, `oxc_*`, `tsrs_*`, `ast_grep*`, or `salsa`.
- **`crates/riffty_infra_core`**: Pure infrastructure efficiency & cost intelligence core (`serde` + `serde_json` only). MUST NOT depend on HTTP clients, `kubectl`, or adapter crates.
- **`crates/riffty_ab_core`**: Pure A/B statistical engine, JUnit/TAP parsers, tamper guard, and SHA-256 attestation (`serde`, `serde_json`, `sha2`, `toml` only).
- **`crates/riffty_pipeline`**: Single-pass ($O(\text{nodes})$) rule dispatcher, `ImportManagerPort` resolver, conflict resolver, bottom-to-top byte patcher, 10-pass fixpoint loop, `AnalyzerRegistry`, and `MultiLanguageReviewFacade`.
- **`crates/riffty_rules_builtin`**: Native Rust `RulePlugin` implementations (`RFT-004`, `RFT-015`, `RFT-118`, `RFT-119`, `RFT-202`, `RFT-301`, `RFT-302`).
- **`crates/adapters/*`**: Thin driven adapters:
  - Python: `riffty_adapter_ruff_ast`, `riffty_adapter_ty`, `riffty_adapter_ast_grep`, `riffty_adapter_refaster`
  - Java: `riffty_adapter_java_fast` (Tier 1 `tree-sitter-java`), `riffty_adapter_plugin` (out-of-process JSON-RPC Tier 2 sidecar bridge)
  - JS/TS: `riffty_adapter_jsts_fast` (Tier 1 `oxc`), `riffty_adapter_tsrs` (Tier 2 `tsrs`)
  - Infra & FS: `riffty_adapter_infra` (native `ureq` HTTP for Prometheus/OpenCost, K8s HPA, local JFR, public pricing), `riffty_adapter_fs`
- **`plugins/riffty-java`**: Bundled JDK 25 / `javac` `Trees` / Error Prone + Picnic Tier 2 sidecar runner (built in CI & Releases; shaded JAR is gitignored).
- **`apps/*`**: Thin driving adapters (`riffty_cli`, `riffty_ab`, `riffty_mcp`).
- **`tests/conformance`**: Hexagonal boundary tests, adapter contract tests, and golden regression fixtures.

## 3. Verification Gates
1. `cargo fmt --all -- --check` — strict Rust formatting.
2. `RUSTFLAGS="-D warnings" cargo test --workspace` — 100% unit, conformance, golden fixture, CLI, AB, and MCP suites green with zero warnings.
3. `cargo clippy --workspace --all-targets -- -D warnings` — zero compiler or lint warnings.
4. `bash plugins/riffty-java/build.sh` — Java Tier 2 sidecar builds & passes tests with SHA-256 manifest.
5. `node scripts/affirmation-tracer.mjs audit` — >= 95% Affirmation Verification Gate with 0 orphans.

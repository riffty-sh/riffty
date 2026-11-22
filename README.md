<div align="center">
  <img src="assets/logo.svg" alt="riffty logo" width="96" height="96" />
  <h1><code>riffty</code></h1>
  <p><strong>Code better. Prove the impact.</strong></p>
  <p>Deterministic software remediation and measurable engineering efficiency — written in Rust.</p>

  <p>
    <a href="https://github.com/riffty-sh/riffty/actions/workflows/ci.yml"><img src="https://github.com/riffty-sh/riffty/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
    <a href="https://github.com/riffty-sh/riffty/releases"><img src="https://img.shields.io/github/v/release/riffty-sh/riffty" alt="Release" /></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License: MIT" /></a>
    <img src="https://img.shields.io/badge/rust-1.85%2B-orange.svg" alt="Rust 1.85+" />
    <img src="https://img.shields.io/badge/architecture-hexagonal-7c3aed.svg" alt="Hexagonal Architecture" />
  </p>
</div>

---

> **We don't just suggest better code. We prove what changed, why it is safe, and what impact it has.**
>
> Riffty identifies software inefficiencies across **Python, Java, and JS/TS**, applies deterministic code transformations only when semantic safety can be established (failing closed to `ManualOnly` otherwise), and measures before-and-after impact with `riffty-ab`, runtime telemetry, and pluggable infrastructure cost intelligence.

## Why Riffty

Existing tools cover parts of the workflow — static analyzers detect issues, AI assistants suggest edits, and codemod engines apply fixes. Riffty closes the gap by combining three capabilities in one verifiable loop:

1. **Safe by design** — Deterministic byte-offset remediations guarded by semantic/type analysis (`ty` for Python, bundled JDK 25 `javac` `Trees` + Error Prone/Picnic for Java, `oxc`/`tsrs` for JS/TS) with strict **fail-closed** (`ManualOnly`) refusal when preconditions cannot be proven.
2. **Measurable by default** — Empirical before-and-after comparative benchmarking via `riffty-ab` (alternating `AB/BA` pairs, exact Student-t 95% CI, deterministic SHA-256 evidence attestation) integrated with runtime and Kubernetes telemetry.
3. **Economically aware** — Pure hexagonal infrastructure intelligence (`riffty_infra_core`) connecting workload efficiency to resource allocation and estimated cost impact (Prometheus, Kubernetes HPA, OpenCost, public pricing) without conflating modeled estimates (`CostKind::Estimated`) with realized cloud billing (`CostKind::ActualBilled`).

---

## Installation

### macOS & Linux (Shell Installer)

```bash
curl -LsSf https://github.com/riffty-sh/riffty/releases/latest/download/riffty-installer.sh | sh
```

### Homebrew (macOS & Linux)

```bash
brew install riffty-sh/tap/riffty
```

### Windows (PowerShell)

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/riffty-sh/riffty/releases/latest/download/riffty-installer.ps1 | iex"
```

### From Source (Cargo)

```bash
cargo install --git https://github.com/riffty-sh/riffty --tag v0.2.0 riffty_cli
```

---

## Key Capabilities

- **Multi-Language Deterministic Remediation (`riffty check`):**
  - **Python (`riffty_rules_builtin`, `riffty_adapter_ruff_ast`, `riffty_adapter_ty`, `riffty_adapter_refaster`):** Type-aware checkers (`RFT-004`, `RFT-015`, `RFT-118`/`119`, `RFT-202`/`301`, `RFT-302`) and declarative YAML Refaster rules guarded by Astral `ty` Salsa semantic queries.
  - **Java (`riffty_adapter_java_fast`, `plugins/riffty-java`):** Zero-JVM Tier 1 (`tree-sitter-java`) + Tier 2 semantic engine (bundled `jlink` JDK 25, Leyden AOT, `javac` `Trees`, Class-File API, shaded Error Prone & Picnic rules, `[[java.architecture.forbid]]`).
  - **JavaScript / TypeScript (`riffty_adapter_jsts_fast`, `riffty_adapter_tsrs`):** Pure-Rust Tier 1 (`oxc_parser`/`oxc_semantic`) + Tier 2 (`tsrs`) with zero Node.js runtime dependency.
- **Empirical A/B Verification (`riffty-ab`):**
  - `riffty-ab compare-metrics`: multi-metric baseline vs. fixed comparison with alternating execution order, median/mean/p95/p99/max, Student-t 95% CI, paired win counts, and cryptographic SHA-256 attestation.
- **Infrastructure Efficiency & Cost Intelligence (`riffty infra`, `riffty efficiency`):**
  - `riffty infra providers`: inspect registered provider plugins (`local-jfr-metrics`, `kubernetes-hpa`, `public-pricing`, `prometheus-metrics`, `opencost-billing`) and runtime health.
  - `riffty efficiency analyze`: operates in **`CoreOnly`** mode (zero external infra) or **`Connected`** mode (Prometheus cumulative CPU/memory + K8s HPA + OpenCost allocation with explicit `__idle__` & shared overhead separation).
- **MCP Diff Reviewer (`riffty mcp`):**
  - Exposes `riffty_review_changes` over stdio JSON-RPC (`rmcp`) with `git diff` scoping and reverse-dependent blast-radius analysis.

## Quick Start

```bash
# 1. Inspect environment & Tier 2 semantic readiness
riffty doctor

# 2. Zero-write dry-run audit (prints unified diffs, modifies 0 files)
riffty check --fix --dry-run --summary path/to/repo

# 3. Apply compiler-verified safe fixes
riffty check --require-tier2 --fix path/to/repo

# 4. Compare baseline vs. fixed metrics with SHA-256 attestation
riffty-ab compare-metrics --input metrics_input.json --out-json ab_report.json --out-md ab_report.md

# 5. Analyze workload efficiency & estimated infrastructure impact (CoreOnly or Connected)
riffty infra providers
riffty efficiency analyze --baseline metrics.json --pricing aws-us-east-1 --format markdown

# 6. Launch the stdio MCP server for AI coding agents
riffty mcp
```

## Workspace Layout

```text
riffty/
├── Cargo.toml                        # Workspace manifest & shared SemVer metadata
├── crates/
│   ├── riffty_core/                  # Pure Remediation Domain Core (zero parser/type-checker deps)
│   ├── riffty_infra_core/            # Pure Infra Efficiency & Cost Core (serde + serde_json only)
│   ├── riffty_ab_core/               # Pure A/B statistical engine & SHA-256 attestation
│   ├── riffty_pipeline/              # Single-pass runner, conflict resolver, 10-pass fixpoint
│   ├── riffty_rules_builtin/         # Native Rust BugCheckers
│   └── adapters/
│       ├── riffty_adapter_ruff_ast/  # Python SyntaxParserPort & ImportManagerPort
│       ├── riffty_adapter_ty/        # Python TypeOraclePort via Astral `ty`
│       ├── riffty_adapter_java_fast/ # Zero-JVM Java Tier 1 + Tier 2 plugin bridge
│       ├── riffty_adapter_jsts_fast/ # Pure-Rust JS/TS Tier 1 (`oxc`)
│       ├── riffty_adapter_tsrs/      # Pure-Rust JS/TS Tier 2 (`tsrs`)
│       ├── riffty_adapter_ast_grep/  # PatternMatcherPort via `ast-grep-core`
│       ├── riffty_adapter_refaster/  # Declarative YAML Refaster bridge
│       ├── riffty_adapter_plugin/    # Out-of-process JSON-RPC language plugin bridge
│       ├── riffty_adapter_infra/     # Local, K8s HPA, Prometheus, OpenCost, Public Pricing adapters
│       └── riffty_adapter_fs/        # WorkspaceVfsPort, PatchApplierPort, GitDiffPort
├── plugins/
│   └── riffty-java/                  # Bundled JDK 25 / Leyden AOT / javac Trees / Error Prone / Picnic runner
├── apps/
│   ├── riffty_cli/                   # Driving CLI (`check`, `doctor`, `explain`, `infra`, `efficiency`, `mcp`)
│   ├── riffty_ab/                    # Empirical A/B runner & `compare-metrics` CLI
│   └── riffty_mcp/                   # Driving MCP stdio server (`riffty_review_changes`)
└── tests/
    └── conformance/                  # Adapter contract tests & golden regression fixtures
```

## Verification Gates

```bash
cargo fmt --all -- --check
RUSTFLAGS="-D warnings" cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Independently Audited Evidence (`riffty-assets`)

All external benchmarks, rule packs, and independently reviewed reports live in [`riffty-sh/riffty-assets`](https://github.com/riffty-sh/riffty-assets):
- **Audited Engineering & Cost Intelligence Package (`9.7 / 10.0 — APPROVED`):** 5-rep alternating `riffty-ab` + Prometheus cumulative CPU (`-45.32%`) + live OpenCost aligned `16.5m` namespace allocation (`-31.79%`, Level 2 `Estimated`, separated from `__idle__`) on Quarkus 3.30.1 (`reports/k8s_hpa_finops/final/`).
- **Multi-Language Open-Source Suites:** 21 Python/Java AI repo cases (`milestones_tests/`), 14 pure-Rust JS/TS cases (`jsts_milestones_tests/`), 12 Apache Airflow cases (`apache_airflow/`), and cross-platform Apple Silicon + Linux `x86_64` JMH/JFR profiling (`finops_java_profiling/`).

See [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md), and `docs/` for architecture and governance details.

<div align="center">
  <img src="assets/logo.svg" alt="riffty logo" width="96" height="96" />
  <h1><code>riffty</code></h1>
  <p><strong>Deterministic, type-aware Python bug detector, surgical byte-offset auto-fixer, and MCP diff reviewer written in Rust.</strong></p>

  <p>
    <a href="https://github.com/riffty-sh/riffty/actions/workflows/ci.yml"><img src="https://github.com/riffty-sh/riffty/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
    <a href="https://github.com/riffty-sh/riffty/releases"><img src="https://img.shields.io/github/v/release/riffty-sh/riffty" alt="Release" /></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License: MIT" /></a>
    <img src="https://img.shields.io/badge/rust-1.85%2B-orange.svg" alt="Rust 1.85+" />
    <img src="https://img.shields.io/badge/architecture-hexagonal-7c3aed.svg" alt="Hexagonal Architecture" />
  </p>
</div>

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

- **Type-Aware Python Bug Checkers (`crates/riffty_rules_builtin`)**:
  - `RFT-004`: Unawaited coroutine expressions (`Callable { is_async: true }` called as a bare statement without `await`, `return`, or assignment).
  - `RFT-015`: Disjoint collection membership (`x in set[Y]` / `x not in container` where operand and element types are provably disjoint).
  - `RFT-118` / `RFT-119`: Surgical `stacklevel=2` injection into `warnings.warn(...)` (`Applicability::Safe`) and narrow `skip_file_prefixes` detection.
  - `RFT-202` + `RFT-301`: Conditional SciPy #20068 migration from `np.testing.assert_warns(W, fn, *args, match=...)` to `with pytest.warns(W, match=...): fn(*args)` — `Safe` only when `fn` accepts neither `match` nor `**kwargs`.
  - `RFT-302`: Compositional SQLAlchemy 1.4 `Session.query(...)` to 2.0 `select(...)` migration guarded by `sqlalchemy.orm.Session` MRO.
- **Refaster Declarative YAML Rules (`crates/adapters/riffty_adapter_refaster`)**:
  - Structural `ast-grep` patterns (`$VAR`, `$$$ARGS`) guarded by Astral `ty` Salsa semantic queries (`fqname_in`, `type_assignable_to`, `inside_subclass_of`, `inside_async_def`, `purity`, `callable_accepts_kwarg`, `not_overridden_below`, `not_inside_with`) and `R05+` `bind_impure_once` walrus lowering.
- **Two Driving Interfaces**:
  - **CLI**: `riffty check [--fix] [--unsafe-fixes] [--diff] [--rules <yaml>] [--python-venv <venv>] [--format text|json]`
  - **MCP Server**: `riffty mcp` exposing `riffty_review_changes` over stdio JSON-RPC (`rmcp`) with `git diff` scoping and `TypeOraclePort::reverse_dependents` blast-radius expansion.

## Quick Start

```bash
# Check a Python project (auto-discovers riffty.toml or [tool.riffty] in pyproject.toml)
riffty check path/to/repo --fix

# Launch the stdio MCP server for AI coding agents
riffty mcp
```

### Project Configuration (`riffty.toml` or `pyproject.toml`)

```toml
# riffty.toml (or [tool.riffty] in pyproject.toml)
rules = "rules/default.yml"
python_venv = ".venv"
exclude = ["tests/fixtures", "legacy_generated.py"]
```

## Workspace Layout

```text
riffty/
├── Cargo.toml                        # Workspace manifest & shared SemVer metadata
├── rust-toolchain.toml               # Pinned stable toolchain + clippy/rustfmt
├── rustfmt.toml                      # Formatting rules
├── crates/
│   ├── riffty_core/                  # Pure Domain Core (zero ruff/ty/ast-grep/salsa deps)
│   ├── riffty_pipeline/              # Single-pass runner, conflict resolver, 10-pass fixpoint, ReviewFacade
│   ├── riffty_rules_builtin/         # Native Rust BugCheckers (RFT-004, RFT-015, RFT-118, RFT-119, RFT-202/301, RFT-302)
│   └── adapters/
│       ├── riffty_adapter_ruff_ast/  # SyntaxParserPort + ImportManagerPort (with symbol merging & R19)
│       ├── riffty_adapter_ty/        # TypeOraclePort via Astral's `ty` Salsa DB + R24 inference
│       ├── riffty_adapter_ast_grep/  # PatternMatcherPort via `ast-grep-core`
│       ├── riffty_adapter_refaster/  # YAML Refaster bridge (ast-grep -> TypeOraclePort guards)
│       └── riffty_adapter_fs/        # WorkspaceVfsPort + PatchApplierPort + GitDiffPort
├── apps/
│   ├── riffty_cli/                   # Driving CLI (`riffty check` / `riffty mcp`)
│   └── riffty_mcp/                   # Driving MCP stdio server (`riffty_review_changes`)
└── tests/
    └── conformance/                  # Adapter contract tests & golden regression fixtures
```

## Verification Gates

```bash
cargo fmt --all -- --check
RUSTFLAGS="-D warnings" cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
node scripts/affirmation-tracer.mjs audit
```

## Empirical Benchmarks

- **Apache Airflow 12-Issue Suite**: 12/12 real `apache/airflow` bugs (3 live `main` `HEAD` bugs + 9 historical PRs) reproduced with failing `pytest`, auto-healed via `riffty check --fix`, and verified passing (`12/12`).
- **Live `OPEN` GitHub Issues Suite**: 6 live `HEAD` clones & 11 verified `OPEN` GitHub issues across FastAPI, LangChain, Airflow, Superset, bump-pydantic, and SciPy.
- **Historical Pre-Fix Suite**: 6 real cloned GitHub repositories validated end-to-end with zero false positives.

See [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md), and `docs/` for architecture and governance details.


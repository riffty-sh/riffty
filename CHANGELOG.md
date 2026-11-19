# Changelog

All notable changes to `riffty` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-11-16

### Added
- **Driving CLI (`riffty_cli`)**: `riffty check [--fix] [--unsafe-fixes] [--diff] [--rules] [--python-venv] [--format text|json]` and `riffty mcp` subcommands, plus automatic `riffty.toml` / `pyproject.toml` `[tool.riffty]` discovery.
- **Driving MCP Server (`riffty_mcp`)**: Stdio JSON-RPC server (`rmcp`) exposing `riffty_review_changes` with `git diff` baseline suppression and `TypeOraclePort::reverse_dependents` blast-radius expansion.
- **`RFT-302` (`sqlalchemy-legacy-session-query`)**: Compositional SQLAlchemy 1.4 `Session.query(...)` to 2.0 `select(...)` migration guarded by `sqlalchemy.orm.Session` MRO.
- **`R24` Intra-Class Return-Type Inference**: Resolves `self.<attr>(...)` factory return types across class attributes and MROs when method annotations are missing or `Any`.
- **`R05+` `bind_impure_once` Walrus Lowering**: Lowers duplicated impure Refaster metavariables into single-evaluation `(_riffty_<var> := <expr>)` bindings.
- **`ImportManagerPort` Symbol Merging**: Merges new `ImportSpec` symbols into existing `from <module> import ...` statements without duplicate import lines.
- **Release & Distribution**: Multi-platform binary archives, POSIX/PowerShell installers (`riffty-installer.sh`, `riffty-installer.ps1`), and Homebrew tap (`brew install riffty-sh/tap/riffty`).

## [0.1.0] - 2026-11-15

### Added
- **Hexagonal Domain Core (`riffty_core`)**: Pure canonical AST/IR, `CanonicalType` semantic lattice, `TriState`/`Purity`/`Applicability` algebra, UTF-8 `ByteOffset`/`TextRange` surgical edits, and Driven Port contracts.
- **Single-Pass Pipeline (`riffty_pipeline`)**: $O(\text{nodes})$ rule dispatcher, deterministic overlap conflict resolver, bottom-to-top byte patcher, and bounded 10-pass fixpoint loop.
- **Driven Adapters**: `riffty_adapter_fs`, `riffty_adapter_ruff_ast`, `riffty_adapter_ty` (Astral `ty` Salsa DB), `riffty_adapter_ast_grep`, and `riffty_adapter_refaster`.
- **Built-in Bug Checkers (`riffty_rules_builtin`)**: `RFT-004`, `RFT-015`, `RFT-118`, `RFT-119`, `RFT-202`, and `RFT-301` with SciPy #20068 golden conformance tests.


# Changelog

All notable changes to `riffty` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.2](https://github.com/riffty-sh/riffty/compare/v0.4.1...v0.4.2) (2026-11-23)


### Fixed

* **windows:** pass Expand-Archive zip paths via env vars in hydrate.rs ([58c0503](https://github.com/riffty-sh/riffty/commit/58c0503d558f24e49cfb6193109fc27aed9f422f))
* **windows:** use .NET ZipFile::ExtractToDirectory to avoid PS7/PS5.1 Expand-Archive module clash ([7d620b8](https://github.com/riffty-sh/riffty/commit/7d620b8017861e5cc65c09bd6c796c248a1317e8))

## [0.4.1](https://github.com/riffty-sh/riffty/compare/v0.4.0...v0.4.1) (2026-11-23)


### Added

* add Refaster pack sugar/extends/validation and cross-language architecture engine ([51aff77](https://github.com/riffty-sh/riffty/commit/51aff771b2e994215cb07d7b5d7e2b1bc1bf8724))

## [Unreleased]

## [0.4.0] - 2026-11-23

### Added
- **On-Demand Verified Plugin Hydration (`riffty_adapter_plugin::hydrate`, `riffty plugin`)**:
  - Core install (`riffty`, `riffty_mcp`, `riffty-ab`) is now a lean pure-Rust bundle (~15–20 MB) with Python (Tier 1+2), JS/TS (Tier 1+2), and Java (Tier 1) built in.
  - On first `.java` scan (unless `--offline` / `RIFFTY_OFFLINE=1`), `riffty` automatically downloads `riffty-java-<target>.tar.gz` (self-contained `jlink` JDK 25 + Leyden AOT + Error Prone/Picnic), verifies its SHA-256 checksum before extraction, and atomically caches it in `~/.cache/riffty/plugins/riffty-java/v<version>/`.
  - Added `riffty plugin install java`, `riffty plugin status`, `riffty plugin clean`, and `riffty check --offline` (`RIFFTY_OFFLINE`) for air-gapped CI workflows.
- **Supply-Chain Hardening & Clean-Room Release Verification**:
  - `scripts/install.sh` and `scripts/install.ps1` now verify `.sha256` checksums before extracting.
  - `release.yml` builds Linux `x86_64` on `ubuntu-22.04` (glibc 2.35 compatibility), adds `aarch64-unknown-linux-gnu` (ARM64 Linux), gates publishing behind pre-release `verify-bundle-linux` (bare `debian:bookworm-slim`) and `verify-bundle-macos` (`env -i`) clean-room jobs, emits a CycloneDX SBOM + SLSA Build Provenance (`actions/attest-build-provenance@v2`), and publishes the pre-hydrated Docker image to `ghcr.io/riffty-sh/riffty`.
  - Added `scripts/verify-all.sh` (Build/CI parity) and `docker-smoke` CI job.

## [0.3.1] - 2026-11-23

### Changed
- **100% Zero-External-Setup Bundles (`release.yml`, `Dockerfile`, `install.sh`, `install.ps1`, `homebrew-tap`)**: Every platform release archive, Docker image, and Homebrew install now packages the self-contained `jlink` JDK 25 runtime + Project Leyden AOT cache (`riffty-java/`) alongside `riffty`, `riffty_mcp`, and `riffty-ab`, eliminating any local JDK, Python, or Node.js prerequisite on user machines.
- **Cross-Platform Bundled Runtime Discovery (`riffty_adapter_plugin`)**: Added `riffty-java/` and `libexec/riffty-java/` discovery paths and Windows `runtime/bin/java.exe` support.

## [0.3.0] - 2026-11-23

### Added
- **Multi-Language Remediation Engine**:
  - **Java (`riffty_adapter_java_fast`, `riffty_adapter_plugin`, `plugins/riffty-java`)**: Zero-JVM Tier 1 (`tree-sitter-java`) + Tier 2 semantic sidecar (JDK 25 `javac` `Trees`, Error Prone, Picnic, SHA-256 verification, warm daemon mode, and declarative `[[java.architecture.forbid]]`).
  - **JavaScript / TypeScript (`riffty_adapter_jsts_fast`, `riffty_adapter_tsrs`)**: Pure-Rust Tier 1 (`oxc`) + Tier 2 (`tsrs`) with zero Node.js runtime dependency and `[[jsts.architecture.forbid]]`.
- **Empirical A/B Verification (`riffty_ab_core`, `apps/riffty_ab`)**: `riffty-ab` CLI with alternating `AB/BA` execution, exact Student-t 95% CI, p95/p99/max statistics, JUnit/TAP parsers, tamper guard, and SHA-256 attestation.
- **Infrastructure Efficiency & Cost Intelligence (`riffty_infra_core`, `riffty_adapter_infra`)**: Pure hexagonal core (`serde` + `serde_json` only) with `riffty infra providers` and `riffty efficiency analyze` (`CoreOnly` and `Connected` modes), native Rust `ureq` HTTP adapters for Prometheus & OpenCost (with optional bearer-token auth), Kubernetes HPA, and public pricing (`CostKind::Estimated`).
- **Hexagonal Architecture Conformance Suite (`tests/conformance/tests/architecture_contracts.rs`)**: Automated purity and contract checks covering `riffty_infra_core`, `riffty_ab_core`, native HTTP adapters, Java/JS-TS `LanguageAnalyzerPort` implementations, and fail-closed SHA-256 sidecar verification.

### Changed
- **Packaging & Distribution**: `release.yml`, `Dockerfile`, `scripts/install.sh`, `scripts/install.ps1`, and Homebrew tap now package and install `riffty-ab` and the Java Tier 2 sidecar bundle (`riffty-java-runner.jar` + `riffty-java.sha256`) alongside `riffty` and `riffty_mcp`.
- **CI Hardening**: Added `cargo fmt --all -- --check` gate and Maven test execution in `plugins/riffty-java/build.sh`; untracked and gitignored the 21 MB shaded `riffty-java-runner.jar` from Git history.

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

[Unreleased]: https://github.com/riffty-sh/riffty/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/riffty-sh/riffty/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/riffty-sh/riffty/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/riffty-sh/riffty/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/riffty-sh/riffty/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/riffty-sh/riffty/releases/tag/v0.1.0

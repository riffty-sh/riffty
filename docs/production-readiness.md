# `riffty` (v2) Production Readiness Architecture (7 Pillars)

`riffty` operates as a hermetic local developer CLI (`riffty check`) and stdio Model Context Protocol server (`riffty mcp`). Below is how each of the 7 Production Readiness Pillars applies to its Rust binary architecture:

| Pillar | Architectural Control | Verification Artifact |
| :--- | :--- | :--- |
| **1. Security & Workspace Containment** | `FsWorkspaceAdapter` canonicalizes paths and refuses disk writes outside the configured workspace root; zero network listeners or shell-injection interpolation (`std::process::Command` arg arrays only). | `crates/adapters/riffty_adapter_fs` |
| **2. State & Cache Lifecycle** | Deterministic `FileId` ordering and thread-safe Salsa / semantic index snapshots per analysis pass; atomic file writes during `--fix` / `auto_heal`. | `crates/adapters/riffty_adapter_ty` |
| **3. API & Protocol Validation** | Typed `serde` / `rmcp` request validation for `riffty_review_changes` (`target: unstaged \| staged \| branch_diff`, `base_ref`, `auto_heal`) with standard JSON-RPC error payloads. | `apps/riffty_mcp` |
| **4. Observability & Telemetry** | Compact structured `summary` telemetry (`files_reviewed`, `auto_healed_count`, `blocking_issues`, `iterations`) while keeping `stdout` strictly clean for JSON-RPC frames. | `crates/riffty_pipeline` |
| **5. Resource Hardening & Safety Caps** | Hard 10-iteration fixpoint ceiling, repeat-source cycle breaker, UTF-8 char-boundary validation, and `Unknown`/`Any` diagnostic suppression (`R03`). | `crates/riffty_pipeline` |
| **6. 12-Factor Configuration** | Configurable via CLI flags and `RIFFTY_*` environment variables documented in `.env.example` with zero hardcoded absolute paths. | `.env.example`, `apps/riffty_cli` |
| **7. Packaging, Containerization & CI** | Multi-stage non-root Rust `Dockerfile`, `docker-compose.yml`, and GitHub Actions workflow running `RUSTFLAGS="-D warnings" cargo test --workspace` + `clippy` + Boule affirmation audit. | `Dockerfile`, `docker-compose.yml`, `.github/workflows/ci.yml` |


# `feat(v2.2)`: Eliminate LLM-in-the-loop YAML tweaking via 4 deterministic Rust engine upgrades (TDD Plan)

## Context & Problem Statement

During the 12-case Apache Airflow benchmark (`airflow_issues_benchmark/`, committed in `686c1d5`), all 12 real-world Airflow bugs were reproduced with failing `pytest` suites and healed to 100% passing tests without modifying `riffty`'s Rust source.

However, **4 of the 12 cases required iterating on custom YAML rules** after observing `riffty` or `pytest` outcomes. Relying on an LLM at runtime to inspect test failures and tweak YAML rules violates `riffty`'s core charter:

> *"Zero false positives, exact UTF-8 byte-offset edits preserving all comments/formatting, and zero LLM token burn for deterministic fixes."*

Every instance where a YAML rule had to be tweaked exposed a concrete capability gap in `riffty`'s deterministic Rust engine. This issue tracks the **4 deterministic Rust engine upgrades** (developed strictly test-first under Boule TDD gates) to eliminate those gaps.

---

## Architectural Review & 4 Deterministic Upgrades

### 1. Automatic Dead-Import Pruning on Last-Reference Rewrite (`R23`)
- **Exposed by:** Airflow Case 10 (`PR #65442` — SFTP `airflow.utils.timezone` -> `airflow.sdk.timezone` in [case_10_pr_65442_sftp_timezone.yml](../airflow_issues_benchmark/rules/case_10_pr_65442_sftp_timezone.yml)).
- **Root Cause:** `ImportManagerPort` in `crates/riffty_pipeline/src/lib.rs` and `crates/adapters/riffty_adapter_ruff_ast/src/lib.rs` injects `required_imports` (`R19`) and rewrites standalone deprecated module imports when targeted, but when a call-site rule rewrites the **last reference** to an imported symbol (`convert_to_utc(...)` -> `timezone.convert_to_utc(...)`), the original `from airflow.utils.timezone import convert_to_utc` statement remains unused at the top of the module, triggering `DeprecatedImportWarning` at import time.
- **Hexagonal Fix:**
  - Extend `ImportManagerPort` (`crates/riffty_core/src/ports.rs`) and `RuffAstAdapter` (`crates/adapters/riffty_adapter_ruff_ast/src/lib.rs`) with deterministic post-fix dead-import pruning for symbols referenced inside patched byte ranges.
  - If all references to a symbol bound by a `from X import a, b` statement are eliminated by applied `Fix` edits:
    - Remove only the unused specifier `a` (if multi-import `from X import a, b` -> `from X import b`).
    - Remove the entire import line (preserving surrounding comments/blank lines) if `a` was the sole imported specifier.
- **Mandatory Tests (TDD First):**
  - Unit test in `riffty_adapter_ruff_ast`: single-specifier removal vs. multi-specifier partial removal when last reference is rewritten.
  - Negative safety test: symbol still referenced elsewhere in the file, in type comments, or re-exported in `__all__` MUST NOT be pruned.
  - End-to-end conformance test on Case 10 (`triggers/sftp.py`) proving a single call-site rule heals both the call and the dead import without needing a separate import-removal rule.

---

### 2. Native Compositional SQLAlchemy 2.0 `Session.query` RulePlugin (`RFT-302`)
- **Exposed by:** Airflow Case 12 (`PR #32645` / `#75118` — SQLAlchemy 2.0 `session.query(...)` removal in [case_12_pr_32645_sqlalchemy20_session_query.yml](../airflow_issues_benchmark/rules/case_12_pr_32645_sqlalchemy20_session_query.yml)).
- **Root Cause:** Declarative YAML `ast-grep` templates match static tree shapes. Because SQLAlchemy 1.x queries chain arbitrarily (`.query(M)`, `.query(M).filter(F)`, `.query(M).filter(F).scalar()`, `.query(M).filter(F).first()`, `.query(c1, c2).filter(F)`), YAML required 7 separate shape-specific rules.
- **Hexagonal Fix:**
  - Implement native Rust `RulePlugin` `RFT-302` (`sqlalchemy-legacy-session-query`) in `crates/riffty_rules_builtin/src/lib.rs`.
  - Walk the call-chain AST compositionally from the terminal call (`.all()`, `.first()`, `.scalar()`, `.one()`, `.one_or_none()`, or bare `.query(...)` / `.filter(...)` / `.order_by(...)`) down to the root `$SESSION.query(...)` receiver.
  - Guard `$SESSION` via `TypeOraclePort::is_subtype_of(session_ty, "sqlalchemy.orm.Session") == TriState::True` (`R03` sound).
  - Deterministically lower:
    - Single entity `session.query(Model)...` -> `session.scalars(select(Model)...)` (converting `.filter(...)` -> `.where(...)`)
    - Multi-column `session.query(c1, c2)...` -> `session.execute(select(c1, c2)...)`
    - Terminal `.scalar()` / `.first()` -> `session.scalar(select(...)...)`
  - Automatically attach `required_imports: ["from sqlalchemy import select"]`.
- **Mandatory Tests (TDD First):**
  - Unit tests in `riffty_rules_builtin` covering all 7 SQLAlchemy chain shapes + negative test when receiver is not `sqlalchemy.orm.Session` (`TriState::Unknown` / unrelated `.query()` method).
  - Conformance test verifying Case 12 (all 14 sites in `fab_security_manager.py`) heals out-of-the-box with **0 custom YAML rules**.

---

### 3. Intra-Class Return-Type Inference for Unannotated / `-> Any` Helper Methods (`R24`)
- **Exposed by:** Airflow Case 06 (`Issue #63182` — `BigQueryHook` blocking sync calls inside `BigQueryTableAsyncHook` in [case_06_issue_63182_bigquery_async.yml](../airflow_issues_benchmark/rules/case_06_issue_63182_bigquery_async.yml)).
- **Root Cause:** `GoogleBaseAsyncHook.get_sync_hook()` is annotated as `-> Any`, even though `BigQueryTableAsyncHook.sync_hook_class = BigQueryHook` and `get_sync_hook()` returns `self.sync_hook_class(...)`. Because `riffty_adapter_ty` saw `-> Any`, it returned `TriState::Unknown` under `R03`, preventing `type_assignable_to: BigQueryHook` from matching `hook = await self.get_sync_hook()`.
- **Hexagonal Fix:**
  - Enhance `TyTypeOracleAdapter` (`crates/adapters/riffty_adapter_ty/src/lib.rs`) so that when a method's explicit return annotation is missing or `Any`:
    1. Inspect `return <Expr>` statements in the method body.
    2. Support class-attribute factory resolution (`self.<attr>(...)` where `<attr> = SomeClass` is assigned on `self`'s class or MRO).
- **Mandatory Tests (TDD First):**
  - Unit test in `riffty_adapter_ty`: subclass overrides `sync_hook_class = ConcreteHook`, base method annotated `-> Any` returns `self.sync_hook_class()`, local variable `hook = await self.get_sync_hook()` resolves to `ConcreteHook`.
  - Negative test: when multiple conflicting types or genuinely dynamic `Any` are returned, preserve `TriState::Unknown` (`R03`).

---

### 4. Impure Metavariable Duplication Diagnostic & Single-Evaluation Walrus Lowering (`R05+`)
- **Exposed by:** Airflow Case 02 (`RFT-015` in `db_manager.py` — [case_02_db_manager_ast_keywords.yml](../airflow_issues_benchmark/rules/case_02_db_manager_ast_keywords.yml)).
- **Root Cause:** When a YAML rule captures an arbitrary expression `$NODE` and references `$NODE` more than once in `replace`, `riffty_adapter_refaster` silently suppresses the fix under `R05` whenever `$NODE` is not `Purity::Pure`, and partial boolean sub-patterns inside `and` chains can match inner clauses without binding the outer `isinstance` check.
- **Hexagonal Fix:**
  - Add compile-time / check-time rule diagnostics when a Refaster YAML template duplicates a metavariable without an explicit `purity: pure` guard or walrus binding.
  - Add optional `bind_impure_once: true` support in `riffty_adapter_refaster` that automatically lowers duplicated impure captures into a single parenthesized walrus evaluation `(_riffty_node := $NODE)` when target Python >= 3.8.
- **Mandatory Tests (TDD First):**
  - Unit test in `riffty_adapter_refaster` verifying deterministic walrus binding on impure `Call` expressions and compile-time warning on unguarded duplication.

---

## Verification & Governance Gates (Boule Charter)

Every PR closing an item in this issue MUST pass all three deterministic gates with zero regressions:
1. `RUSTFLAGS="-D warnings" cargo test --workspace` — 100% unit, conformance, golden fixture, CLI, and MCP E2E suites green with zero warnings.
2. `cargo clippy --workspace --all-targets -- -D warnings` — zero compiler or lint warnings.
3. `node scripts/affirmation-tracer.mjs audit` — >= 95% Affirmation Verification Gate with 0 orphans (`AFM-051`..`AFM-054`).

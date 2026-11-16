# `riffty` (v2) Hexagonal Technical Tasks & Milestone DAG

---

## TASK-001: Milestone 1 — `crates/riffty_core` Pure Domain IR, Type Lattice, Fix Algebra & Driven Ports

### 1. Traceability & Context
- **Linked User Story**: [US-001](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md#us-001-pure-domain-core-contracts--sound-typefix-algebra)
- **Linked Affirmations**: `AFM-001`, `AFM-002`, `AFM-003`, `AFM-004`, `AFM-005`, `AFM-006`, `AFM-007`, `AFM-008`, `AFM-009`, `AFM-019`, `AFM-022`, `AFM-024`
- **Architectural Layer**: `Domain Core Entity | Value Object | Outbound Port`
- **Dependency DAG**: `blocks: [TASK-002, TASK-003, TASK-004, TASK-005, TASK-006]`

### 2. Hexagonal Boundary & Invariants
- `crates/riffty_core/Cargo.toml` depends **only** on `serde`, `serde_json`, and `thiserror`. Zero `ruff_*`, `ty_*`, `ast_grep*`, or `salsa` imports (`R01`).
- Implements exact canonical primitives (`ByteOffset`, `TextRange`, `FileId`, `NodeId`, `NodeKind`, `CanonicalNode<'a>`, `PrimitiveKind`, `CanonicalType`, `ParamInfo`, `TriState`, `Purity`, `Applicability`, `Edit`, `ImportSpec`, `Fix`, `Severity`, `Diagnostic`) and `riffty_core::ports` (`SyntaxParserPort`, `TypeOraclePort`, `PatternMatcherPort`, `ImportManagerPort`, `RulePlugin`, `WorkspaceVfsPort`, `PatchApplierPort`, `GitDiffPort`).

### 3. Implementation Checklist
- [x] Author unit tests for `TextRange`, `CanonicalType` subtype lattice, `TriState` Unknown/Any soundness, and Cargo.toml dependency purity.
- [x] Verify `RUSTFLAGS="-D warnings" cargo test -p riffty_core`.

---

## TASK-002: Milestone 2 — `crates/riffty_pipeline` Single-Pass Runner, Conflict Resolver & Fixpoint Loop

### 1. Traceability & Context
- **Linked User Story**: [US-002](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md#us-002-single-pass-pipeline-conflict-resolution--bounded-byte-offset-fixpoint-engine)
- **Linked Affirmations**: `AFM-016`, `AFM-017`, `AFM-018`, `AFM-023`, `AFM-025`, `AFM-027`, `AFM-028`, `AFM-029`, `AFM-030`
- **Architectural Layer**: `Inbound Port | Application Pipeline`
- **Dependency DAG**: `depends_on: [TASK-001]`, `blocks: [TASK-007]`

### 2. Hexagonal Boundary & Invariants
- Single-pass $O(\text{nodes})$ traversal dispatching nodes strictly to `RulePlugin`s subscribed to `node.kind` (`R04`).
- Resolves `Fix::required_imports` via `ImportManagerPort`, sorts `Edit`s by `(range.start, range.end)`, resolves overlaps deterministically (`Severity` desc, then earlier range), patches UTF-8 bytes bottom-to-top (`R06`–`R08`), and iterates up to 10 passes until fixpoint convergence (`R09`).

### 3. Implementation Checklist
- [x] Author unit tests for single-pass subscription filtering, overlapping edit conflict deferral, bottom-to-top multi-edit byte patching, and 10-pass cycle cap.
- [x] Verify `RUSTFLAGS="-D warnings" cargo test -p riffty_pipeline`.

---

## TASK-003: Milestone 3a — `riffty_adapter_ruff_ast` & `riffty_adapter_fs` Driven Adapters

### 1. Traceability & Context
- **Linked User Story**: [US-003](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md#us-003-driven-syntax-semantic-ty-structural-ast-grep--filesystemgit-adapters)
- **Linked Affirmations**: `AFM-003`, `AFM-027`, `AFM-035`
- **Architectural Layer**: `Driven Adapter`
- **Dependency DAG**: `depends_on: [TASK-001]`, `blocks: [TASK-004, TASK-006, TASK-007]`

### 2. Hexagonal Boundary & Invariants
- `RuffAstAdapter` implements `SyntaxParserPort` via `ruff_python_parser`, preserving exact byte `TextRange`s, call metadata, compare ops, and inline `# riffty: ignore[...]` suppressions.
- `RuffImportAdapter` implements `ImportManagerPort` using `ruff_python_parser` + `ruff_python_codegen::Stylist` newline detection to emit deduplicated byte-offset import insertion `Edit`s after module docstrings and `__future__` imports.
- `FsWorkspaceAdapter` implements `WorkspaceVfsPort`, `PatchApplierPort`, and `GitDiffPort` (`unstaged`, `staged`, `branch_diff` unified hunk -> byte `TextRange` translation).

---

## TASK-004: Milestone 3b — `riffty_adapter_ty` (Astral `ty` Salsa DB) & `riffty_adapter_ast_grep` + Conformance Suite

### 1. Traceability & Context
- **Linked User Story**: [US-003](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md#us-003-driven-syntax-semantic-ty-structural-ast-grep--filesystemgit-adapters)
- **Linked Affirmations**: `AFM-004`, `AFM-005`, `AFM-006`, `AFM-036`
- **Architectural Layer**: `Driven Adapter`
- **Dependency DAG**: `depends_on: [TASK-001, TASK-003]`, `blocks: [TASK-005, TASK-006, TASK-007]`

### 2. Hexagonal Boundary & Invariants
- `TyOracleAdapter` integrates Astral's `ty_python_semantic` (`SemanticModel`, `HasType`, `HasDefinition`) + `ruff_db` Salsa database (`TestDb` / in-memory virtual FS + vendored typeshed `ty_vendored`) alongside deterministic AST signature/purity/cross-file caller indexing to implement `TypeOraclePort` (`type_of`, `resolve_fqname`, `is_subtype`, `can_be_none`, `expression_purity`, `reverse_dependents`), plus `NullTypeOracle` fallback.
- `AstGrepAdapter` implements `PatternMatcherPort` via `ast-grep-core` + `ast-grep-language::Python`, supporting single `$VAR` and variadic `$$$ARGS` (including zero-arg trailing-comma calls).
- Verified via `RUSTFLAGS="-D warnings" cargo test --test conformance`.

---

## TASK-005: Milestone 4a — `riffty_adapter_refaster` Declarative YAML Rule Engine

### 1. Traceability & Context
- **Linked User Story**: [US-004](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md#us-004-refaster-declarative-yaml-rules-with-purity-guarded-rewrites)
- **Linked Affirmations**: `AFM-006`, `AFM-015`, `AFM-026`
- **Architectural Layer**: `Driven Adapter`
- **Dependency DAG**: `depends_on: [TASK-001, TASK-004]`, `blocks: [TASK-007]`

### 2. Hexagonal Boundary & Invariants
- Compiles Refaster YAML rule specs into `RulePlugin`s evaluated on `NodeKind::Module`.
- Evaluates `PatternMatcherPort` matches against `TypeOraclePort` guards (`fqname_in`, `type_assignable_to`, `purity`, `callable_accepts_kwarg`) and enforces `R05`: metavariables referenced >= 2 times in a replacement template require `expression_purity == Purity::Pure`.

---

## TASK-006: Milestone 4b — `crates/riffty_rules_builtin` Flagship Rules & Golden SciPy #20068 Fixture

### 1. Traceability & Context
- **Linked User Story**: [US-005](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md#us-005-native-rust-bugcheckers--conditional-scipy-20068-assert_warns-migration)
- **Linked Affirmations**: `AFM-011`, `AFM-012`, `AFM-013`, `AFM-014`, `AFM-031`, `AFM-032`, `AFM-033`, `AFM-034`, `AFM-038`, `AFM-039`, `AFM-040`
- **Architectural Layer**: `Domain Rule Plugins`
- **Dependency DAG**: `depends_on: [TASK-001, TASK-003, TASK-004]`, `blocks: [TASK-007]`

### 2. Hexagonal Boundary & Invariants
- `RFT-004` (`UnawaitedCoroutine`): Flags bare expression-statement calls to `Callable { is_async: true }` without `await`, `return`, or assignment (`R12`).
- `RFT-015` (`CollectionIncompatibleType`): Flags `x in container` / `x not in container` when `x` and container element type are provably disjoint (`R13`).
- `RFT-118` (`WarningsMissingStacklevel`): Injects `stacklevel=2` into `warnings.warn(...)` as `Applicability::Safe` when omitted (`R11`).
- `RFT-202` + `RFT-301` (`AssertWarnsMigration`): Rewrites `np.testing.assert_warns(W, fn, *args, match=...)` -> `with pytest.warns(W, match=...): fn(*args)` with `ImportSpec { module: "pytest", symbol: None }`. Classifies as `Applicability::Safe` (`RFT-202`) when `fn` is a `Callable` that does NOT accept `match` and has `has_var_kwargs == false` (or when `match=` is not passed), and `Applicability::Unsafe` / `Error` (`RFT-301` with `type_proof`) when `fn` accepts `match` or `has_var_kwargs == true` or `match=` is silently swallowed (`R10`).

---

## TASK-007: Milestone 5 — `ReviewFacade`, `apps/riffty_cli`, & `apps/riffty_mcp`

### 1. Traceability & Context
- **Linked User Story**: [US-006](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md#us-006-developer-cli-riffty-check---fix--ai-reviewer-mcp-server-riffty_review_changes)
- **Linked Affirmations**: `AFM-010`, `AFM-020`, `AFM-021`, `AFM-035`, `AFM-036`, `AFM-037`
- **Architectural Layer**: `Driving Adapter | Application Facade`
- **Dependency DAG**: `depends_on: [TASK-002, TASK-003, TASK-004, TASK-005, TASK-006]`

### 2. Hexagonal Boundary & Invariants
- `ReviewFacade` extracts touched `TextRange`s from `GitDiffPort`, expands blast radius via `TypeOraclePort::reverse_dependents`, suppresses untouched baseline diagnostics, optionally heals `Applicability::Safe` fixes on disk (`auto_heal: true`), and emits the exact JSON verdict (`APPROVED` / `REQUEST_CHANGES`, `summary`, `auto_healed`, `blocking_comments`).
- `apps/riffty_cli` exposes `riffty check [--fix] [--unsafe-fixes] [--diff] [--format text|json] [--rules <yaml>]` and `riffty mcp` subcommand delegating to `riffty_mcp`.
- `apps/riffty_mcp` exposes `riffty_review_changes` over stdio JSON-RPC (`rmcp` / MCP 2024-11-05 protocol).

---

## TASK-008: Milestone 6a — Core v2.1 Soundness Lattice, First-Class Control/Class `NodeKind`s & `RFT-015` Overhaul

### 1. Traceability & Context
- **Linked User Story**: [US-007](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md#us-007-v21-open-world-soundness-polymorphic-dispatch-safety--lexical-scope-remediation)
- **Linked Affirmations**: `AFM-041`, `AFM-044`, `AFM-046`, `AFM-049`
- **Architectural Layer**: `Domain Core | Driven Syntax & Type Adapters | Builtin Rules`
- **Dependency DAG**: `depends_on: [TASK-001, TASK-003, TASK-004, TASK-006]`, `blocks: [TASK-009, TASK-010]`

### 2. Hexagonal Boundary & Invariants
- Expand `NodeKind` in `riffty_core` to 17 variants (`ClassDef`, `With`, `If`, `Try`) with `ClassMeta`, `WithMeta`, and `IfMeta` populated by `riffty_adapter_ruff_ast` (`R21`).
- Enforce open-world `eval_is_subtype` conservatism (`TriState::Unknown` for `(Nominal, Primitive)` and distinct `(Nominal, Nominal)` without complete MRO proof), `TypeOraclePort::has_custom_eq`, `Literal[...]` / `str`-`Enum` / `@property` / `<class 'X'>` / `&` intersection normalization, and local variable shadowing in `TyOracleAdapter` (`R13`, `R18`).

---

## TASK-009: Milestone 6b — Polymorphic Dispatch Safety (`OverrideRisk`), PEP 562 `TYPE_CHECKING` MRO & `--python-venv`

### 1. Traceability & Context
- **Linked User Story**: [US-007](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md#us-007-v21-open-world-soundness-polymorphic-dispatch-safety--lexical-scope-remediation)
- **Linked Affirmations**: `AFM-041`, `AFM-043`, `AFM-045`, `AFM-050`
- **Architectural Layer**: `Domain Core | Driven Type & Refaster Adapters | CLI`
- **Dependency DAG**: `depends_on: [TASK-005, TASK-008]`

### 2. Hexagonal Boundary & Invariants
- Implement `OverrideRisk` and `TypeOraclePort::method_override_risk`, plus `RefasterGuard::NotOverriddenBelow`, `RefasterGuard::NotInsideWith`, and polymorphic `Applicability::Unsafe` downgrade / dispatch-guarded templates (`R17`).
- Index PEP 562 `if TYPE_CHECKING:` imports/re-exports, discover canonical `__init__.py` package roots for monorepos, preserve `pydantic.v1.BaseModel` separation, and add optional `--python-venv` (`RIFFTY_PYTHON_VENV`) site-packages mounting (`R22`).

---

## TASK-010: Milestone 6c — Lexical Scope-Aware `ImportManagerPort` & Python 3.12+ `skip_file_prefixes` (`RFT-118`/`RFT-119`)

### 1. Traceability & Context
- **Linked User Story**: [US-007](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md#us-007-v21-open-world-soundness-polymorphic-dispatch-safety--lexical-scope-remediation)
- **Linked Affirmations**: `AFM-042`, `AFM-044`, `AFM-047`, `AFM-048`
- **Architectural Layer**: `Domain Core | Pipeline | Driven Import Adapter | Builtin Rules`
- **Dependency DAG**: `depends_on: [TASK-002, TASK-003, TASK-006, TASK-008]`

### 2. Hexagonal Boundary & Invariants
- Add `ScopedImportRequest` and `ImportManagerPort::resolve_scoped_import_edits` wired through `riffty_pipeline` so function-local shadowing imports and single-symbol deprecated module imports are rewritten in-place (`R19`).
- Suppress `RFT-118` when `skip_file_prefixes=` is passed, and add native `RFT-119` (`warnings-narrow-skip-file-prefixes`) catching narrow `os.path.dirname(__file__)` prefixes inside internal helper subpackages (`R20`).

---

## Production Readiness Tasks (7 Pillars for Local CLI & Stdio MCP Engine)

- **TASK-PROD-001 (Security & Workspace Sandboxing)**: Enforce canonical path containment inside workspace root when applying byte-offset patches on disk (`PatchApplierPort` rejects path traversal outside workspace).
- **TASK-PROD-002 (Deterministic State & Cache Lifecycle)**: Thread-safe in-memory semantic/Salsa indexing with deterministic sorted file IDs; zero ad-hoc mutable external DB dependencies.
- **TASK-PROD-003 (Structured JSON & MCP Error Contracts)**: Validated MCP tool input schema (`target`, `base_ref`, `auto_heal`) and structured JSON-RPC error codes (`-32602` invalid params, `-32603` internal error) plus `--format json` diagnostic schema.
- **TASK-PROD-004 (Observability & Execution Telemetry)**: Deterministic pass counts, files reviewed, auto-healed counts, and structured stderr diagnostic summaries without polluting MCP stdout JSON-RPC frames.
- **TASK-PROD-005 (Resource Bounds & Cycle Hardening)**: Bounded 10-pass fixpoint cap, cycle hash detection, and UTF-8 boundary guards preventing panics or runaway rewrites on malformed inputs.
- **TASK-PROD-006 (12-Factor Configuration)**: Environment variable overrides (`RIFFTY_MAX_PASSES`, `RIFFTY_FORMAT`, `RIFFTY_RULES_PATH`) documented in `.env.example` with zero hardcoded host paths.
- **TASK-PROD-007 (Containerization & CI/CD)**: Multi-stage non-root `Dockerfile`, `docker-compose.yml`, and `.github/workflows/ci.yml` enforcing `RUSTFLAGS="-D warnings" cargo test --workspace` and `clippy`.


# `riffty` (v2) User Stories (Mike Cohn / INVEST / SPIDR)

---

## US-001: Pure Domain Core Contracts & Sound Type/Fix Algebra

### 1. Cohn User Story Narrative
**As a** static-analysis rule author and engine maintainer,
**I want** a zero-dependency `riffty_core` domain crate defining canonical UTF-8 byte coordinates, syntax nodes, semantic types, three-valued subtype logic, purity lattice, byte-offset `Fix` algebra, and Driven Port traits,
**So that** rules and orchestration remain 100% insulated from upstream `ruff`, `ty`, `salsa`, and `ast-grep` internal API churn while guaranteeing zero false positives.

### 2. Affirmation Traceability Anchor
- **Linked Affirmation IDs**: `AFM-001`, `AFM-002`, `AFM-003`, `AFM-004`, `AFM-005`, `AFM-006`, `AFM-007`, `AFM-008`, `AFM-009`, `AFM-019`, `AFM-022`, `AFM-024`
- **Domain Ontology Anchor**:
  - Target Classification/Obstacle: `SUPPRESSED_BY_TYPE_ORACLE`
  - Target Terminal State: `CLEAN`, `SUPPRESSED_BY_TYPE_ORACLE`
  - Governing Policy Rules: `R01`, `R03`
- **Dependency DAG**: `blocks: [US-002, US-003, US-004, US-005]`

### 3. Acceptance Criteria (Gherkin BDD Format)
```gherkin
Scenario: Pure Cargo manifest and zero-false-positive Unknown/Any suppression
  Given crates/riffty_core/Cargo.toml contains zero ruff_*, ty_*, ast_grep*, or salsa dependencies
  When a rule queries TypeOraclePort and receives CanonicalType::Unknown or CanonicalType::Any
  Then is_subtype returns TriState::Unknown and type-dependent diagnostics are suppressed
  And ByteOffset and TextRange slices preserve exact UTF-8 byte boundaries
```

### 4. INVEST Quality Assessment
- [x] **Independent** | [x] **Negotiable** | [x] **Valuable** | [x] **Estimable** | [x] **Small** | [x] **Testable**

### 5. SPIDR Splitting Strategy
- **Splitting Dimension**: `Interfaces`
- **Rationale**: Separates pure domain contracts and port traits from upstream adapter implementations.

---

## US-002: Single-Pass Pipeline, Conflict Resolution & Bounded Byte-Offset Fixpoint Engine

### 1. Cohn User Story Narrative
**As a** Python engineer running automated code remediation,
**I want** `riffty_pipeline` to dispatch rules in a single $O(\text{nodes})$ pass, synthesize structured imports, resolve overlapping edits deterministically, patch UTF-8 byte ranges bottom-to-top, and converge within at most 10 fixpoint iterations,
**So that** multi-rule auto-fixes never corrupt byte coordinates, lose comments, or enter infinite rewrite loops.

### 2. Affirmation Traceability Anchor
- **Linked Affirmation IDs**: `AFM-016`, `AFM-017`, `AFM-018`, `AFM-023`, `AFM-025`, `AFM-027`, `AFM-028`, `AFM-029`, `AFM-030`
- **Domain Ontology Anchor**:
  - Target Classification/Obstacle: `OMITTED_WARNING_STACKLEVEL`, `SILENT_KWARG_FORWARDING_TRAP`
  - Target Terminal State: `CLEAN`, `AUTO_HEALED`, `DIAGNOSED_UNSAFE_OR_MANUAL`
  - Governing Policy Rules: `R02`, `R04`, `R06`, `R07`, `R08`, `R09`
- **Dependency DAG**: `depends_on: [US-001]`, `blocks: [US-006]`

### 3. Acceptance Criteria (Gherkin BDD Format)
```gherkin
Scenario: Overlapping Safe fixes resolve deterministically and converge bottom-to-top
  Given two rules emit overlapping Applicability::Safe edits on the same Python statement
  When the fixpoint loop executes with max 10 passes
  Then pass 1 applies the higher-severity or earlier-starting edit bottom-to-top and defers the overlapping edit
  And pass 2 re-parses and applies the remaining fix while preserving all inline comments and formatting
```

### 4. INVEST Quality Assessment
- [x] **Independent** | [x] **Negotiable** | [x] **Valuable** | [x] **Estimable** | [x] **Small** | [x] **Testable**

### 5. SPIDR Splitting Strategy
- **Splitting Dimension**: `Rules`
- **Rationale**: Isolates pipeline scheduling, conflict resolution, and byte patching from concrete lint logic.

---

## US-003: Driven Syntax, Semantic (`ty`), Structural (`ast-grep`) & Filesystem/Git Adapters

### 1. Cohn User Story Narrative
**As a** platform architect integrating best-of-breed Rust analysis engines,
**I want** thin Driven Adapters wrapping `ruff_python_parser`/`ruff_python_codegen`, Astral's `ty` Salsa DB (`ty_python_semantic`/`ruff_db` + `NullTypeOracle`), `ast-grep-core`, and workspace VFS/git-diff operations,
**So that** `riffty` resolves real Python AST spans, callable signatures, type lattices, reverse dependents, and structural metavariables behind stable domain ports.

### 2. Affirmation Traceability Anchor
- **Linked Affirmation IDs**: `AFM-003`, `AFM-004`, `AFM-005`, `AFM-006`, `AFM-027`, `AFM-036`
- **Domain Ontology Anchor**:
  - Target Classification/Obstacle: `UNRESOLVED_ASYNC_COROUTINE`, `DISJOINT_COLLECTION_MEMBERSHIP`
  - Target Terminal State: `CLEAN`, `SUPPRESSED_BY_TYPE_ORACLE`
  - Governing Policy Rules: `R01`, `R03`, `R06`, `R15`
- **Dependency DAG**: `depends_on: [US-001]`, `blocks: [US-004, US-005, US-006]`

### 3. Acceptance Criteria (Gherkin BDD Format)
```gherkin
Scenario: Conformance verification across Ruff AST, Ty Salsa Oracle, and AstGrep adapters
  Given Python workspace modules with typed callables, imports, async defs, and cross-file callers
  When SyntaxParserPort, ImportManagerPort, TypeOraclePort, and PatternMatcherPort execute
  Then exact byte TextRanges, CanonicalType::Callable signatures, reverse_dependents, and $VAR/$$$ARGS captures match conformance contracts
  And NullTypeOracle returns CanonicalType::Unknown cleanly when semantic resolution is disabled
```

### 4. INVEST Quality Assessment
- [x] **Independent** | [x] **Negotiable** | [x] **Valuable** | [x] **Estimable** | [x] **Small** | [x] **Testable**

### 5. SPIDR Splitting Strategy
- **Splitting Dimension**: `Interfaces`
- **Rationale**: Encapsulates third-party engine crates behind `crates/adapters/*` verified by `tests/conformance`.

---

## US-004: Refaster Declarative YAML Rules with Purity-Guarded Rewrites

### 1. Cohn User Story Narrative
**As a** framework maintainer authoring library migration rules,
**I want** `riffty_adapter_refaster` to evaluate declarative YAML rules combining `ast-grep` patterns (`$VAR`, `$$$ARGS`) with `TypeOraclePort` guards (`fqname_in`, `type_assignable_to`, `purity`, `callable_accepts_kwarg`),
**So that** framework idioms migrate deterministically via byte-offset templates without ever duplicating side-effecting expressions.

### 2. Affirmation Traceability Anchor
- **Linked Affirmation IDs**: `AFM-006`, `AFM-015`, `AFM-026`
- **Domain Ontology Anchor**:
  - Target Classification/Obstacle: `REFASTER_DECLARATIVE_IDIOM`
  - Target Terminal State: `AUTO_HEALED`, `DIAGNOSED_UNSAFE_OR_MANUAL`
  - Governing Policy Rules: `R02`, `R05`, `R06`
- **Dependency DAG**: `depends_on: [US-001, US-003]`, `blocks: [US-006]`

### 3. Acceptance Criteria (Gherkin BDD Format)
```gherkin
Scenario: Refaster rule blocks metavariable duplication when expression is SideEffecting
  Given a YAML Refaster rule whose replacement template references $X twice
  When $X matches a pure variable vs. an impure function call foo()
  Then the pure variable match synthesizes an Applicability::Safe Fix
  And the SideEffecting call foo() refuses Safe duplication to preserve runtime semantics
```

### 4. INVEST Quality Assessment
- [x] **Independent** | [x] **Negotiable** | [x] **Valuable** | [x] **Estimable** | [x] **Small** | [x] **Testable**

### 5. SPIDR Splitting Strategy
- **Splitting Dimension**: `Data`
- **Rationale**: Evaluates declarative YAML rule specifications over structural and semantic ports.

---

## US-005: Native Rust BugCheckers & Conditional SciPy #20068 `assert_warns` Migration

### 1. Cohn User Story Narrative
**As a** scientific Python developer maintaining test suites and async services,
**I want** native Rust rules `RFT-004` (unawaited coroutine), `RFT-015` (disjoint `x in container`), `RFT-118` (`warnings.warn` missing `stacklevel=2`), and `RFT-202` + `RFT-301` (`np.testing.assert_warns` conditional `Safe`/`Unsafe` migration to `with pytest.warns(...):`),
**So that** real semantic bugs like SciPy #20068 are caught with structured `type_proof` witnesses and rewritten safely while preserving every comment.

### 2. Affirmation Traceability Anchor
- **Linked Affirmation IDs**: `AFM-011`, `AFM-012`, `AFM-013`, `AFM-014`, `AFM-031`, `AFM-032`, `AFM-033`, `AFM-034`, `AFM-038`, `AFM-039`, `AFM-040`
- **Domain Ontology Anchor**:
  - Target Classification/Obstacle: `UNRESOLVED_ASYNC_COROUTINE`, `DISJOINT_COLLECTION_MEMBERSHIP`, `OMITTED_WARNING_STACKLEVEL`, `SILENT_KWARG_FORWARDING_TRAP`
  - Target Terminal State: `AUTO_HEALED`, `DIAGNOSED_UNSAFE_OR_MANUAL`, `SUPPRESSED_BY_TYPE_ORACLE`
  - Governing Policy Rules: `R02`, `R03`, `R10`, `R11`, `R12`, `R13`
- **Dependency DAG**: `depends_on: [US-001, US-003]`, `blocks: [US-006]`

### 3. Acceptance Criteria (Gherkin BDD Format)
```gherkin
Scenario: Golden SciPy #20068 conditional Safe vs Unsafe callable-signature verification
  Given def calculate_average(values): ... and def wrapper(*args, **kwargs): ...
  When np.testing.assert_warns(RuntimeWarning, calculate_average, [], match="empty") is analyzed
  Then RFT-301/RFT-202 classifies the fix as Applicability::Safe, injects import pytest, and rewrites to with pytest.warns(RuntimeWarning, match="empty"): calculate_average([]) preserving all comments
  And np.testing.assert_warns(RuntimeWarning, wrapper, [], match="empty") downgrades to Applicability::Unsafe because wrapper has has_var_kwargs == true
```

### 4. INVEST Quality Assessment
- [x] **Independent** | [x] **Negotiable** | [x] **Valuable** | [x] **Estimable** | [x] **Small** | [x] **Testable**

### 5. SPIDR Splitting Strategy
- **Splitting Dimension**: `Rules`
- **Rationale**: Implements the four flagship semantic rules and golden regression test gate.

---

## US-006: Developer CLI (`riffty check --fix`) & AI Reviewer MCP Server (`riffty_review_changes`)

### 1. Cohn User Story Narrative
**As a** developer or AI coding agent (Claude Code, Cursor, Codex),
**I want** `riffty check [--fix] [--diff]` for CLI/CI execution and `riffty mcp` exposing `riffty_review_changes` over stdio JSON-RPC with git-diff scoping, `reverse_dependents` blast-radius expansion, and optional `auto_heal`,
**So that** touched code and impacted callers are auto-healed on disk and reviewed with a compact `APPROVED` / `REQUEST_CHANGES` JSON verdict at zero LLM token cost.

### 2. Affirmation Traceability Anchor
- **Linked Affirmation IDs**: `AFM-010`, `AFM-020`, `AFM-021`, `AFM-035`, `AFM-036`, `AFM-037`
- **Domain Ontology Anchor**:
  - Target Classification/Obstacle: `SILENT_KWARG_FORWARDING_TRAP`, `OMITTED_WARNING_STACKLEVEL`
  - Target Terminal State: `APPROVED`, `REQUEST_CHANGES`, `AUTO_HEALED`
  - Governing Policy Rules: `R14`, `R15`, `R16`
- **Dependency DAG**: `depends_on: [US-002, US-003, US-004, US-005]`

### 3. Acceptance Criteria (Gherkin BDD Format)
```gherkin
Scenario: MCP riffty_review_changes auto-heals Safe fixes, suppresses untouched baseline, and flags reverse-dependent regressions
  Given a git repository with legacy untouched warnings, a modified function signature, and a newly added warnings.warn("msg")
  When riffty_review_changes is invoked with {"target": "unstaged", "base_ref": "main", "auto_heal": true}
  Then warnings.warn("msg") is auto-healed on disk with stacklevel=2 and recorded in auto_healed
  And untouched baseline diagnostics outside the diff and reverse_dependents are suppressed
  And broken callers or unsafe issues return verdict "REQUEST_CHANGES" with structured blocking_comments
```

### 4. INVEST Quality Assessment
- [x] **Independent** | [x] **Negotiable** | [x] **Valuable** | [x] **Estimable** | [x] **Small** | [x] **Testable**

### 5. SPIDR Splitting Strategy
- **Splitting Dimension**: `Interfaces`
- **Rationale**: Exposes the two thin driving adapters (`apps/riffty_cli` and `apps/riffty_mcp`) over `ReviewFacade`.

---

## US-007: v2.1 Open-World Soundness, Polymorphic Dispatch Safety & Lexical-Scope Remediation

### 1. Cohn User Story Narrative
**As a** maintainer of large polymorphic Python frameworks (`langchain`, `airflow`, `scipy`, `bump-pydantic`),
**I want** `riffty` v2.1 to guard base-class method migrations against subclass overrides and suppression blocks (`R17`), enforce closed-MRO and custom-`__eq__` conservatism on `RFT-015` (`R18`), rewrite shadowed function-local and single-symbol deprecated imports in-place (`R19`), honor Python 3.12+ `skip_file_prefixes=` while flagging narrow helper prefixes via `RFT-119` (`R20`), emit first-class `ClassDef`/`With`/`If`/`Try` syntax nodes (`R21`), and resolve PEP 562 `if TYPE_CHECKING:` MRO chains across monorepo packages (`R22`),
**So that** automated `--fix` runs never bypass subclass overrides, never leave shadowed local imports behind, and achieve zero false positives across all 12 benchmark repositories.

### 2. Affirmation Traceability Anchor
- **Linked Affirmation IDs**: `AFM-041`, `AFM-042`, `AFM-043`, `AFM-044`, `AFM-045`, `AFM-046`, `AFM-047`, `AFM-048`, `AFM-049`, `AFM-050`
- **Domain Ontology Anchor**:
  - Target Classification/Obstacle: `POLYMORPHIC_OVERRIDE_BYPASS`, `DISJOINT_COLLECTION_MEMBERSHIP`, `NARROW_SKIP_FILE_PREFIXES`
  - Target Terminal State: `AUTO_HEALED`, `DIAGNOSED_UNSAFE_OR_MANUAL`, `SUPPRESSED_BY_TYPE_ORACLE`, `CLEAN`
  - Governing Policy Rules: `R17`, `R18`, `R19`, `R20`, `R21`, `R22`
- **Dependency DAG**: `depends_on: [US-001, US-002, US-003, US-004, US-005, US-006]`

### 3. Acceptance Criteria (Gherkin BDD Format)
```gherkin
Scenario: Polymorphic override preservation, sound Enum/Property RFT-015, lexical local import rewrite, and skip_file_prefixes awareness
  Given a PromptTemplate subclass overriding def dict(self) and a BasePromptTemplate._dict_for_compat() compatibility shim inside with suppress_langchain_deprecation_warning():
  And a class Rule(str, Enum) evaluated via Rule.BP001 not in disabled and an Airflow function containing a local from airflow.sdk._shared.secrets_masker import mask_secret
  And warnings.warn(..., skip_file_prefixes=(os.path.dirname(__file__),)) inside scipy/_lib/_util.py
  When riffty check --fix executes across the workspace
  Then concrete closed .dict() calls migrate safely to .model_dump() while polymorphic self.dict() shims are preserved from blind Safe overwrite
  And RFT-015 emits 0 false positives on str-Enum, Literal, and @property members while still catching true disjoint bugs (ast.keyword vs str, int vs set[str])
  And the function-local mask_secret import is rewritten in-place to from airflow.sdk.log import mask_secret without module-top shadowing
  And RFT-118 skips warnings.warn calls carrying skip_file_prefixes= while RFT-119 flags narrow _lib/__file__ prefixes
```

### 4. INVEST Quality Assessment
- [x] **Independent** | [x] **Negotiable** | [x] **Valuable** | [x] **Estimable** | [x] **Small** | [x] **Testable**

### 5. SPIDR Splitting Strategy
- **Splitting Dimension**: `Rules`
- **Rationale**: Closes the six postmortem soundness and scope gaps (`GAP-01` through `GAP-06`) across core, adapters, and rules.



## 1. Product Scope & Strategic Boundaries

### In Scope (Build This)
1. **Type-Aware Correctness & Bug Detection:** Catch high-confidence Python bugs that pass syntactic linters (Ruff) because they require resolved type/signature/control-flow proof (e.g., unawaited coroutines, `x in set[Y]` with disjoint types, `**kwargs` wrappers silently swallowing invalid arguments).
2. **Deterministic Byte-Offset Auto-Fixes:** Rewrite broken idioms and framework migrations (e.g., `np.testing.assert_warns` -> `pytest.warns`, Pydantic/SQLAlchemy/FastAPI migrations) via exact UTF-8 byte `TextRange` edits—never unparsing/reprinting the AST—preserving all comments and formatting.
3. **Refaster-Style Declarative YAML Rules + Native Rust Rules:** Support structural pattern matching (`ast-grep`) guarded by resolved type/purity queries (`ty`).
4. **Two Driving Interfaces:**
   - **CLI (`riffty check --fix`)** for developers and CI/CD.
   - **MCP Reviewer Server (`riffty mcp`)** exposing `riffty_review_changes` so AI coding agents (Claude Code, Cursor, Codex) can auto-heal diffs and receive compact, deterministic review verdicts without burning LLM tokens.

### Out of Scope (DO NOT Build)
- Style formatting or PEP8 lints (delegated to `ruff format` / `ruff check`).
- Generic type checking from scratch (delegated to `ty` / `pyright`).
- Traditional AppSec SAST / CVE taint tracking (delegated to Semgrep / CodeQL).
- WASM plugin runtimes, Python RPC subprocess workers, or A2A HTTP servers.

---

## 2. Hard Architectural Invariants (Hexagonal Ports & Adapters)

1. **Zero Upstream Leakage into `riffty_core`:**
   `crates/riffty_core/Cargo.toml` MUST NOT depend on `ruff_*`, `ty_*`, `ast_grep*`, or `salsa`. All third-party engines sit behind traits in `riffty_core::ports` inside `crates/adapters/*`.
2. **Byte-Offset Edits Only:**
   Every `Fix` MUST consist of UTF-8 byte `TextRange` replacements against the original source string, plus structured `ImportSpec` requests resolved by `ImportManagerPort`. Never pretty-print an AST.
3. **Zero-False-Positive Soundness:**
   If `TypeOraclePort` returns `CanonicalType::Unknown` or `CanonicalType::Any`, type-dependent rules MUST suppress diagnostics rather than guess.
4. **No Upstream API Guessing:**
   Before coding any crate in `crates/adapters/`, inspect the actual source/signatures of the pinned `astral-sh/ruff` (`ruff_python_parser`, `ruff_python_codegen`, `ty_python_semantic`, `ruff_db`) and `ast-grep-core` crates in the Cargo cache/vendor directory. Isolate upstream calls behind thin adapter structs so upstream refactors touch only one crate.

---

## 3. Cargo Workspace Layout

```text
riffty/
├── Cargo.toml # Workspace pinning astral-sh/ruff git rev + ast-grep-core + rmcp/serde
├── crates/
│ ├── riffty_core/ # ZERO external parser/type deps: IR, TypeLattice, Fix Algebra, Ports
│ ├── riffty_pipeline/ # Pluggable stages, conflict resolver, fixpoint loop, ReviewFacade
│ ├── riffty_rules_builtin/ # Native Rust BugCheckers (RFT-004, RFT-015, RFT-118, RFT-301)
│ └── adapters/
│ ├── riffty_adapter_ruff_ast/ # Implements SyntaxParserPort + ImportManagerPort via Ruff
│ ├── riffty_adapter_ty/ # Implements TypeOraclePort via Astral's `ty` Salsa DB
│ ├── riffty_adapter_ast_grep/ # Implements PatternMatcherPort via `ast-grep-core`
│ ├── riffty_adapter_refaster/ # Evaluates YAML rules: ast-grep spans -> TypeOraclePort guards
│ └── riffty_adapter_fs/ # Implements WorkspaceVfsPort + PatchApplierPort + GitDiffPort
├── apps/
│ ├── riffty_cli/ # Driving Adapter: `riffty check [--fix] [--diff]`
│ └── riffty_mcp/ # Driving Adapter: MCP stdio server exposing `riffty_review_changes`
└── tests/
    └── conformance/ # Adapter contract tests + golden regression fixtures (SciPy #20068)
```

## 4. Core Domain Contracts (`crates/riffty_core`)

Implement these exact canonical primitives and Driven Port traits in `riffty_core`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ByteOffset(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextRange {
    pub start: ByteOffset,
    pub end: ByteOffset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Module, FunctionDef, AsyncFunctionDef, Call, Compare, Attribute,
    Name, Subscript, Return, Await, Import, ImportFrom, Other,
}

pub struct CanonicalNode<'a> {
    pub id: NodeId,
    pub kind: NodeKind,
    pub range: TextRange,
    pub parent: Option<NodeId>,
    pub raw_text: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalType {
    Unknown,
    Any,
    NoneType,
    Primitive(PrimitiveKind), // Int, Str, Float, Bool, Bytes
    Nominal { fqname: String, args: Vec<CanonicalType> },
    Union(Vec<CanonicalType>),
    Callable {
        params: Vec<ParamInfo>,
        has_var_args: bool,
        has_var_kwargs: bool,
        return_type: Box<CanonicalType>,
        is_async: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamInfo {
    pub name: String,
    pub param_type: CanonicalType,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriState { Yes, No, Unknown }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purity { Pure, ReadOnly, SideEffecting }

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Applicability {
    Safe, // Provably preserves or fixes broken semantics (--fix)
    Unsafe, // Could alter valid behavior; requires --unsafe-fixes or human review
    ManualOnly, // Diagnostic only
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub range: TextRange,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSpec {
    pub module: String,
    pub symbol: Option<String>, // None => `import <module>`, Some(s) => `from <module> import <s>`
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    pub title: String,
    pub edits: Vec<Edit>,
    pub required_imports: Vec<ImportSpec>,
    pub applicability: Applicability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity { Error, Warning, Info }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub rule_id: String,
    pub rule_name: String,
    pub severity: Severity,
    pub file_id: FileId,
    pub range: TextRange,
    pub message: String,
    pub type_proof: Option<serde_json::Value>,
    pub fix: Option<Fix>,
}
```

### Driven Ports (`riffty_core::ports`)

```rust
pub trait SyntaxParserPort: Send + Sync {
    fn parse(&self, file_id: FileId, source: &str) -> Result<CanonicalSyntaxTree, String>;
}

pub trait TypeOraclePort: Send + Sync {
    fn type_of(&self, file_id: FileId, range: TextRange) -> CanonicalType;
    fn resolve_fqname(&self, file_id: FileId, range: TextRange) -> Option<String>;
    fn is_subtype(&self, sub: &CanonicalType, sup: &CanonicalType) -> TriState;
    fn can_be_none(&self, file_id: FileId, range: TextRange) -> TriState;
    fn expression_purity(&self, file_id: FileId, range: TextRange) -> Purity;
    /// Returns ranges in other workspace files that call or depend on symbols modified in `changed_ranges`.
    fn reverse_dependents(&self, file_id: FileId, changed_ranges: &[TextRange]) -> Vec<(FileId, TextRange)>;
}

pub trait PatternMatcherPort: Send + Sync {
    fn find_matches(&self, source: &str, pattern: &str) -> Result<Vec<PatternMatch>, String>;
}

pub trait ImportManagerPort: Send + Sync {
    fn resolve_import_edits(&self, source: &str, imports: &[ImportSpec]) -> Vec<Edit>;
}

pub trait RulePlugin: Send + Sync {
    fn id(&self) -> &'static str;
    fn target_node_kinds(&self) -> &[NodeKind];
    fn evaluate(&self, node: &CanonicalNode, view: &AnalysisView) -> Vec<Diagnostic>;
}
```

## 5. Pluggable Pipeline & Fixer Mechanics (`crates/riffty_pipeline`)
 * **Single-Pass Dispatch:** Traverse `CanonicalSyntaxTree` once ($O(\text{nodes})$), dispatching each node only to `RulePlugin`s subscribed to `node.kind`.
 * **Refaster YAML Bridge (`riffty_adapter_refaster`):**
   * `PatternMatcherPort` (`ast-grep-core`) matches structural patterns and returns byte `TextRange`s for the match and captured metavariables (`$VAR`, `$$$ARGS`).
   * Map those byte ranges to `TypeOraclePort` queries (`fqname_in`, `type_assignable_to`, `purity: pure`, `callable_accepts_kwarg`).
   * Never duplicate a metavariable in a `Fix` replacement unless `expression_purity == Purity::Pure`.
 * **Conflict Resolution & Fixpoint Loop:**
   * Resolve `Fix::required_imports` via `ImportManagerPort` into concrete `Edit`s.
   * Sort all `Edit`s by `range.start`. If two fixes overlap, apply the higher-severity/earlier fix and defer the conflicting one.
   * Apply non-overlapping edits bottom-to-top on the source string.
   * Re-run incrementally up to 10 iterations until convergence (zero new `Safe` fixes).

## 6. MCP Reviewer Contract (`apps/riffty_mcp` + `ReviewFacade`)
Expose an MCP tool named `riffty_review_changes` over stdio JSON-RPC:
 * **Inputs:** `{ "target": "unstaged" | "staged" | "branch_diff", "base_ref": "main", "auto_heal": true }`
 * **ReviewFacade Behavior:**
   * Extract touched `TextRange`s from `git diff`.
   * Expand review scope using `TypeOraclePort::reverse_dependents` (if a function signature/return type changed, audit its callers across the repo).
   * Suppress baseline diagnostics on untouched spans.
   * If `auto_heal == true`, apply all `Applicability::Safe` fixes on disk.
   * Return a token-compact JSON verdict (`"APPROVED"` if `blocking_issues == 0`, else `"REQUEST_CHANGES"`):

```json
{
  "verdict": "REQUEST_CHANGES",
  "summary": { "files_reviewed": 1, "auto_healed_count": 2, "blocking_issues": 1 },
  "auto_healed": [
    { "file": "stats.py", "line": 8, "rule": "RFT-118", "action": "Added stacklevel=2 to warnings.warn()" }
  ],
  "blocking_comments": [
    {
      "id": "rev-1",
      "file": "stats.py",
      "line": 18,
      "rule": "RFT-301",
      "severity": "ERROR",
      "message": "np.testing.assert_warns forwards `match=` to `calculate_average`, which does not accept `match`.",
      "type_proof": { "target_callable": "calculate_average", "accepted_params": ["values"], "unexpected_kwarg": "match" },
      "suggested_action": "Migrate to `with pytest.warns(RuntimeWarning, match=...):`."
    }
  ]
}
```

## 7. Sequential Execution Milestones (With Mandatory Test Gates)
Execute one milestone at a time. Do NOT advance until `cargo test` passes with zero warnings (`RUSTFLAGS="-D warnings"`).
 * **Milestone 1 (`riffty_core`):** Implement canonical IR, types, fix algebra, and port traits. Gate: `cargo test -p riffty_core`.
 * **Milestone 2 (`riffty_pipeline`):** Implement single-pass runner, conflict resolver, bottom-to-top byte patcher, and fixpoint loop (max 10 passes). Gate: `cargo test -p riffty_pipeline` (including overlapping edit tests).
 * **Milestone 3 (Adapters):** Implement `RuffAstAdapter`, `RuffImportAdapter`, `TyOracleAdapter` (with `NullTypeOracle` fallback), and `AstGrepAdapter`. Gate: `cargo test --test conformance`.
 * **Milestone 4 (Flagship Rules & Golden SciPy #20068 Fixture):** Implement:
   * `RFT-202` + `RFT-301`: Migrate `np.testing.assert_warns(W, fn, *args, match=...)` -> `with pytest.warns(W, match=...): fn(*args)`. Query `TypeOraclePort` on `fn`: classify as `Applicability::Safe` ONLY if `fn` does NOT accept `match` and has `has_var_kwargs == false`; otherwise classify as `Applicability::Unsafe`.
   * `RFT-118`: Inject `stacklevel=2` into `warnings.warn(...)` when omitted (`Applicability::Safe`).
   * `RFT-004`: Flag unawaited coroutine expressions (`Callable { is_async: true }` called without `await` or return/assignment).
   * `RFT-015`: Flag `x in container` where `x` type and container element type are provably disjoint.
   * **Golden Acceptance Test:** Verify `calculate_average(values)` + `np.testing.assert_warns(RuntimeWarning, calculate_average, [], match="...")` produces `Applicability::Safe` and rewrites cleanly while preserving all comments, whereas a callable with `**kwargs` downgrades to `Applicability::Unsafe`.
 * **Milestone 5 (`riffty_cli` & `riffty_mcp`):** Implement `riffty check [--fix]` and `riffty mcp` (`riffty_review_changes` with git-diff scoping and reverse-dependency expansion). Gate: End-to-end integration test on a temp git repo.

---

### Why This Version Works With Zero Prior Context
* **Zero Implicit Knowledge:** Every type name (`CanonicalNode`, `Applicability`, `TriState`), port signature (`TypeOraclePort::reverse_dependents`), rule ID (`RFT-301`, `RFT-202`, `RFT-118`, `RFT-004`, `RFT-015`), and JSON contract is explicitly defined inside the prompt.
* **Self-Verifying:** Because Milestone 4 embeds the exact SciPy #20068 conditional `Safe` vs. `Unsafe` callable-signature test as a compilation/test gate, the model cannot declare completion until the exact behavior we designed is working and verified by `cargo test`.


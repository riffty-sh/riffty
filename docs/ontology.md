# `riffty` (v2) Domain Ontology & Deterministic Policy Engine

## 1. Ubiquitous Language & Core Entities

| Entity | Layer | Definition |
| :--- | :---: | :--- |
| `ByteOffset` | Domain Core | Zero-indexed UTF-8 byte coordinate (`u32`) inside an immutable Python source string. |
| `TextRange` | Domain Core | Half-open UTF-8 byte interval `[start, end)` anchoring AST nodes, diagnostics, and surgical edits. |
| `CanonicalNode` | Domain Core | Parser-agnostic syntax node carrying `NodeId`, `NodeKind`, `TextRange`, parent link, and exact `raw_text` slice. |
| `CanonicalType` | Domain Core | Sound semantic type representation (`Unknown`, `Any`, `NoneType`, `Primitive`, `Nominal`, `Union`, `Callable` with `has_var_args`, `has_var_kwargs`, `is_async`). |
| `TriState` | Domain Core | Three-valued Kleene logic (`Yes`, `No`, `Unknown`) guaranteeing zero-false-positive type compatibility proofs. |
| `Purity` | Domain Core | Expression side-effect lattice (`Pure`, `ReadOnly`, `SideEffecting`) governing metavariable duplication safety in rewrites. |
| `Applicability` | Domain Core | Remediation safety tier (`Safe`, `Unsafe`, `ManualOnly`) controlling automatic healing under `--fix` and `auto_heal`. |
| `Fix` | Domain Core | Deterministic byte-offset patch bundling `Vec<Edit>`, structured `Vec<ImportSpec>`, optional replaced FQNs, and `Applicability`. |
| `Diagnostic` | Domain Core | Immutable defect report carrying rule ID, `Severity`, `FileId`, `TextRange`, message, structured JSON `type_proof`, and optional `Fix`. |
| `OverrideRisk` | Domain Core | Open-world polymorphic method-dispatch risk classification (`SealedEquivalent`, `PolymorphicOrOverridden`, `Unknown`) preventing unsafe base-method migrations on `self`/`cls` or overridden MRO chains. |
| `ScopedImportRequest` | Domain Core | Lexical call-site-anchored import synthesis request pairing `ImportSpec`, call `use_range`, and optional deprecated `replaces_fqname` for shadow-free local/module import remediation. |
| `ReviewVerdict` | Application | Token-compact MCP diff review payload (`APPROVED` or `REQUEST_CHANGES`) summarizing auto-healed fixes and blocking comments. |

## 2. Defect & Friction Taxonomies

| Taxonomy | Target Rules | Semantic Description |
| :--- | :---: | :--- |
| `UNRESOLVED_ASYNC_COROUTINE` | `RFT-004` | Calling a `Callable { is_async: true }` as a bare expression statement without `await`, `return`, or variable binding. |
| `DISJOINT_COLLECTION_MEMBERSHIP` | `RFT-015` | Evaluating `x in container` or `x not in container` where `x` and container element types are provably disjoint (`TriState::No` both ways) and neither defines custom `__eq__`. |
| `OMITTED_WARNING_STACKLEVEL` | `RFT-118` | Calling `warnings.warn(...)` without an explicit `stacklevel=` or Python 3.12+ `skip_file_prefixes=` keyword argument, misattributing caller frames. |
| `NARROW_SKIP_FILE_PREFIXES` | `RFT-119` | Calling `warnings.warn(..., skip_file_prefixes=(os.path.dirname(__file__),))` inside an internal helper subpackage (`_lib`, `_internal`, `_shared`), leaking sibling subpackage frames. |
| `SILENT_KWARG_FORWARDING_TRAP` | `RFT-202`, `RFT-301` | Calling `np.testing.assert_warns(W, fn, *args, match=...)` where `assert_warns` forwards `match=` into `fn` instead of matching the warning message (SciPy #20068). |
| `POLYMORPHIC_OVERRIDE_BYPASS` | `Refaster Guards` | Rewriting `self.method()` on a non-`@final` base class or MRO subclass that overrides `method`, silently bypassing user/subclass polymorphic overrides (`langchain#38054`). |
| `REFASTER_DECLARATIVE_IDIOM` | `YAML Rules` | Structural `ast-grep` patterns guarded by `TypeOraclePort` predicates (`fqname_in`, `type_assignable_to`, `purity`, `callable_accepts_kwarg`, `not_overridden_below`, `not_inside_with`). |

## 3. Canonical Terminal States

| State | Interface | Terminal Meaning |
| :--- | :---: | :--- |
| `CLEAN` | CLI / Pipeline | Zero diagnostics emitted across analyzed files or diff spans. |
| `AUTO_HEALED` | CLI `--fix` / MCP `auto_heal` | All `Applicability::Safe` byte-offset edits and `ImportSpec` insertions converged within <= 10 fixpoint passes. |
| `DIAGNOSED_UNSAFE_OR_MANUAL` | CLI / MCP | Diagnostics reported with `Applicability::Unsafe` or `Applicability::ManualOnly`, preserved untouched on disk for human/agent action. |
| `SUPPRESSED_BY_TYPE_ORACLE` | Domain Rule | Candidate finding suppressed with zero false positives because `TypeOraclePort` returned `Unknown` or `Any`. |
| `APPROVED` | MCP Reviewer | Diff + reverse-dependent scope contains zero remaining blocking issues after optional `auto_heal`. |
| `REQUEST_CHANGES` | MCP Reviewer | Diff + reverse-dependent scope contains >= 1 remaining blocking issue requiring caller or logic changes. |

## 4. Deterministic Policy Rules (R01–R22)

| Rule ID | Invariant Name | Deterministic Enforcement Policy |
| :--- | :--- | :--- |
| `R01` | Pure Domain Core | `crates/riffty_core/Cargo.toml` MUST NOT depend on `ruff_*`, `ty_*`, `ast_grep*`, or `salsa`. |
| `R02` | Byte-Offset Edits Only | Every `Fix` applies UTF-8 byte `TextRange` replacements on original source text; AST unparsing/pretty-printing is forbidden. |
| `R03` | Zero-False-Positive Soundness | Whenever `TypeOraclePort` returns `CanonicalType::Unknown` or `CanonicalType::Any`, type-dependent rules MUST suppress diagnostics. |
| `R04` | Single-Pass Node Dispatch | `riffty_pipeline` traverses `CanonicalSyntaxTree` once in $O(\text{nodes})$, dispatching nodes strictly to `RulePlugin`s subscribed to `node.kind`. |
| `R05` | Purity-Guarded Metavariable Duplication | Refaster YAML fixes MUST NOT duplicate any captured metavariable in replacement templates unless `expression_purity == Purity::Pure`. |
| `R06` | Structured Import Synthesis | `Fix::required_imports` are resolved via `ImportManagerPort::resolve_import_edits` / `resolve_scoped_import_edits` into concrete, deduplicated `Edit`s. |
| `R07` | Deterministic Overlap Resolution | Edits are sorted by `(range.start, range.end)`; overlapping fixes select the higher-severity / earlier fix and defer conflicts to the next pass. |
| `R08` | Bottom-to-Top Byte Patching | Non-overlapping `Edit`s are applied strictly from highest byte offset to lowest so earlier byte coordinates remain invariant. |
| `R09` | Bounded Fixpoint Convergence | Auto-fix loops execute at most 10 iterations and halt immediately when zero new `Applicability::Safe` fixes apply or a cycle repeats. |
| `R10` | Conditional `assert_warns` Safety (`RFT-202`/`RFT-301`) | Rewriting `np.testing.assert_warns(W, fn, *args, match=...)` to `with pytest.warns(W, match=...): fn(*args)` is `Safe` ONLY when `fn` is a `Callable` without a `match` parameter and `has_var_kwargs == false`; if `fn` accepts `match` or `**kwargs` (or is unknown), downgrade to `Unsafe`. |
| `R11` | `warnings.warn` Stacklevel Injection (`RFT-118`) | Calls resolving to `warnings.warn` without `stacklevel=`, without `skip_file_prefixes=`, and without `**kwargs` unpacking receive `stacklevel=2` as `Applicability::Safe`. |
| `R12` | Unawaited Coroutine Proof (`RFT-004`) | Bare expression-statement calls whose callee resolves to `Callable { is_async: true }` emit `RFT-004` (`Error`), ignoring `await`, `return`, or assigned calls. |
| `R13` | Disjoint Container Membership (`RFT-015`) | `x in c` / `x not in c` emits `RFT-015` ONLY when `x` and element type $T$ have completely resolved MROs (`is_subtype(X, T) == No` and `is_subtype(T, X) == No`, excluding numeric tower equivalence) AND both satisfy `has_custom_eq == No`. |
| `R14` | Diff-Scoped Baseline Suppression | `ReviewFacade` filters diagnostics to touched `TextRange`s from `git diff` plus `TypeOraclePort::reverse_dependents`, suppressing untouched legacy code. |
| `R15` | Reverse-Dependency Blast Radius | Modifying a function definition in a diff expands `ReviewFacade` scope via `TypeOraclePort::reverse_dependents` to audit callers in other workspace files. |
| `R16` | Zero-Token Compact Verdict | `riffty_review_changes` returns deterministic JSON (`APPROVED` when `blocking_issues == 0`, else `REQUEST_CHANGES`) with `auto_healed` and `blocking_comments`. |
| `R17` | Open-World Polymorphic Dispatch Safety | Method-renaming codemods (`old_m` -> `new_m` on base `B`) MUST NOT emit `Applicability::Safe` if any subclass in receiver MRO overrides `old_m`, if receiver is `self`/`cls` inside a non-`@final` class, or if inside an explicit deprecation-suppression `with` block. |
| `R18` | Open-World Subtype & Custom-Eq Conservatism | Without complete MRO proof, `eval_is_subtype` between `(Nominal, Primitive)` or distinct `(Nominal, Nominal)` MUST return `TriState::Unknown` (never `No`), and `Literal[Enum.MEMBER]` / `@property` getters MUST unwrap to their underlying types. |
| `R19` | Lexical Scope-Aware Import Remediation | When a `Fix` migrates a symbol inside a function containing a shadowing local `from <old_mod> import <sym>` (or replaces all module-level uses of `<old_mod>.<sym>`), `ImportManagerPort::resolve_scoped_import_edits` rewrites the shadowed/deprecated import in-place rather than leaving a shadowing local binding. |
| `R20` | Python 3.12+ `skip_file_prefixes` Semantics (`RFT-118`/`RFT-119`) | `RFT-118` suppresses `stacklevel=2` injection when `skip_file_prefixes=` is present; `RFT-119` flags narrow `skip_file_prefixes=(os.path.dirname(__file__),)` inside internal helper subpackages (`_lib`, `_internal`, `_shared`). |
| `R21` | First-Class Control & Class Syntax Nodes | `CanonicalSyntaxTree` emits first-class `NodeKind::{ClassDef, With, If, Try}` with structured `ClassMeta`, `WithMeta`, and `IfMeta` so rules inspect enclosing scopes without raw string slicing. |
| `R22` | PEP 562 `if TYPE_CHECKING:` & Package Root MRO | `TyOracleAdapter` indexes imports and re-exports inside `if TYPE_CHECKING:` / `if typing.TYPE_CHECKING:` blocks, resolves canonical `__init__.py` package roots in monorepos, and supports optional `--python-venv` site-packages mounting. |


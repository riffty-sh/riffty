# `riffty` Ubiquitous Language Glossary

- **Applicability**: Classification (`Safe`, `Unsafe`, `ManualOnly`) determining whether a `Fix` can be auto-applied without changing valid runtime behavior.
- **Byte-Offset Patching**: Applying `[start, end)` UTF-8 byte replacements bottom-to-top directly on the original source buffer, preserving comments and formatting untouched.
- **Canonical IR**: The minimal, parser-independent AST representation (`CanonicalSyntaxTree`, `CanonicalNode`, `NodeKind`) defined in `riffty_core`.
- **Driven Port**: A trait in `riffty_core::ports` (`SyntaxParserPort`, `TypeOraclePort`, `PatternMatcherPort`, `ImportManagerPort`, `WorkspaceVfsPort`, `PatchApplierPort`, `GitDiffPort`) implemented by an external adapter crate.
- **Fixpoint Loop**: Iterative parse-index-dispatch-resolve-patch cycle capped at 10 passes until no `Applicability::Safe` edits remain.
- **Purity Lattice**: Classification of Python expressions into `Pure`, `ReadOnly`, or `SideEffecting` to prevent duplicating impure expressions during Refaster template expansion.
- **Reverse Dependents**: Call sites and symbol usages across workspace files referencing a symbol modified within a `git diff` hunk.
- **Type Proof**: Structured JSON witness attached to a `Diagnostic` proving why a type-aware rule fired (e.g., `target_callable`, `accepted_params`, `unexpected_kwarg`).


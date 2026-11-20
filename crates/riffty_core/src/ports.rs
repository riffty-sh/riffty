use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    AnalysisProvenance, AnalysisView, CanonicalNode, CanonicalSyntaxTree, CanonicalType,
    Diagnostic, Edit, FileId, ImportSpec, Language, NodeKind, OverrideRisk, PatternMatch, Purity,
    ScopedImportRequest, TextRange, TriState, WorkspaceFile, eval_has_custom_eq_default,
};

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
    fn reverse_dependents(
        &self,
        file_id: FileId,
        changed_ranges: &[TextRange],
    ) -> Vec<(FileId, TextRange)>;

    /// Evaluates whether `ty` may define a custom `__eq__` accepting cross-hierarchy values (`R13`, `R18`).
    fn has_custom_eq(&self, ty: &CanonicalType) -> TriState {
        eval_has_custom_eq_default(ty)
    }

    /// Evaluates open-world polymorphic dispatch risk when migrating `receiver.method(...)` anchored on `anchor_base_fqn` (`R17`).
    fn method_override_risk(
        &self,
        _file_id: FileId,
        _receiver_range: TextRange,
        _method: &str,
        _anchor_base_fqn: &str,
    ) -> OverrideRisk {
        OverrideRisk::Unknown
    }

    /// Optional workspace indexing hook invoked by the pipeline before dispatching rules.
    /// Default no-op allows minimal single-file or mock `TypeOraclePort` implementations.
    fn index_workspace(&self, _files: &[WorkspaceFile]) {}
}

pub trait PatternMatcherPort: Send + Sync {
    fn find_matches(&self, source: &str, pattern: &str) -> Result<Vec<PatternMatch>, String>;
}

pub trait ImportManagerPort: Send + Sync {
    fn resolve_import_edits(&self, source: &str, imports: &[ImportSpec]) -> Vec<Edit>;

    /// Lexical scope-aware import resolution (`R19`). Defaults to module-level `resolve_import_edits`
    /// for lightweight mock implementations.
    fn resolve_scoped_import_edits(
        &self,
        tree: &CanonicalSyntaxTree,
        requests: &[ScopedImportRequest],
    ) -> Vec<Edit> {
        let mut seen = HashSet::new();
        let mut specs = Vec::new();
        for req in requests {
            if seen.insert(req.spec.clone()) {
                specs.push(req.spec.clone());
            }
        }
        self.resolve_import_edits(&tree.source, &specs)
    }
}

pub trait RulePlugin: Send + Sync {
    fn id(&self) -> &'static str;
    fn target_node_kinds(&self) -> &[NodeKind];
    fn evaluate(&self, node: &CanonicalNode, view: &AnalysisView) -> Vec<Diagnostic>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffTarget {
    Unstaged,
    Staged,
    BranchDiff { base_ref: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiffRanges {
    pub relative_path: PathBuf,
    pub touched_ranges: Vec<TextRange>,
}

pub trait WorkspaceVfsPort: Send + Sync {
    /// Generalized extension-based workspace file discovery.
    fn discover_files(
        &self,
        root: &Path,
        extensions: &[&str],
    ) -> Result<Vec<WorkspaceFile>, String>;

    /// Backwards-compatible default delegating to `discover_files`.
    fn discover_python_files(&self, root: &Path) -> Result<Vec<WorkspaceFile>, String> {
        self.discover_files(root, &["py"])
    }

    fn read_file(&self, path: &Path) -> Result<String, String>;
}

#[derive(Debug, Clone)]
pub struct AnalyzePassRequest<'a> {
    pub workspace_root: &'a Path,
    pub files: &'a [WorkspaceFile],
    /// Optional git-diff byte ranges per file; `None` => full file scope.
    pub scoped_ranges: Option<&'a HashMap<FileId, Vec<TextRange>>>,
    pub rules_paths: &'a [PathBuf],
    pub allow_unsafe: bool,
    pub pass_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzePassResponse {
    pub diagnostics: Vec<Diagnostic>,
    pub provenance: AnalysisProvenance,
}

/// Coarse-grained multi-language seam: 1 call per pass/batch, zero per-node IPC.
pub trait LanguageAnalyzerPort: Send + Sync {
    fn language(&self) -> Language;
    fn extensions(&self) -> &'static [&'static str];
    fn max_fixpoint_passes(&self) -> usize {
        10
    }

    /// Runs a single analysis pass over the provided in-memory file buffers.
    fn analyze_pass(&self, req: &AnalyzePassRequest<'_>) -> Result<AnalyzePassResponse, String>;

    /// Optional import manager (Python uses `RuffImportAdapter`; Java returns `None`
    /// because Error Prone emits explicit import `Edit`s directly inside `Fix::edits`).
    fn import_manager(&self) -> Option<&dyn ImportManagerPort> {
        None
    }

    /// Optional lexical-scope-aware import resolution for a single file.
    /// Defaults to delegating to `import_manager().resolve_import_edits(...)`.
    fn resolve_file_import_edits(
        &self,
        file: &WorkspaceFile,
        requests: &[ScopedImportRequest],
    ) -> Vec<Edit> {
        let Some(mgr) = self.import_manager() else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        let mut specs = Vec::new();
        for req in requests {
            if seen.insert(req.spec.clone()) {
                specs.push(req.spec.clone());
            }
        }
        mgr.resolve_import_edits(&file.source, &specs)
    }

    /// Optional reverse-dependent expansion for `--diff` mode.
    fn reverse_dependents(
        &self,
        _root: &Path,
        _files: &[WorkspaceFile],
        _touched: &HashMap<FileId, Vec<TextRange>>,
    ) -> Vec<(FileId, TextRange)> {
        Vec::new()
    }
}

pub trait PatchApplierPort: Send + Sync {
    fn write_patched_file(
        &self,
        workspace_root: &Path,
        target_path: &Path,
        updated_source: &str,
    ) -> Result<(), String>;
}

pub trait GitDiffPort: Send + Sync {
    fn touched_ranges(
        &self,
        repo_root: &Path,
        target: &DiffTarget,
    ) -> Result<Vec<FileDiffRanges>, String>;
}

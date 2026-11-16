use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    AnalysisView, CanonicalNode, CanonicalSyntaxTree, CanonicalType, Diagnostic, Edit, FileId,
    ImportSpec, NodeKind, OverrideRisk, PatternMatch, Purity, ScopedImportRequest, TextRange,
    TriState, WorkspaceFile, eval_has_custom_eq_default,
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
    fn discover_python_files(&self, root: &Path) -> Result<Vec<WorkspaceFile>, String>;
    fn read_file(&self, path: &Path) -> Result<String, String>;
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


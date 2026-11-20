pub mod ports;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use ports::{
    AnalyzePassRequest, AnalyzePassResponse, DiffTarget, FileDiffRanges, GitDiffPort,
    ImportManagerPort, LanguageAnalyzerPort, PatchApplierPort, PatternMatcherPort, RulePlugin,
    SyntaxParserPort, TypeOraclePort, WorkspaceVfsPort,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Python,
    Java,
    #[serde(rename = "jsts", alias = "js", alias = "ts", alias = "javascript", alias = "typescript")]
    JsTs,
}

impl Language {
    pub const JSTS_EXTENSIONS: &'static [&'static str] = &[
        "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs",
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Language::Python => "python",
            Language::Java => "java",
            Language::JsTs => "jsts",
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "py" | "pyi" => Some(Language::Python),
            "java" => Some(Language::Java),
            "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => Some(Language::JsTs),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisTier {
    /// Full semantic/compiler-verified analysis (Ty for Python, javac+ErrorProne for Java).
    Tier2Semantic,
    /// Fast syntactic-only analysis (NullTypeOracle for Python, tree-sitter-java for Java).
    Tier1Syntactic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisProvenance {
    pub language: Language,
    pub tier: AnalysisTier,
    pub files_analyzed: usize,
    /// e.g. `Some("jdk 25 (bundled jlink · aot) · 84 jars via target/classes")`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_detail: Option<String>,
    /// Present whenever analysis ran below `Tier2Semantic`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
    /// Actionable single-line recovery hint for CLI/MCP.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation_hint: Option<String>,
}

/// Lightweight O(log N) byte-offset -> (1-based line, 1-based UTF-8 col) mapper
/// avoiding redundant full-AST re-parses when formatting diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    /// Sorted byte offsets of line starts (always starts with 0).
    line_starts: Vec<u32>,
}

impl LineIndex {
    pub fn new(source: &str) -> Self {
        Self {
            line_starts: compute_line_starts(source),
        }
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// 1-indexed line number for a `ByteOffset`.
    pub fn line_of(&self, offset: ByteOffset) -> u32 {
        let idx = self.line_starts.partition_point(|&start| start <= offset.0);
        idx.max(1) as u32
    }

    /// 1-indexed UTF-8 column number for a `ByteOffset`.
    pub fn col_of(&self, source: &str, offset: ByteOffset) -> u32 {
        let line = self.line_of(offset);
        let line_start = self
            .line_starts
            .get((line - 1) as usize)
            .copied()
            .unwrap_or(0) as usize;
        let end = (offset.0 as usize).min(source.len());
        if line_start <= end
            && source.is_char_boundary(line_start)
            && source.is_char_boundary(end)
        {
            (source[line_start..end].chars().count() as u32) + 1
        } else {
            1
        }
    }

    /// Returns `(1-based line, 1-based UTF-8 column)` for `offset`.
    pub fn line_col(&self, source: &str, offset: ByteOffset) -> (u32, u32) {
        (self.line_of(offset), self.col_of(source, offset))
    }

    /// Returns the source slice for a 1-based line number (without trailing `\r`/`\n`).
    pub fn line_slice<'a>(&self, source: &'a str, line_1based: u32) -> &'a str {
        let idx = line_1based.saturating_sub(1) as usize;
        let Some(&start_u32) = self.line_starts.get(idx) else {
            return "";
        };
        let start = (start_u32 as usize).min(source.len());
        let end = self
            .line_starts
            .get(idx + 1)
            .map(|&e| (e as usize).min(source.len()))
            .unwrap_or(source.len());
        if start <= end && source.is_char_boundary(start) && source.is_char_boundary(end) {
            source[start..end].trim_end_matches(['\r', '\n'])
        } else {
            ""
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ByteOffset(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TextRange {
    pub start: ByteOffset,
    pub end: ByteOffset,
}

impl TextRange {
    pub const fn new(start: u32, end: u32) -> Self {
        Self {
            start: ByteOffset(start),
            end: ByteOffset(end),
        }
    }

    pub const fn len(&self) -> u32 {
        self.end.0.saturating_sub(self.start.0)
    }

    pub const fn is_empty(&self) -> bool {
        self.start.0 >= self.end.0
    }

    pub const fn contains_offset(&self, offset: ByteOffset) -> bool {
        self.start.0 <= offset.0 && offset.0 < self.end.0
    }

    pub const fn contains_range(&self, other: TextRange) -> bool {
        self.start.0 <= other.start.0 && other.end.0 <= self.end.0
    }

    pub const fn intersects(&self, other: TextRange) -> bool {
        if self.is_empty() && other.is_empty() {
            return self.start.0 == other.start.0;
        }
        if self.is_empty() {
            return other.start.0 <= self.start.0 && self.start.0 <= other.end.0;
        }
        if other.is_empty() {
            return self.start.0 <= other.start.0 && other.start.0 <= self.end.0;
        }
        self.start.0 < other.end.0 && other.start.0 < self.end.0
    }

    pub fn slice<'a>(&self, source: &'a str) -> Option<&'a str> {
        let s = self.start.0 as usize;
        let e = self.end.0 as usize;
        if s <= e && e <= source.len() && source.is_char_boundary(s) && source.is_char_boundary(e) {
            Some(&source[s..e])
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeKind {
    Module,
    ClassDef,
    FunctionDef,
    AsyncFunctionDef,
    With,
    If,
    Try,
    Call,
    Compare,
    Attribute,
    Name,
    Subscript,
    Return,
    Await,
    Import,
    ImportFrom,
    Other,
}

impl NodeKind {
    pub const COUNT: usize = 17;

    pub const ALL: [NodeKind; Self::COUNT] = [
        NodeKind::Module,
        NodeKind::ClassDef,
        NodeKind::FunctionDef,
        NodeKind::AsyncFunctionDef,
        NodeKind::With,
        NodeKind::If,
        NodeKind::Try,
        NodeKind::Call,
        NodeKind::Compare,
        NodeKind::Attribute,
        NodeKind::Name,
        NodeKind::Subscript,
        NodeKind::Return,
        NodeKind::Await,
        NodeKind::Import,
        NodeKind::ImportFrom,
        NodeKind::Other,
    ];

    pub const fn as_index(self) -> usize {
        match self {
            NodeKind::Module => 0,
            NodeKind::ClassDef => 1,
            NodeKind::FunctionDef => 2,
            NodeKind::AsyncFunctionDef => 3,
            NodeKind::With => 4,
            NodeKind::If => 5,
            NodeKind::Try => 6,
            NodeKind::Call => 7,
            NodeKind::Compare => 8,
            NodeKind::Attribute => 9,
            NodeKind::Name => 10,
            NodeKind::Subscript => 11,
            NodeKind::Return => 12,
            NodeKind::Await => 13,
            NodeKind::Import => 14,
            NodeKind::ImportFrom => 15,
            NodeKind::Other => 16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalNode<'a> {
    pub id: NodeId,
    pub kind: NodeKind,
    pub range: TextRange,
    pub parent: Option<NodeId>,
    pub raw_text: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeywordArgMeta {
    /// `None` represents `**kwargs` dictionary unpacking.
    pub name: Option<String>,
    pub name_range: Option<TextRange>,
    pub value_range: TextRange,
    pub full_range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallMeta {
    pub callee_range: TextRange,
    pub positional_args: Vec<TextRange>,
    pub starred_args: Vec<TextRange>,
    pub keyword_args: Vec<KeywordArgMeta>,
    /// True when this `Call` is the direct expression of a standalone `Stmt::Expr` statement.
    pub is_expr_stmt: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareOp {
    In,
    NotIn,
    Is,
    IsNot,
    Eq,
    NotEq,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareMeta {
    pub left_range: TextRange,
    pub comparators: Vec<(CompareOp, TextRange)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionMeta {
    pub name: String,
    pub name_range: TextRange,
    pub is_async: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassMeta {
    pub name: String,
    pub name_range: TextRange,
    pub bases: Vec<TextRange>,
    pub decorators: Vec<TextRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithItemMeta {
    pub context_expr_range: TextRange,
    pub optional_vars_range: Option<TextRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithMeta {
    pub is_async: bool,
    pub items: Vec<WithItemMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IfMeta {
    pub test_range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NodeMeta {
    #[default]
    None,
    Call(CallMeta),
    Compare(CompareMeta),
    Function(FunctionMeta),
    Class(ClassMeta),
    With(WithMeta),
    If(IfMeta),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredNode {
    pub id: NodeId,
    pub kind: NodeKind,
    pub range: TextRange,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub meta: NodeMeta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineSuppression {
    pub line: u32,
    /// Empty list means blanket `# riffty: ignore`.
    pub rule_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalSyntaxTree {
    pub file_id: FileId,
    pub source: String,
    nodes: Vec<StoredNode>,
    suppressions: Vec<InlineSuppression>,
    line_starts: Vec<u32>,
}

impl CanonicalSyntaxTree {
    pub fn new(
        file_id: FileId,
        source: String,
        nodes: Vec<StoredNode>,
        suppressions: Vec<InlineSuppression>,
    ) -> Self {
        let line_starts = compute_line_starts(&source);
        Self {
            file_id,
            source,
            nodes,
            suppressions,
            line_starts,
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node<'a>(&'a self, id: NodeId) -> Option<CanonicalNode<'a>> {
        let stored = self.nodes.get(id.0 as usize)?;
        let raw_text = stored.range.slice(&self.source).unwrap_or("");
        Some(CanonicalNode {
            id: stored.id,
            kind: stored.kind,
            range: stored.range,
            parent: stored.parent,
            raw_text,
        })
    }

    pub fn iter_nodes<'a>(&'a self) -> impl Iterator<Item = CanonicalNode<'a>> + 'a {
        self.nodes.iter().map(move |stored| CanonicalNode {
            id: stored.id,
            kind: stored.kind,
            range: stored.range,
            parent: stored.parent,
            raw_text: stored.range.slice(&self.source).unwrap_or(""),
        })
    }

    pub fn node_meta(&self, id: NodeId) -> Option<&NodeMeta> {
        self.nodes.get(id.0 as usize).map(|n| &n.meta)
    }

    pub fn children_of(&self, id: NodeId) -> &[NodeId] {
        self.nodes
            .get(id.0 as usize)
            .map(|n| n.children.as_slice())
            .unwrap_or(&[])
    }

    /// 1-indexed line number for a `ByteOffset`.
    pub fn line_of(&self, offset: ByteOffset) -> u32 {
        match self.line_starts.binary_search(&offset.0) {
            Ok(idx) => (idx as u32) + 1,
            Err(idx) => idx.max(1) as u32,
        }
    }

    /// 1-indexed UTF-8 column number for a `ByteOffset`.
    pub fn col_of(&self, offset: ByteOffset) -> u32 {
        let line = self.line_of(offset);
        let line_start = self
            .line_starts
            .get((line - 1) as usize)
            .copied()
            .unwrap_or(0) as usize;
        let end = (offset.0 as usize).min(self.source.len());
        if line_start <= end
            && self.source.is_char_boundary(line_start)
            && self.source.is_char_boundary(end)
        {
            (self.source[line_start..end].chars().count() as u32) + 1
        } else {
            1
        }
    }

    pub fn line_indent_at(&self, offset: ByteOffset) -> &str {
        let line = self.line_of(offset);
        let start = self
            .line_starts
            .get((line - 1) as usize)
            .copied()
            .unwrap_or(0) as usize;
        let rest = &self.source[start.min(self.source.len())..];
        let indent_len = rest
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count();
        &rest[..indent_len]
    }

    pub fn is_suppressed(&self, offset: ByteOffset, rule_id: &str) -> bool {
        let line = self.line_of(offset);
        self.suppressions.iter().any(|sup| {
            sup.line == line
                && (sup.rule_ids.is_empty()
                    || sup.rule_ids.iter().any(|r| r.eq_ignore_ascii_case(rule_id)))
        })
    }
}

pub fn compute_line_starts(source: &str) -> Vec<u32> {
    let mut starts = vec![0];
    for (idx, b) in source.bytes().enumerate() {
        if b == b'\n' {
            starts.push((idx + 1) as u32);
        }
    }
    starts
}

pub fn extract_inline_suppressions(source: &str) -> Vec<InlineSuppression> {
    let mut out = Vec::new();
    for (idx, line) in source.lines().enumerate() {
        let Some(hash_pos) = line.find('#') else {
            continue;
        };
        let comment = line[hash_pos + 1..].trim();
        let Some(rest) = comment.strip_prefix("riffty:") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(after_ignore) = rest.strip_prefix("ignore") else {
            continue;
        };
        let after_ignore = after_ignore.trim_start();
        if let Some(bracketed) = after_ignore.strip_prefix('[') {
            if let Some(end_bracket) = bracketed.find(']') {
                let ids: Vec<String> = bracketed[..end_bracket]
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                out.push(InlineSuppression {
                    line: (idx + 1) as u32,
                    rule_ids: ids,
                });
            }
        } else {
            out.push(InlineSuppression {
                line: (idx + 1) as u32,
                rule_ids: Vec::new(),
            });
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PrimitiveKind {
    Int,
    Str,
    Float,
    Bool,
    Bytes,
}

impl PrimitiveKind {
    pub const fn fqname(self) -> &'static str {
        match self {
            PrimitiveKind::Int => "builtins.int",
            PrimitiveKind::Str => "builtins.str",
            PrimitiveKind::Float => "builtins.float",
            PrimitiveKind::Bool => "builtins.bool",
            PrimitiveKind::Bytes => "builtins.bytes",
        }
    }

    pub const fn is_numeric_tower(self) -> bool {
        matches!(
            self,
            PrimitiveKind::Bool | PrimitiveKind::Int | PrimitiveKind::Float
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanonicalType {
    Unknown,
    Any,
    NoneType,
    Primitive(PrimitiveKind), // Int, Str, Float, Bool, Bytes
    Nominal {
        fqname: String,
        args: Vec<CanonicalType>,
    },
    /// Arbitrary-length homogeneous tuple `tuple[T, ...]`, distinct from fixed-arity `tuple[T]`.
    VariadicTuple(Box<CanonicalType>),
    Union(Vec<CanonicalType>),
    Callable {
        params: Vec<ParamInfo>,
        has_var_args: bool,
        has_var_kwargs: bool,
        return_type: Box<CanonicalType>,
        is_async: bool,
    },
}

impl CanonicalType {
    pub fn is_unknown_or_any(&self) -> bool {
        matches!(self, CanonicalType::Unknown | CanonicalType::Any)
    }

    pub fn includes_none(&self) -> bool {
        match self {
            CanonicalType::NoneType => true,
            CanonicalType::Union(items) => items.iter().any(CanonicalType::includes_none),
            _ => false,
        }
    }

    pub fn strip_none(&self) -> CanonicalType {
        match self {
            CanonicalType::NoneType => CanonicalType::Unknown,
            CanonicalType::Union(items) => {
                let mut filtered: Vec<CanonicalType> = items
                    .iter()
                    .filter(|t| !matches!(t, CanonicalType::NoneType))
                    .cloned()
                    .collect();
                match filtered.len() {
                    0 => CanonicalType::Unknown,
                    1 => filtered.pop().unwrap_or(CanonicalType::Unknown),
                    _ => CanonicalType::Union(filtered),
                }
            }
            other => other.clone(),
        }
    }

    /// Extracts the membership/iteration element type `T` tested by `x in container` or `for x in container`.
    pub fn container_element_type(&self) -> Option<CanonicalType> {
        match self {
            CanonicalType::Primitive(PrimitiveKind::Str) => {
                Some(CanonicalType::Primitive(PrimitiveKind::Str))
            }
            CanonicalType::VariadicTuple(elem) => Some((**elem).clone()),
            CanonicalType::Nominal { fqname, args } => match canonical_nominal_key(fqname) {
                "set"
                | "frozenset"
                | "list"
                | "collections.abc.Sequence"
                | "collections.abc.MutableSequence"
                | "collections.abc.Set"
                | "collections.abc.MutableSet"
                | "collections.abc.Collection"
                | "collections.abc.Iterable"
                | "collections.abc.Reversible"
                | "dict"
                | "collections.abc.Mapping"
                | "collections.abc.MutableMapping" => args.first().cloned(),
                "tuple" => {
                    if args.is_empty() {
                        None
                    } else if args.len() == 1 {
                        args.first().cloned()
                    } else {
                        let mut unique: Vec<CanonicalType> = Vec::new();
                        for a in args {
                            if !unique.contains(a) {
                                unique.push(a.clone());
                            }
                        }
                        if unique.len() == 1 {
                            unique.pop()
                        } else {
                            Some(CanonicalType::Union(unique))
                        }
                    }
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Extracts the value type `V` produced by subscripting `container[key]` (`d[k]` -> `V` on `dict[K, V]`).
    pub fn subscript_value_type(&self) -> Option<CanonicalType> {
        match self {
            CanonicalType::Nominal { fqname, args }
                if matches!(
                    canonical_nominal_key(fqname),
                    "dict" | "collections.abc.Mapping" | "collections.abc.MutableMapping"
                ) =>
            {
                args.get(1).cloned()
            }
            _ => self.container_element_type(),
        }
    }

    pub fn display_short(&self) -> String {
        match self {
            CanonicalType::Unknown => "Unknown".to_string(),
            CanonicalType::Any => "Any".to_string(),
            CanonicalType::NoneType => "None".to_string(),
            CanonicalType::Primitive(PrimitiveKind::Int) => "int".to_string(),
            CanonicalType::Primitive(PrimitiveKind::Str) => "str".to_string(),
            CanonicalType::Primitive(PrimitiveKind::Float) => "float".to_string(),
            CanonicalType::Primitive(PrimitiveKind::Bool) => "bool".to_string(),
            CanonicalType::Primitive(PrimitiveKind::Bytes) => "bytes".to_string(),
            CanonicalType::VariadicTuple(elem) => {
                format!("tuple[{}, ...]", elem.display_short())
            }
            CanonicalType::Nominal { fqname, args } => {
                let short = fqname.strip_prefix("builtins.").unwrap_or(fqname);
                if args.is_empty() {
                    short.to_string()
                } else {
                    let inner: Vec<String> =
                        args.iter().map(CanonicalType::display_short).collect();
                    format!("{short}[{}]", inner.join(", "))
                }
            }
            CanonicalType::Union(items) => items
                .iter()
                .map(CanonicalType::display_short)
                .collect::<Vec<_>>()
                .join(" | "),
            CanonicalType::Callable { is_async, .. } => {
                if *is_async {
                    "async Callable".to_string()
                } else {
                    "Callable".to_string()
                }
            }
        }
    }

    /// Pure parser for Python static type expressions (e.g. `"tuple[str, ...]"`, `"list[tuple[str, ...]]"`,
    /// `"Sequence[str]"`, `"int | str"`, `"Optional[int]"`), preserving `R01` zero-dependency purity.
    pub fn parse_type_expr(raw: &str) -> CanonicalType {
        let trimmed = strip_balanced_outer_parens(raw);
        if trimmed.is_empty() || trimmed == "Unknown" {
            return CanonicalType::Unknown;
        }
        if matches!(trimmed, "Any" | "typing.Any" | "typing_extensions.Any") {
            return CanonicalType::Any;
        }
        if matches!(trimmed, "None" | "NoneType" | "types.NoneType") {
            return CanonicalType::NoneType;
        }
        if let Some(parts) = split_top_level_type_pipe(trimmed) {
            let parsed_parts: Vec<CanonicalType> = parts
                .into_iter()
                .map(CanonicalType::parse_type_expr)
                .collect();
            return CanonicalType::Union(parsed_parts);
        }
        if let Some(bracket_idx) = trimmed.find('[')
            && trimmed.ends_with(']')
        {
            let head = trimmed[..bracket_idx].trim();
            let inner = &trimmed[bracket_idx + 1..trimmed.len() - 1];
            let raw_args = split_top_level_type_commas(inner);
            if is_tuple_type_head(head) {
                if raw_args.len() == 2 && raw_args[1] == "..." {
                    return CanonicalType::VariadicTuple(Box::new(CanonicalType::parse_type_expr(
                        raw_args[0],
                    )));
                }
                if raw_args.is_empty()
                    || (raw_args.len() == 1 && matches!(raw_args[0], "()" | "..."))
                {
                    return CanonicalType::Nominal {
                        fqname: "builtins.tuple".to_string(),
                        args: Vec::new(),
                    };
                }
            }
            let args = raw_args
                .into_iter()
                .filter(|seg| *seg != "...")
                .map(CanonicalType::parse_type_expr)
                .collect();
            return normalize_subscript_canonical_type(head, args);
        }

        parse_bare_nominal_or_primitive(trimmed)
    }
}

pub fn is_tuple_type_head(head: &str) -> bool {
    matches!(
        head,
        "tuple" | "builtins.tuple" | "Tuple" | "typing.Tuple" | "typing_extensions.Tuple"
    )
}

pub fn normalize_subscript_canonical_type(head: &str, args: Vec<CanonicalType>) -> CanonicalType {
    match head {
        "Optional" | "typing.Optional" | "typing_extensions.Optional" => {
            let inner = args.into_iter().next().unwrap_or(CanonicalType::Unknown);
            CanonicalType::Union(vec![inner, CanonicalType::NoneType])
        }
        "Union" | "typing.Union" | "typing_extensions.Union" => CanonicalType::Union(args),
        "type" | "builtins.type" | "Type" | "typing.Type" => CanonicalType::Nominal {
            fqname: "builtins.type".to_string(),
            args,
        },
        "set" | "builtins.set" | "Set" | "typing.Set" => CanonicalType::Nominal {
            fqname: "builtins.set".to_string(),
            args,
        },
        "frozenset" | "builtins.frozenset" | "FrozenSet" | "typing.FrozenSet" => {
            CanonicalType::Nominal {
                fqname: "builtins.frozenset".to_string(),
                args,
            }
        }
        "list" | "builtins.list" | "List" | "typing.List" => CanonicalType::Nominal {
            fqname: "builtins.list".to_string(),
            args,
        },
        "dict" | "builtins.dict" | "Dict" | "typing.Dict" => CanonicalType::Nominal {
            fqname: "builtins.dict".to_string(),
            args,
        },
        "tuple" | "builtins.tuple" | "Tuple" | "typing.Tuple" | "typing_extensions.Tuple" => {
            CanonicalType::Nominal {
                fqname: "builtins.tuple".to_string(),
                args,
            }
        }
        "Sequence"
        | "builtins.Sequence"
        | "typing.Sequence"
        | "collections.abc.Sequence"
        | "typing_extensions.Sequence" => CanonicalType::Nominal {
            fqname: "builtins.Sequence".to_string(),
            args,
        },
        "MutableSequence"
        | "builtins.MutableSequence"
        | "typing.MutableSequence"
        | "collections.abc.MutableSequence" => CanonicalType::Nominal {
            fqname: "collections.abc.MutableSequence".to_string(),
            args,
        },
        "Collection"
        | "builtins.Collection"
        | "typing.Collection"
        | "collections.abc.Collection" => CanonicalType::Nominal {
            fqname: "collections.abc.Collection".to_string(),
            args,
        },
        "Iterable" | "builtins.Iterable" | "typing.Iterable" | "collections.abc.Iterable" => {
            CanonicalType::Nominal {
                fqname: "collections.abc.Iterable".to_string(),
                args,
            }
        }
        "Reversible" | "typing.Reversible" | "collections.abc.Reversible" => {
            CanonicalType::Nominal {
                fqname: "collections.abc.Reversible".to_string(),
                args,
            }
        }
        "AbstractSet" | "typing.AbstractSet" | "collections.abc.Set" => CanonicalType::Nominal {
            fqname: "collections.abc.Set".to_string(),
            args,
        },
        "MutableSet" | "typing.MutableSet" | "collections.abc.MutableSet" => {
            CanonicalType::Nominal {
                fqname: "collections.abc.MutableSet".to_string(),
                args,
            }
        }
        "Mapping" | "typing.Mapping" | "collections.abc.Mapping" => CanonicalType::Nominal {
            fqname: "collections.abc.Mapping".to_string(),
            args,
        },
        "MutableMapping" | "typing.MutableMapping" | "collections.abc.MutableMapping" => {
            CanonicalType::Nominal {
                fqname: "collections.abc.MutableMapping".to_string(),
                args,
            }
        }
        other => CanonicalType::Nominal {
            fqname: other.to_string(),
            args,
        },
    }
}

pub fn parse_bare_nominal_or_primitive(trimmed: &str) -> CanonicalType {
    match trimmed {
        "int" | "builtins.int" => CanonicalType::Primitive(PrimitiveKind::Int),
        "str"
        | "builtins.str"
        | "LiteralString"
        | "typing.LiteralString"
        | "typing_extensions.LiteralString" => CanonicalType::Primitive(PrimitiveKind::Str),
        "float" | "builtins.float" => CanonicalType::Primitive(PrimitiveKind::Float),
        "bool" | "builtins.bool" => CanonicalType::Primitive(PrimitiveKind::Bool),
        "bytes" | "builtins.bytes" => CanonicalType::Primitive(PrimitiveKind::Bytes),
        "type" | "builtins.type" | "Type" | "typing.Type" => CanonicalType::Nominal {
            fqname: "builtins.type".to_string(),
            args: Vec::new(),
        },
        "set" | "builtins.set" | "Set" | "typing.Set" => CanonicalType::Nominal {
            fqname: "builtins.set".to_string(),
            args: Vec::new(),
        },
        "frozenset" | "builtins.frozenset" | "FrozenSet" | "typing.FrozenSet" => {
            CanonicalType::Nominal {
                fqname: "builtins.frozenset".to_string(),
                args: Vec::new(),
            }
        }
        "list" | "builtins.list" | "List" | "typing.List" => CanonicalType::Nominal {
            fqname: "builtins.list".to_string(),
            args: Vec::new(),
        },
        "dict" | "builtins.dict" | "Dict" | "typing.Dict" => CanonicalType::Nominal {
            fqname: "builtins.dict".to_string(),
            args: Vec::new(),
        },
        "tuple" | "builtins.tuple" | "Tuple" | "typing.Tuple" | "typing_extensions.Tuple" => {
            CanonicalType::Nominal {
                fqname: "builtins.tuple".to_string(),
                args: Vec::new(),
            }
        }
        "Sequence"
        | "builtins.Sequence"
        | "typing.Sequence"
        | "collections.abc.Sequence"
        | "typing_extensions.Sequence" => CanonicalType::Nominal {
            fqname: "builtins.Sequence".to_string(),
            args: Vec::new(),
        },
        "MutableSequence"
        | "builtins.MutableSequence"
        | "typing.MutableSequence"
        | "collections.abc.MutableSequence" => CanonicalType::Nominal {
            fqname: "collections.abc.MutableSequence".to_string(),
            args: Vec::new(),
        },
        "Collection"
        | "builtins.Collection"
        | "typing.Collection"
        | "collections.abc.Collection" => CanonicalType::Nominal {
            fqname: "collections.abc.Collection".to_string(),
            args: Vec::new(),
        },
        "Iterable" | "builtins.Iterable" | "typing.Iterable" | "collections.abc.Iterable" => {
            CanonicalType::Nominal {
                fqname: "collections.abc.Iterable".to_string(),
                args: Vec::new(),
            }
        }
        "Reversible" | "typing.Reversible" | "collections.abc.Reversible" => {
            CanonicalType::Nominal {
                fqname: "collections.abc.Reversible".to_string(),
                args: Vec::new(),
            }
        }
        "AbstractSet" | "typing.AbstractSet" | "collections.abc.Set" => CanonicalType::Nominal {
            fqname: "collections.abc.Set".to_string(),
            args: Vec::new(),
        },
        "MutableSet" | "typing.MutableSet" | "collections.abc.MutableSet" => {
            CanonicalType::Nominal {
                fqname: "collections.abc.MutableSet".to_string(),
                args: Vec::new(),
            }
        }
        "Mapping" | "typing.Mapping" | "collections.abc.Mapping" => CanonicalType::Nominal {
            fqname: "collections.abc.Mapping".to_string(),
            args: Vec::new(),
        },
        "MutableMapping" | "typing.MutableMapping" | "collections.abc.MutableMapping" => {
            CanonicalType::Nominal {
                fqname: "collections.abc.MutableMapping".to_string(),
                args: Vec::new(),
            }
        }
        "UUID" | "uuid.UUID" => CanonicalType::Nominal {
            fqname: "uuid.UUID".to_string(),
            args: Vec::new(),
        },
        other => CanonicalType::Nominal {
            fqname: other.to_string(),
            args: Vec::new(),
        },
    }
}

pub fn strip_balanced_outer_parens(raw: &str) -> &str {
    let mut cur = raw.trim();
    while cur.len() > 2 && cur.starts_with('(') && cur.ends_with(')') {
        let bytes = cur.as_bytes();
        let mut depth = 0i32;
        let mut in_quote: Option<u8> = None;
        let mut outer_wraps_all = true;
        let mut idx = 0usize;
        while idx < bytes.len() {
            let b = bytes[idx];
            if let Some(q) = in_quote {
                if b == b'\\' {
                    idx += 2;
                    continue;
                }
                if b == q {
                    in_quote = None;
                }
                idx += 1;
                continue;
            }
            match b {
                b'\'' | b'"' => in_quote = Some(b),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 && idx + 1 < bytes.len() {
                        outer_wraps_all = false;
                        break;
                    }
                }
                _ => {}
            }
            idx += 1;
        }
        if outer_wraps_all && depth == 0 {
            cur = cur[1..cur.len() - 1].trim();
        } else {
            break;
        }
    }
    cur
}

pub fn split_top_level_type_pipe(s: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_quote: Option<u8> = None;
    let bytes = s.as_bytes();
    let mut last = 0usize;
    let mut idx = 0usize;
    while idx < bytes.len() {
        let b = bytes[idx];
        if let Some(q) = in_quote {
            if b == b'\\' {
                idx += 2;
                continue;
            }
            if b == q {
                in_quote = None;
            }
            idx += 1;
            continue;
        }
        match b {
            b'\'' | b'"' => in_quote = Some(b),
            b'[' | b'(' | b'{' => depth += 1,
            b']' | b')' | b'}' => depth -= 1,
            b'|' if depth == 0 => {
                parts.push(s[last..idx].trim());
                last = idx + 1;
            }
            _ => {}
        }
        idx += 1;
    }
    if parts.is_empty() {
        None
    } else {
        parts.push(s[last..].trim());
        Some(parts)
    }
}

/// Splits top-level comma-separated type arguments while preserving `"..."` (`Ellipsis`) tokens.
pub fn split_top_level_type_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_quote: Option<u8> = None;
    let bytes = s.as_bytes();
    let mut last = 0usize;
    let mut idx = 0usize;
    while idx < bytes.len() {
        let b = bytes[idx];
        if let Some(q) = in_quote {
            if b == b'\\' {
                idx += 2;
                continue;
            }
            if b == q {
                in_quote = None;
            }
            idx += 1;
            continue;
        }
        match b {
            b'\'' | b'"' => in_quote = Some(b),
            b'[' | b'(' | b'{' => depth += 1,
            b']' | b')' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                let seg = s[last..idx].trim();
                if !seg.is_empty() {
                    parts.push(seg);
                }
                last = idx + 1;
            }
            _ => {}
        }
        idx += 1;
    }
    let tail = s[last..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamInfo {
    pub name: String,
    pub param_type: CanonicalType,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TriState {
    Yes,
    No,
    Unknown,
}

impl TriState {
    pub const fn and(self, other: TriState) -> TriState {
        match (self, other) {
            (TriState::No, _) | (_, TriState::No) => TriState::No,
            (TriState::Unknown, _) | (_, TriState::Unknown) => TriState::Unknown,
            (TriState::Yes, TriState::Yes) => TriState::Yes,
        }
    }
}

/// Deterministic Kleene 3-valued subtype evaluator enforcing `R03` (Zero-False-Positive Soundness).
pub fn eval_is_subtype(sub: &CanonicalType, sup: &CanonicalType) -> TriState {
    eval_is_subtype_with(sub, sup, &mut |_, _| TriState::Unknown)
}

/// Parameterized Kleene 3-valued subtype evaluator sharing exact Union, tuple (fixed & variadic),
/// and covariant stdlib ABC (`Sequence`/`Collection`/`Iterable`/`Mapping`/`AbstractSet`) rules across
/// pure `riffty_core` and MRO-backed `TypeOraclePort` adapters.
pub fn eval_is_subtype_with<F>(
    sub: &CanonicalType,
    sup: &CanonicalType,
    leaf_oracle: &mut F,
) -> TriState
where
    F: FnMut(&CanonicalType, &CanonicalType) -> TriState,
{
    if sub.is_unknown_or_any() || sup.is_unknown_or_any() {
        return TriState::Unknown;
    }
    if sub == sup {
        return TriState::Yes;
    }
    match (sub, sup) {
        (CanonicalType::Union(sub_members), _) => {
            if sub_members.is_empty() {
                return TriState::Unknown;
            }
            let mut acc = TriState::Yes;
            for m in sub_members {
                acc = acc.and(eval_is_subtype_with(m, sup, leaf_oracle));
                if acc == TriState::No {
                    return TriState::No;
                }
            }
            acc
        }
        (_, CanonicalType::Union(sup_members)) => {
            if sup_members.is_empty() {
                return TriState::Unknown;
            }
            let mut saw_unknown = false;
            for m in sup_members {
                match eval_is_subtype_with(sub, m, leaf_oracle) {
                    TriState::Yes => return TriState::Yes,
                    TriState::Unknown => saw_unknown = true,
                    TriState::No => {}
                }
            }
            if saw_unknown {
                TriState::Unknown
            } else {
                TriState::No
            }
        }
        (_, CanonicalType::Nominal { fqname, args })
            if (fqname == "builtins.object" || fqname == "object") && args.is_empty() =>
        {
            TriState::Yes
        }
        // PEP 484 / Python runtime subclassing: bool <: int, and int <: float in PEP 484 type tower
        (
            CanonicalType::Primitive(PrimitiveKind::Bool),
            CanonicalType::Primitive(PrimitiveKind::Int),
        )
        | (
            CanonicalType::Primitive(PrimitiveKind::Bool),
            CanonicalType::Primitive(PrimitiveKind::Float),
        )
        | (
            CanonicalType::Primitive(PrimitiveKind::Int),
            CanonicalType::Primitive(PrimitiveKind::Float),
        ) => TriState::Yes,
        (CanonicalType::Primitive(_), CanonicalType::Primitive(_)) => TriState::No,
        (CanonicalType::NoneType, _) | (_, CanonicalType::NoneType) => TriState::No,
        // Variadic homogeneous tuple `tuple[T, ...]` subtyping
        (CanonicalType::VariadicTuple(sub_elem), CanonicalType::VariadicTuple(sup_elem)) => {
            eval_is_subtype_with(sub_elem, sup_elem, leaf_oracle)
        }
        (
            CanonicalType::Nominal {
                fqname: sub_fq,
                args: sub_args,
            },
            CanonicalType::VariadicTuple(sup_elem),
        ) => {
            let sub_key = canonical_nominal_key(sub_fq);
            if sub_key == "tuple" {
                if sub_args.is_empty() {
                    if matches!(
                        sup_elem.as_ref(),
                        CanonicalType::Nominal { fqname, args }
                            if (fqname == "builtins.object" || fqname == "object") && args.is_empty()
                    ) {
                        TriState::Yes
                    } else {
                        TriState::Unknown
                    }
                } else {
                    eval_all_subtypes(sub_args, sup_elem, leaf_oracle)
                }
            } else if is_builtin_closed_container_key(sub_key)
                || is_stdlib_collection_abc_key(sub_key)
            {
                TriState::No
            } else {
                leaf_oracle(sub, sup)
            }
        }
        (
            CanonicalType::VariadicTuple(sub_elem),
            CanonicalType::Nominal {
                fqname: sup_fq,
                args: sup_args,
            },
        ) => {
            let sup_key = canonical_nominal_key(sup_fq);
            match sup_key {
                "tuple" => {
                    if sup_args.is_empty() {
                        TriState::Yes
                    } else if sub_elem.is_unknown_or_any() {
                        TriState::Unknown
                    } else {
                        TriState::No
                    }
                }
                "collections.abc.Sequence"
                | "collections.abc.Reversible"
                | "collections.abc.Collection"
                | "collections.abc.Iterable" => {
                    eval_single_arg_covariant_super(sub_elem, sup_args, leaf_oracle)
                }
                _ if is_builtin_closed_container_key(sup_key)
                    || is_stdlib_collection_abc_key(sup_key) =>
                {
                    TriState::No
                }
                _ => leaf_oracle(sub, sup),
            }
        }
        (CanonicalType::VariadicTuple(_), CanonicalType::Primitive(_))
        | (CanonicalType::Primitive(_), CanonicalType::VariadicTuple(_))
        | (CanonicalType::VariadicTuple(_), CanonicalType::Callable { .. })
        | (CanonicalType::Callable { .. }, CanonicalType::VariadicTuple(_)) => TriState::No,
        (CanonicalType::Primitive(p), CanonicalType::Nominal { fqname, args }) => {
            if p.fqname() == fqname && args.is_empty() {
                return TriState::Yes;
            }
            let sup_key = canonical_nominal_key(fqname);
            if matches!(
                sup_key,
                "collections.abc.Sequence"
                    | "collections.abc.Reversible"
                    | "collections.abc.Collection"
                    | "collections.abc.Iterable"
            ) {
                match p {
                    PrimitiveKind::Str => {
                        let elem = CanonicalType::Primitive(PrimitiveKind::Str);
                        return eval_single_arg_covariant_super(&elem, args, leaf_oracle);
                    }
                    PrimitiveKind::Bytes => {
                        let elem = CanonicalType::Primitive(PrimitiveKind::Int);
                        return eval_single_arg_covariant_super(&elem, args, leaf_oracle);
                    }
                    _ => return TriState::No,
                }
            }
            if is_builtin_closed_container_key(sup_key) || is_stdlib_collection_abc_key(sup_key) {
                TriState::No
            } else {
                // Without MRO knowledge, `fqname` could be an ABC/Protocol/supertype (`R18`)
                leaf_oracle(sub, sup)
            }
        }
        (CanonicalType::Nominal { fqname, args }, CanonicalType::Primitive(p)) => {
            if p.fqname() == fqname && args.is_empty() {
                return TriState::Yes;
            }
            let sub_key = canonical_nominal_key(fqname);
            if is_builtin_closed_container_key(sub_key) || is_stdlib_collection_abc_key(sub_key) {
                TriState::No
            } else {
                // Without MRO knowledge, `fqname` could subclass `p` (e.g. `class Rule(str, Enum)`) (`R18`)
                leaf_oracle(sub, sup)
            }
        }
        (
            CanonicalType::Nominal {
                fqname: sub_fq,
                args: sub_args,
            },
            CanonicalType::Nominal {
                fqname: sup_fq,
                args: sup_args,
            },
        ) => {
            let sub_key = canonical_nominal_key(sub_fq);
            let sup_key = canonical_nominal_key(sup_fq);
            if sub_key == sup_key {
                if sup_args.is_empty() {
                    return TriState::Yes;
                }
                if sub_args.is_empty() {
                    return TriState::Unknown;
                }
                if sub_args.len() != sup_args.len() {
                    return TriState::No;
                }
                let mut acc = TriState::Yes;
                for (a, b) in sub_args.iter().zip(sup_args.iter()) {
                    acc = acc.and(eval_is_subtype_with(a, b, leaf_oracle));
                    if acc == TriState::No {
                        return TriState::No;
                    }
                }
                return acc;
            }

            if let Some(res) =
                eval_stdlib_container_hierarchy(sub_key, sub_args, sup_key, sup_args, leaf_oracle)
            {
                return res;
            }

            // Distinct nominals without MRO context may be in an inheritance relation (`R18`)
            leaf_oracle(sub, sup)
        }
        (CanonicalType::Nominal { fqname, .. }, CanonicalType::Callable { .. }) => {
            let sub_key = canonical_nominal_key(fqname);
            if is_builtin_closed_container_key(sub_key) && sub_key != "type" {
                TriState::No
            } else {
                // A class (or `builtins.type`) may implement `__call__`
                TriState::Unknown
            }
        }
        _ => TriState::No,
    }
}

fn eval_all_subtypes<F>(
    items: &[CanonicalType],
    target: &CanonicalType,
    leaf_oracle: &mut F,
) -> TriState
where
    F: FnMut(&CanonicalType, &CanonicalType) -> TriState,
{
    let mut acc = TriState::Yes;
    for item in items {
        acc = acc.and(eval_is_subtype_with(item, target, leaf_oracle));
        if acc == TriState::No {
            return TriState::No;
        }
    }
    acc
}

fn eval_single_arg_covariant_super<F>(
    sub_elem: &CanonicalType,
    sup_args: &[CanonicalType],
    leaf_oracle: &mut F,
) -> TriState
where
    F: FnMut(&CanonicalType, &CanonicalType) -> TriState,
{
    if sup_args.is_empty() {
        TriState::Yes
    } else if sup_args.len() == 1 {
        eval_is_subtype_with(sub_elem, &sup_args[0], leaf_oracle)
    } else {
        TriState::No
    }
}

fn eval_single_arg_invariant_super<F>(
    sub_args: &[CanonicalType],
    sup_args: &[CanonicalType],
    leaf_oracle: &mut F,
) -> TriState
where
    F: FnMut(&CanonicalType, &CanonicalType) -> TriState,
{
    if sup_args.is_empty() {
        TriState::Yes
    } else if sub_args.is_empty() {
        TriState::Unknown
    } else if sub_args.len() == 1 && sup_args.len() == 1 {
        eval_is_subtype_with(&sub_args[0], &sup_args[0], leaf_oracle).and(eval_is_subtype_with(
            &sup_args[0],
            &sub_args[0],
            leaf_oracle,
        ))
    } else {
        TriState::No
    }
}

fn eval_stdlib_container_hierarchy<F>(
    sub_key: &str,
    sub_args: &[CanonicalType],
    sup_key: &str,
    sup_args: &[CanonicalType],
    leaf_oracle: &mut F,
) -> Option<TriState>
where
    F: FnMut(&CanonicalType, &CanonicalType) -> TriState,
{
    let sub_is_stdlib =
        is_builtin_closed_container_key(sub_key) || is_stdlib_collection_abc_key(sub_key);
    let sup_is_stdlib =
        is_builtin_closed_container_key(sup_key) || is_stdlib_collection_abc_key(sup_key);
    if !sub_is_stdlib || !sup_is_stdlib {
        return None;
    }

    let res = match (sub_key, sup_key) {
        (
            "tuple",
            "collections.abc.Sequence"
            | "collections.abc.Reversible"
            | "collections.abc.Collection"
            | "collections.abc.Iterable",
        ) => {
            if sup_args.is_empty() {
                TriState::Yes
            } else if sub_args.is_empty() {
                TriState::Unknown
            } else if sup_args.len() == 1 {
                eval_all_subtypes(sub_args, &sup_args[0], leaf_oracle)
            } else {
                TriState::No
            }
        }
        ("list", "collections.abc.MutableSequence") => {
            eval_single_arg_invariant_super(sub_args, sup_args, leaf_oracle)
        }
        (
            "list" | "collections.abc.MutableSequence",
            "collections.abc.Sequence"
            | "collections.abc.Reversible"
            | "collections.abc.Collection"
            | "collections.abc.Iterable",
        )
        | (
            "collections.abc.Sequence",
            "collections.abc.Reversible"
            | "collections.abc.Collection"
            | "collections.abc.Iterable",
        )
        | ("collections.abc.Reversible", "collections.abc.Iterable")
        | (
            "set" | "frozenset" | "collections.abc.MutableSet",
            "collections.abc.Set" | "collections.abc.Collection" | "collections.abc.Iterable",
        )
        | ("collections.abc.Set", "collections.abc.Collection" | "collections.abc.Iterable")
        | ("collections.abc.Collection", "collections.abc.Iterable") => {
            if sup_args.is_empty() {
                TriState::Yes
            } else if sub_args.is_empty() {
                TriState::Unknown
            } else if sub_args.len() == 1 && sup_args.len() == 1 {
                eval_is_subtype_with(&sub_args[0], &sup_args[0], leaf_oracle)
            } else {
                TriState::No
            }
        }
        ("set", "collections.abc.MutableSet") => {
            eval_single_arg_invariant_super(sub_args, sup_args, leaf_oracle)
        }
        ("dict", "collections.abc.MutableMapping") => {
            if sup_args.is_empty() {
                TriState::Yes
            } else if sub_args.is_empty() {
                TriState::Unknown
            } else if sub_args.len() == 2 && sup_args.len() == 2 {
                let k = eval_is_subtype_with(&sub_args[0], &sup_args[0], leaf_oracle).and(
                    eval_is_subtype_with(&sup_args[0], &sub_args[0], leaf_oracle),
                );
                let v = eval_is_subtype_with(&sub_args[1], &sup_args[1], leaf_oracle).and(
                    eval_is_subtype_with(&sup_args[1], &sub_args[1], leaf_oracle),
                );
                k.and(v)
            } else {
                TriState::No
            }
        }
        ("dict" | "collections.abc.MutableMapping", "collections.abc.Mapping") => {
            if sup_args.is_empty() {
                TriState::Yes
            } else if sub_args.is_empty() {
                TriState::Unknown
            } else if sub_args.len() == 2 && sup_args.len() == 2 {
                let k = eval_is_subtype_with(&sub_args[0], &sup_args[0], leaf_oracle).and(
                    eval_is_subtype_with(&sup_args[0], &sub_args[0], leaf_oracle),
                );
                let v = eval_is_subtype_with(&sub_args[1], &sup_args[1], leaf_oracle);
                k.and(v)
            } else {
                TriState::No
            }
        }
        (
            "dict" | "collections.abc.MutableMapping" | "collections.abc.Mapping",
            "collections.abc.Collection" | "collections.abc.Iterable",
        )
        | ("dict", "collections.abc.Reversible") => {
            if sup_args.is_empty() {
                TriState::Yes
            } else if sub_args.is_empty() {
                TriState::Unknown
            } else if sup_args.len() == 1 {
                eval_is_subtype_with(&sub_args[0], &sup_args[0], leaf_oracle)
            } else {
                TriState::No
            }
        }
        _ => TriState::No,
    };

    Some(res)
}

fn canonical_nominal_key(fqn: &str) -> &str {
    match fqn {
        "tuple" | "builtins.tuple" | "Tuple" | "typing.Tuple" | "typing_extensions.Tuple" => {
            "tuple"
        }
        "list" | "builtins.list" | "List" | "typing.List" => "list",
        "set" | "builtins.set" | "Set" | "typing.Set" => "set",
        "frozenset" | "builtins.frozenset" | "FrozenSet" | "typing.FrozenSet" => "frozenset",
        "dict" | "builtins.dict" | "Dict" | "typing.Dict" => "dict",
        "type" | "builtins.type" | "Type" | "typing.Type" => "type",
        "Sequence"
        | "builtins.Sequence"
        | "typing.Sequence"
        | "collections.abc.Sequence"
        | "typing_extensions.Sequence" => "collections.abc.Sequence",
        "MutableSequence"
        | "builtins.MutableSequence"
        | "typing.MutableSequence"
        | "collections.abc.MutableSequence" => "collections.abc.MutableSequence",
        "Collection"
        | "builtins.Collection"
        | "typing.Collection"
        | "collections.abc.Collection" => "collections.abc.Collection",
        "Iterable" | "builtins.Iterable" | "typing.Iterable" | "collections.abc.Iterable" => {
            "collections.abc.Iterable"
        }
        "Reversible" | "typing.Reversible" | "collections.abc.Reversible" => {
            "collections.abc.Reversible"
        }
        "AbstractSet" | "typing.AbstractSet" | "collections.abc.Set" => "collections.abc.Set",
        "MutableSet" | "typing.MutableSet" | "collections.abc.MutableSet" => {
            "collections.abc.MutableSet"
        }
        "Mapping" | "typing.Mapping" | "collections.abc.Mapping" => "collections.abc.Mapping",
        "MutableMapping" | "typing.MutableMapping" | "collections.abc.MutableMapping" => {
            "collections.abc.MutableMapping"
        }
        other => other.strip_prefix("builtins.").unwrap_or(other),
    }
}

fn is_builtin_closed_container_key(key: &str) -> bool {
    matches!(
        key,
        "list" | "set" | "frozenset" | "dict" | "tuple" | "type"
    )
}

fn is_stdlib_collection_abc_key(key: &str) -> bool {
    matches!(
        key,
        "collections.abc.Sequence"
            | "collections.abc.MutableSequence"
            | "collections.abc.Collection"
            | "collections.abc.Iterable"
            | "collections.abc.Reversible"
            | "collections.abc.Set"
            | "collections.abc.MutableSet"
            | "collections.abc.Mapping"
            | "collections.abc.MutableMapping"
    )
}

fn is_builtin_closed_container_fqn(fqn: &str) -> bool {
    is_builtin_closed_container_key(canonical_nominal_key(fqn))
}

/// Default conservative custom-equality check (`R13`, `R18`) when no MRO oracle is available.
pub fn eval_has_custom_eq_default(ty: &CanonicalType) -> TriState {
    match ty {
        CanonicalType::Primitive(_)
        | CanonicalType::NoneType
        | CanonicalType::Callable { .. }
        | CanonicalType::VariadicTuple(_) => TriState::No,
        CanonicalType::Nominal { fqname, .. } if is_builtin_closed_container_fqn(fqname) => {
            TriState::No
        }
        CanonicalType::Union(items) => {
            if items.is_empty() {
                return TriState::Unknown;
            }
            let mut saw_unknown = false;
            for item in items {
                match eval_has_custom_eq_default(item) {
                    TriState::Yes => return TriState::Yes,
                    TriState::Unknown => saw_unknown = true,
                    TriState::No => {}
                }
            }
            if saw_unknown {
                TriState::Unknown
            } else {
                TriState::No
            }
        }
        _ => TriState::Unknown,
    }
}

/// Checks whether two runtime Python values of types `lhs` and `rhs` have provably disjoint equality
/// (`1 in {1.0}` and `True in {1}` are valid in Python, so numeric-tower types are NEVER disjoint,
/// and classes with custom or unknown `__eq__` are NEVER assumed disjoint — `R13`, `R18`).
pub fn are_types_provably_disjoint(
    lhs: &CanonicalType,
    rhs: &CanonicalType,
    oracle: &dyn TypeOraclePort,
) -> bool {
    if lhs.is_unknown_or_any() || rhs.is_unknown_or_any() {
        return false;
    }
    match (lhs, rhs) {
        (CanonicalType::Union(items), other) => {
            !items.is_empty()
                && items
                    .iter()
                    .all(|m| are_types_provably_disjoint(m, other, oracle))
        }
        (other, CanonicalType::Union(items)) => {
            !items.is_empty()
                && items
                    .iter()
                    .all(|m| are_types_provably_disjoint(other, m, oracle))
        }
        (CanonicalType::Primitive(p1), CanonicalType::Primitive(p2))
            if p1.is_numeric_tower() && p2.is_numeric_tower() =>
        {
            false
        }
        (CanonicalType::VariadicTuple(t), CanonicalType::VariadicTuple(u)) => {
            are_types_provably_disjoint(t, u, oracle)
        }
        (CanonicalType::VariadicTuple(elem), CanonicalType::Nominal { fqname, args })
        | (CanonicalType::Nominal { fqname, args }, CanonicalType::VariadicTuple(elem))
            if canonical_nominal_key(fqname) == "tuple" =>
        {
            if args.is_empty() {
                false
            } else {
                args.iter()
                    .any(|arg| are_types_provably_disjoint(arg, elem, oracle))
            }
        }
        (
            CanonicalType::Nominal {
                fqname: fq1,
                args: args1,
            },
            CanonicalType::Nominal {
                fqname: fq2,
                args: args2,
            },
        ) if canonical_nominal_key(fq1) == "tuple" && canonical_nominal_key(fq2) == "tuple" => {
            if args1.is_empty() || args2.is_empty() {
                false
            } else if args1.len() != args2.len() {
                true
            } else {
                args1
                    .iter()
                    .zip(args2.iter())
                    .any(|(a, b)| are_types_provably_disjoint(a, b, oracle))
            }
        }
        _ => {
            oracle.is_subtype(lhs, rhs) == TriState::No
                && oracle.is_subtype(rhs, lhs) == TriState::No
                && oracle.has_custom_eq(lhs) == TriState::No
                && oracle.has_custom_eq(rhs) == TriState::No
        }
    }
}

/// Open-world polymorphic method-dispatch risk classification (`R17`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OverrideRisk {
    /// Receiver is a closed concrete subtype and no class in MRO below the anchor base overrides the method.
    SealedEquivalent,
    /// Either an MRO subclass overrides the method, or receiver is `self`/`cls` inside a non-`@final` class.
    PolymorphicOrOverridden,
    /// Receiver type or MRO cannot be proven closed.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Purity {
    Pure,
    ReadOnly,
    SideEffecting,
}

impl Purity {
    pub const fn combine(self, other: Purity) -> Purity {
        match (self, other) {
            (Purity::SideEffecting, _) | (_, Purity::SideEffecting) => Purity::SideEffecting,
            (Purity::ReadOnly, _) | (_, Purity::ReadOnly) => Purity::ReadOnly,
            (Purity::Pure, Purity::Pure) => Purity::Pure,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Applicability {
    Safe,       // Provably preserves or fixes broken semantics (--fix)
    Unsafe,     // Could alter valid behavior; requires --unsafe-fixes or human review
    ManualOnly, // Diagnostic only
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub range: TextRange,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImportSpec {
    pub module: String,
    pub symbol: Option<String>, // None => `import <module>`, Some(s) => `from <module> import <s>`
}

/// Call-site-anchored import synthesis request (`R19`) enabling lexical scope-aware import edits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScopedImportRequest {
    pub spec: ImportSpec,
    pub use_range: TextRange,
    pub replaces_fqname: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fix {
    pub title: String,
    pub edits: Vec<Edit>,
    pub required_imports: Vec<ImportSpec>,
    pub applicability: Applicability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    /// Higher rank wins during deterministic edit conflict resolution (`Error > Warning > Info`).
    pub const fn priority_rank(self) -> u8 {
        match self {
            Severity::Error => 3,
            Severity::Warning => 2,
            Severity::Info => 1,
        }
    }

    pub const fn as_upper_str(self) -> &'static str {
        match self {
            Severity::Error => "ERROR",
            Severity::Warning => "WARNING",
            Severity::Info => "INFO",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedMetavar {
    pub name: String,
    pub range: TextRange,
    pub is_variadic: bool,
    pub item_ranges: Vec<TextRange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternMatch {
    pub range: TextRange,
    pub captures: HashMap<String, CapturedMetavar>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFile {
    pub file_id: FileId,
    pub path: PathBuf,
    pub relative_path: String,
    pub source: String,
}

pub struct AnalysisView<'a> {
    pub file_id: FileId,
    pub file_path: &'a str,
    pub tree: &'a CanonicalSyntaxTree,
    pub oracle: &'a dyn TypeOraclePort,
    pub pattern_matcher: Option<&'a dyn PatternMatcherPort>,
}

impl<'a> AnalysisView<'a> {
    pub fn source(&self) -> &'a str {
        &self.tree.source
    }

    pub fn node(&self, id: NodeId) -> Option<CanonicalNode<'a>> {
        self.tree.node(id)
    }

    pub fn parent_of(&self, node: &CanonicalNode<'_>) -> Option<CanonicalNode<'a>> {
        node.parent.and_then(|pid| self.tree.node(pid))
    }

    pub fn node_meta(&self, id: NodeId) -> Option<&'a NodeMeta> {
        self.tree.node_meta(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyOracle;

    impl TypeOraclePort for DummyOracle {
        fn type_of(&self, _file_id: FileId, _range: TextRange) -> CanonicalType {
            CanonicalType::Unknown
        }

        fn resolve_fqname(&self, _file_id: FileId, _range: TextRange) -> Option<String> {
            None
        }

        fn is_subtype(&self, sub: &CanonicalType, sup: &CanonicalType) -> TriState {
            eval_is_subtype(sub, sup)
        }

        fn can_be_none(&self, _file_id: FileId, _range: TextRange) -> TriState {
            TriState::Unknown
        }

        fn expression_purity(&self, _file_id: FileId, _range: TextRange) -> Purity {
            Purity::SideEffecting
        }

        fn reverse_dependents(
            &self,
            _file_id: FileId,
            _changed_ranges: &[TextRange],
        ) -> Vec<(FileId, TextRange)> {
            Vec::new()
        }
    }

    #[test]
    fn r01_riffty_core_cargo_toml_has_zero_upstream_leakage() {
        let manifest = include_str!("../Cargo.toml");
        let deps_section = manifest
            .split("[dependencies]")
            .nth(1)
            .expect("missing [dependencies] section");
        for forbidden in ["ruff_", "ty_", "ast_grep", "ast-grep", "salsa"] {
            assert!(
                !deps_section.contains(forbidden),
                "R01 violation: riffty_core/Cargo.toml must not depend on `{forbidden}`"
            );
        }
    }

    #[test]
    fn text_range_slices_utf8_bytes_and_computes_line_col() {
        let src = "# header\nx = 'αβγ'\ny = 42\n";
        let tree = CanonicalSyntaxTree::new(
            FileId(1),
            src.to_string(),
            vec![StoredNode {
                id: NodeId(0),
                kind: NodeKind::Module,
                range: TextRange::new(0, src.len() as u32),
                parent: None,
                children: Vec::new(),
                meta: NodeMeta::None,
            }],
            extract_inline_suppressions(src),
        );
        let alpha_pos = src.find("'αβγ'").unwrap() as u32;
        let lit_range = TextRange::new(alpha_pos, alpha_pos + "'αβγ'".len() as u32);
        assert_eq!(lit_range.slice(src), Some("'αβγ'"));
        assert_eq!(tree.line_of(ByteOffset(alpha_pos)), 2);
        assert_eq!(tree.col_of(ByteOffset(alpha_pos)), 5);

        // Mid-codepoint byte index safely returns None rather than panicking
        let invalid = TextRange::new(alpha_pos + 2, alpha_pos + 4);
        assert_eq!(invalid.slice(src), None);
    }

    #[test]
    fn r03_tristate_subtype_soundness_suppresses_unknown_and_any() {
        let oracle = DummyOracle;
        let int_ty = CanonicalType::Primitive(PrimitiveKind::Int);
        let str_ty = CanonicalType::Primitive(PrimitiveKind::Str);
        let float_ty = CanonicalType::Primitive(PrimitiveKind::Float);
        let bool_ty = CanonicalType::Primitive(PrimitiveKind::Bool);

        assert_eq!(
            eval_is_subtype(&CanonicalType::Unknown, &int_ty),
            TriState::Unknown
        );
        assert_eq!(
            eval_is_subtype(&int_ty, &CanonicalType::Any),
            TriState::Unknown
        );
        assert!(!are_types_provably_disjoint(
            &CanonicalType::Unknown,
            &str_ty,
            &oracle
        ));
        assert!(!are_types_provably_disjoint(
            &CanonicalType::Any,
            &str_ty,
            &oracle
        ));

        // Disjoint concrete types: int vs str
        assert!(are_types_provably_disjoint(&int_ty, &str_ty, &oracle));

        // Numeric tower: int vs float, bool vs int are NOT disjoint in Python (`1 in {1.0}` is True)
        assert!(!are_types_provably_disjoint(&int_ty, &float_ty, &oracle));
        assert!(!are_types_provably_disjoint(&bool_ty, &int_ty, &oracle));

        // Union disjointness
        let int_or_none = CanonicalType::Union(vec![int_ty.clone(), CanonicalType::NoneType]);
        assert!(are_types_provably_disjoint(&int_or_none, &str_ty, &oracle));
        let int_or_str = CanonicalType::Union(vec![int_ty, str_ty.clone()]);
        assert!(!are_types_provably_disjoint(&int_or_str, &str_ty, &oracle));
    }

    #[test]
    fn applicability_ordering_and_purity_lattice() {
        assert!(Applicability::Safe < Applicability::Unsafe);
        assert!(Applicability::Unsafe < Applicability::ManualOnly);
        assert_eq!(Purity::Pure.combine(Purity::Pure), Purity::Pure);
        assert_eq!(Purity::Pure.combine(Purity::ReadOnly), Purity::ReadOnly);
        assert_eq!(
            Purity::ReadOnly.combine(Purity::SideEffecting),
            Purity::SideEffecting
        );
    }

    #[test]
    fn r18_open_world_nominal_vs_primitive_and_distinct_nominals_return_unknown_without_mro() {
        let oracle = DummyOracle;
        let str_ty = CanonicalType::Primitive(PrimitiveKind::Str);
        let rule_enum_ty = CanonicalType::Nominal {
            fqname: "bump_pydantic.codemods.Rule".to_string(),
            args: Vec::new(),
        };
        let other_cls_ty = CanonicalType::Nominal {
            fqname: "pkg.CustomWrapper".to_string(),
            args: Vec::new(),
        };

        // Without MRO knowledge, `Rule` could subclass `str` (`class Rule(str, Enum)`), so NEVER return `No`.
        assert_eq!(eval_is_subtype(&rule_enum_ty, &str_ty), TriState::Unknown);
        assert_eq!(eval_is_subtype(&str_ty, &rule_enum_ty), TriState::Unknown);
        assert_eq!(
            eval_is_subtype(&rule_enum_ty, &other_cls_ty),
            TriState::Unknown
        );
        assert!(!are_types_provably_disjoint(
            &rule_enum_ty,
            &str_ty,
            &oracle
        ));
        assert!(!are_types_provably_disjoint(
            &rule_enum_ty,
            &other_cls_ty,
            &oracle
        ));
    }

    #[test]
    fn r13_custom_eq_blocks_disjointness_even_when_subtypes_are_disjoint() {
        struct CustomEqOracle;
        impl TypeOraclePort for CustomEqOracle {
            fn type_of(&self, _file_id: FileId, _range: TextRange) -> CanonicalType {
                CanonicalType::Unknown
            }
            fn resolve_fqname(&self, _file_id: FileId, _range: TextRange) -> Option<String> {
                None
            }
            fn is_subtype(&self, _sub: &CanonicalType, _sup: &CanonicalType) -> TriState {
                TriState::No
            }
            fn can_be_none(&self, _file_id: FileId, _range: TextRange) -> TriState {
                TriState::Unknown
            }
            fn expression_purity(&self, _file_id: FileId, _range: TextRange) -> Purity {
                Purity::SideEffecting
            }
            fn reverse_dependents(
                &self,
                _file_id: FileId,
                _changed_ranges: &[TextRange],
            ) -> Vec<(FileId, TextRange)> {
                Vec::new()
            }
            fn has_custom_eq(&self, ty: &CanonicalType) -> TriState {
                match ty {
                    CanonicalType::Nominal { fqname, .. } if fqname == "pkg.FlexibleTag" => {
                        TriState::Yes
                    }
                    _ => TriState::No,
                }
            }
        }

        let oracle = CustomEqOracle;
        let tag_ty = CanonicalType::Nominal {
            fqname: "pkg.FlexibleTag".to_string(),
            args: Vec::new(),
        };
        let str_ty = CanonicalType::Primitive(PrimitiveKind::Str);
        assert!(!are_types_provably_disjoint(&str_ty, &tag_ty, &oracle));
        assert!(!are_types_provably_disjoint(&tag_ty, &str_ty, &oracle));
    }

    #[test]
    fn r21_node_kind_indices_cover_all_17_variants_bijectively() {
        assert_eq!(NodeKind::ALL.len(), NodeKind::COUNT);
        for (idx, kind) in NodeKind::ALL.iter().copied().enumerate() {
            assert_eq!(kind.as_index(), idx);
        }
    }

    #[test]
    fn issue_6_variadic_tuple_representation_subtyping_disjointness_and_covariant_abcs() {
        let oracle = DummyOracle;
        let str_ty = CanonicalType::Primitive(PrimitiveKind::Str);
        let int_ty = CanonicalType::Primitive(PrimitiveKind::Int);
        let float_ty = CanonicalType::Primitive(PrimitiveKind::Float);
        let bool_ty = CanonicalType::Primitive(PrimitiveKind::Bool);

        let var_str = CanonicalType::parse_type_expr("tuple[str, ...]");
        assert_eq!(
            var_str,
            CanonicalType::VariadicTuple(Box::new(str_ty.clone()))
        );
        assert_eq!(var_str.display_short(), "tuple[str, ...]");
        assert_eq!(var_str.container_element_type(), Some(str_ty.clone()));
        assert_eq!(var_str.subscript_value_type(), Some(str_ty.clone()));

        let nested_list = CanonicalType::parse_type_expr("list[tuple[str, ...]]");
        assert_eq!(nested_list.display_short(), "list[tuple[str, ...]]");
        assert_eq!(nested_list.container_element_type(), Some(var_str.clone()));

        let fixed_1_int = CanonicalType::parse_type_expr("tuple[int]");
        assert_eq!(fixed_1_int.display_short(), "tuple[int]");
        let fixed_2_int = CanonicalType::parse_type_expr("tuple[int, int]");
        let fixed_4_str = CanonicalType::parse_type_expr("tuple[str, str, str, str]");
        let fixed_3_int = CanonicalType::parse_type_expr("tuple[int, int, int]");
        let var_float = CanonicalType::parse_type_expr("Tuple[float, ...]");
        let var_bool = CanonicalType::parse_type_expr("tuple[bool, ...]");

        // Subtyping: fixed-arity <: variadic iff all Ti <: U
        assert_eq!(eval_is_subtype(&fixed_4_str, &var_str), TriState::Yes);
        assert_eq!(eval_is_subtype(&fixed_2_int, &var_float), TriState::Yes);
        assert_eq!(eval_is_subtype(&fixed_3_int, &var_str), TriState::No);
        // Variadic is NOT a subtype of fixed-arity
        assert_eq!(eval_is_subtype(&var_str, &fixed_4_str), TriState::No);
        assert_eq!(eval_is_subtype(&var_float, &fixed_1_int), TriState::No);
        // Variadic <: Variadic is covariant
        assert_eq!(eval_is_subtype(&var_bool, &var_float), TriState::Yes);
        assert_eq!(eval_is_subtype(&var_float, &var_str), TriState::No);

        // Disjointness: openshift/lightspeed-service 4-tuple vs tuple[str, ...]
        assert!(!are_types_provably_disjoint(
            &fixed_4_str,
            &var_str,
            &oracle
        ));
        assert!(!are_types_provably_disjoint(
            &var_str,
            &fixed_4_str,
            &oracle
        ));
        assert!(are_types_provably_disjoint(&fixed_3_int, &var_str, &oracle));
        assert!(are_types_provably_disjoint(
            &fixed_2_int,
            &fixed_1_int,
            &oracle
        ));

        // Overlap without subtyping: tuple[int | str, str] vs tuple[str, ...] is NOT disjoint
        let union_pair = CanonicalType::Nominal {
            fqname: "builtins.tuple".to_string(),
            args: vec![
                CanonicalType::Union(vec![int_ty.clone(), str_ty.clone()]),
                str_ty.clone(),
            ],
        };
        assert_eq!(eval_is_subtype(&union_pair, &var_str), TriState::No);
        assert_eq!(eval_is_subtype(&var_str, &union_pair), TriState::No);
        assert!(!are_types_provably_disjoint(&union_pair, &var_str, &oracle));

        // Numeric tower across opposite tuple slots: (float, int) vs (int, float) is NOT disjoint (`(1.0, 2) == (1, 2.0)`)
        let float_int = CanonicalType::Nominal {
            fqname: "builtins.tuple".to_string(),
            args: vec![float_ty.clone(), int_ty.clone()],
        };
        let int_float = CanonicalType::Nominal {
            fqname: "builtins.tuple".to_string(),
            args: vec![int_ty, float_ty.clone()],
        };
        assert!(!are_types_provably_disjoint(
            &float_int, &int_float, &oracle
        ));

        // Covariant Sequence / Collection / Iterable ABC subtyping
        let seq_str = CanonicalType::parse_type_expr("Sequence[str]");
        let iter_str = CanonicalType::parse_type_expr("Iterable[str]");
        let coll_float = CanonicalType::parse_type_expr("Collection[float]");
        let list_str = CanonicalType::parse_type_expr("list[str]");
        let tuple_bool_int = CanonicalType::Nominal {
            fqname: "builtins.tuple".to_string(),
            args: vec![bool_ty, float_ty],
        };
        assert_eq!(eval_is_subtype(&fixed_4_str, &seq_str), TriState::Yes);
        assert_eq!(eval_is_subtype(&fixed_4_str, &iter_str), TriState::Yes);
        assert_eq!(eval_is_subtype(&var_str, &seq_str), TriState::Yes);
        assert_eq!(eval_is_subtype(&list_str, &seq_str), TriState::Yes);
        assert_eq!(eval_is_subtype(&list_str, &iter_str), TriState::Yes);
        assert_eq!(eval_is_subtype(&str_ty, &seq_str), TriState::Yes);
        assert_eq!(eval_is_subtype(&tuple_bool_int, &coll_float), TriState::Yes);
        assert_eq!(eval_is_subtype(&fixed_3_int, &seq_str), TriState::No);

        // Balanced outer parens on multi-group union string
        let multi_union = CanonicalType::parse_type_expr("(int | str) | (float | bool)");
        assert_eq!(multi_union.display_short(), "int | str | float | bool");
    }

    #[test]
    fn line_index_maps_utf8_offsets_and_slices_accurately() {
        let src = "alpha = 1\ncafé = 2\n🚀 = 3";
        let idx = LineIndex::new(src);
        assert_eq!(idx.line_count(), 3);
        assert_eq!(idx.line_col(src, ByteOffset(0)), (1, 1));
        assert_eq!(idx.line_col(src, ByteOffset(10)), (2, 1));
        // 'é' is 2 bytes in UTF-8, so "café" is 5 bytes (4 chars)
        let eq_pos = src.find("= 2").unwrap() as u32;
        assert_eq!(idx.line_col(src, ByteOffset(eq_pos)), (2, 6));
        let rocket_eq = src.find("= 3").unwrap() as u32;
        assert_eq!(idx.line_col(src, ByteOffset(rocket_eq)), (3, 3));
        assert_eq!(idx.line_slice(src, 1), "alpha = 1");
        assert_eq!(idx.line_slice(src, 2), "café = 2");
        assert_eq!(idx.line_slice(src, 3), "🚀 = 3");
        assert_eq!(idx.line_slice(src, 99), "");
    }

    #[test]
    fn req_m0_01_language_jsts_str_serde_and_extensions() {
        assert_eq!(Language::JsTs.as_str(), "jsts");
        assert_eq!(
            serde_json::to_string(&Language::JsTs).unwrap(),
            "\"jsts\""
        );
        assert_eq!(
            serde_json::from_str::<Language>("\"jsts\"").unwrap(),
            Language::JsTs
        );

        for ext in ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "TS", "TSX", "MTS", "CTS", "JS", "JSX", "MJS", "CJS"] {
            assert_eq!(
                Language::from_extension(ext),
                Some(Language::JsTs),
                "expected Language::JsTs for extension `{ext}`"
            );
        }
        assert_eq!(Language::from_extension("py"), Some(Language::Python));
        assert_eq!(Language::from_extension("java"), Some(Language::Java));
        assert_eq!(Language::from_extension("rs"), None);
        assert_eq!(Language::from_extension("json"), None);
    }
}

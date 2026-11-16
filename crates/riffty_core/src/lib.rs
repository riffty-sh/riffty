pub mod ports;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use ports::{
    DiffTarget, FileDiffRanges, GitDiffPort, ImportManagerPort, PatchApplierPort,
    PatternMatcherPort, RulePlugin, SyntaxParserPort, TypeOraclePort, WorkspaceVfsPort,
};

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
                    || sup
                        .rule_ids
                        .iter()
                        .any(|r| r.eq_ignore_ascii_case(rule_id)))
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

    /// Extracts the membership element type `T` tested by `x in container`.
    pub fn container_element_type(&self) -> Option<CanonicalType> {
        match self {
            CanonicalType::Primitive(PrimitiveKind::Str) => {
                Some(CanonicalType::Primitive(PrimitiveKind::Str))
            }
            CanonicalType::Nominal { fqname, args } => match fqname.as_str() {
                "builtins.set"
                | "set"
                | "builtins.frozenset"
                | "frozenset"
                | "builtins.list"
                | "list"
                | "builtins.Sequence"
                | "typing.Sequence"
                | "collections.abc.Sequence"
                | "typing.Set"
                | "typing.FrozenSet"
                | "typing.List"
                | "typing.AbstractSet"
                | "collections.abc.Set"
                | "collections.abc.Collection"
                | "typing.Collection" => args.first().cloned(),
                "builtins.dict"
                | "dict"
                | "typing.Dict"
                | "typing.Mapping"
                | "collections.abc.Mapping" => args.first().cloned(),
                "builtins.tuple" | "tuple" | "typing.Tuple" => {
                    if args.is_empty() {
                        None
                    } else if args.len() == 1 {
                        args.first().cloned()
                    } else {
                        Some(CanonicalType::Union(args.clone()))
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
                    fqname.as_str(),
                    "builtins.dict"
                        | "dict"
                        | "typing.Dict"
                        | "typing.Mapping"
                        | "collections.abc.Mapping"
                        | "typing.MutableMapping"
                        | "collections.abc.MutableMapping"
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
            CanonicalType::Nominal { fqname, args } => {
                let short = fqname.strip_prefix("builtins.").unwrap_or(fqname);
                if args.is_empty() {
                    short.to_string()
                } else {
                    let inner: Vec<String> = args.iter().map(CanonicalType::display_short).collect();
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

/// Deterministic Kleene 3-valued subtype evaluator enforcing `R03` (Zero-False-Positive Soundness).
pub fn eval_is_subtype(sub: &CanonicalType, sup: &CanonicalType) -> TriState {
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
            let mut saw_unknown = false;
            for m in sub_members {
                match eval_is_subtype(m, sup) {
                    TriState::Yes => {}
                    TriState::No => return TriState::No,
                    TriState::Unknown => saw_unknown = true,
                }
            }
            if saw_unknown {
                TriState::Unknown
            } else {
                TriState::Yes
            }
        }
        (_, CanonicalType::Union(sup_members)) => {
            let mut saw_unknown = false;
            for m in sup_members {
                match eval_is_subtype(sub, m) {
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
        // PEP 484 / Python runtime subclassing: bool <: int, and int <: float in PEP 484 type tower
        (CanonicalType::Primitive(PrimitiveKind::Bool), CanonicalType::Primitive(PrimitiveKind::Int))
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
        (CanonicalType::Primitive(p), CanonicalType::Nominal { fqname, args }) => {
            if fqname == "builtins.object"
                || fqname == "object"
                || (p.fqname() == fqname && args.is_empty())
            {
                TriState::Yes
            } else if is_builtin_closed_container_fqn(fqname) {
                TriState::No
            } else {
                // Without MRO knowledge, `fqname` could be an ABC/Protocol/supertype (`R18`)
                TriState::Unknown
            }
        }
        (CanonicalType::Nominal { fqname, args }, CanonicalType::Primitive(p)) => {
            if p.fqname() == fqname && args.is_empty() {
                TriState::Yes
            } else if is_builtin_closed_container_fqn(fqname) {
                TriState::No
            } else {
                // Without MRO knowledge, `fqname` could subclass `p` (e.g. `class Rule(str, Enum)`) (`R18`)
                TriState::Unknown
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
            if sup_fq == "builtins.object" || sup_fq == "object" {
                return TriState::Yes;
            }
            if normalize_fqn(sub_fq) == normalize_fqn(sup_fq) {
                if sup_args.is_empty() || sub_args.is_empty() {
                    return TriState::Yes;
                }
                if sub_args.len() != sup_args.len() {
                    return TriState::No;
                }
                let mut saw_unknown = false;
                for (a, b) in sub_args.iter().zip(sup_args.iter()) {
                    match eval_is_subtype(a, b) {
                        TriState::Yes => {}
                        TriState::No => return TriState::No,
                        TriState::Unknown => saw_unknown = true,
                    }
                }
                if saw_unknown {
                    TriState::Unknown
                } else {
                    TriState::Yes
                }
            } else if is_builtin_closed_container_fqn(sub_fq)
                && is_builtin_closed_container_fqn(sup_fq)
            {
                TriState::No
            } else {
                // Distinct nominals without MRO context may be in an inheritance relation (`R18`)
                TriState::Unknown
            }
        }
        (CanonicalType::Nominal { .. }, CanonicalType::Callable { .. }) => {
            // A class may implement `__call__`
            TriState::Unknown
        }
        _ => TriState::No,
    }
}

fn is_builtin_closed_container_fqn(fqn: &str) -> bool {
    matches!(
        normalize_fqn(fqn),
        "list" | "set" | "frozenset" | "dict" | "tuple" | "type"
    )
}

/// Default conservative custom-equality check (`R13`, `R18`) when no MRO oracle is available.
pub fn eval_has_custom_eq_default(ty: &CanonicalType) -> TriState {
    match ty {
        CanonicalType::Primitive(_)
        | CanonicalType::NoneType
        | CanonicalType::Callable { .. } => TriState::No,
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
        _ => {
            oracle.is_subtype(lhs, rhs) == TriState::No
                && oracle.is_subtype(rhs, lhs) == TriState::No
                && oracle.has_custom_eq(lhs) == TriState::No
                && oracle.has_custom_eq(rhs) == TriState::No
        }
    }
}

fn normalize_fqn(fqn: &str) -> &str {
    fqn.strip_prefix("builtins.").unwrap_or(fqn)
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

        assert_eq!(eval_is_subtype(&CanonicalType::Unknown, &int_ty), TriState::Unknown);
        assert_eq!(eval_is_subtype(&int_ty, &CanonicalType::Any), TriState::Unknown);
        assert!(!are_types_provably_disjoint(&CanonicalType::Unknown, &str_ty, &oracle));
        assert!(!are_types_provably_disjoint(&CanonicalType::Any, &str_ty, &oracle));

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
        assert!(!are_types_provably_disjoint(&rule_enum_ty, &str_ty, &oracle));
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
}

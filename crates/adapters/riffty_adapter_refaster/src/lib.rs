use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use riffty_core::{
    AnalysisView, Applicability, CanonicalNode, CanonicalSyntaxTree, CanonicalType,
    CapturedMetavar, Diagnostic, Edit, Fix, ImportSpec, NodeKind, NodeMeta, OverrideRisk,
    PatternMatch, PrimitiveKind, Purity, RulePlugin, Severity, TextRange, TriState,
};
use serde::{Deserialize, Serialize};

static INTERNED_IDS: LazyLock<Mutex<HashSet<&'static str>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn intern_rule_id(id: &str) -> &'static str {
    let mut guard = INTERNED_IDS.lock().expect("interner lock");
    if let Some(&existing) = guard.get(id) {
        return existing;
    }
    let leaked: &'static str = Box::leak(id.to_string().into_boxed_str());
    guard.insert(leaked);
    leaked
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum YamlDocument {
    Envelope { rules: Vec<RefasterRuleSpec> },
    List(Vec<RefasterRuleSpec>),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RefasterRuleSpec {
    pub id: String,
    pub name: String,
    #[serde(default = "default_severity")]
    pub severity: YamlSeverity,
    #[serde(default = "default_applicability")]
    pub applicability: YamlApplicability,
    pub message: String,
    pub pattern: String,
    #[serde(default)]
    pub guards: Vec<RefasterGuard>,
    #[serde(default)]
    pub replace: Option<String>,
    #[serde(default)]
    pub fix_title: Option<String>,
    #[serde(default)]
    pub imports: Vec<ImportSpec>,
    #[serde(default)]
    pub bind_impure_once: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum YamlSeverity {
    #[serde(alias = "Error", alias = "ERROR")]
    Error,
    #[serde(alias = "Warning", alias = "WARNING")]
    Warning,
    #[serde(alias = "Info", alias = "INFO")]
    Info,
}

const fn default_severity() -> YamlSeverity {
    YamlSeverity::Warning
}

impl From<YamlSeverity> for Severity {
    fn from(value: YamlSeverity) -> Self {
        match value {
            YamlSeverity::Error => Severity::Error,
            YamlSeverity::Warning => Severity::Warning,
            YamlSeverity::Info => Severity::Info,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum YamlApplicability {
    #[serde(alias = "Safe", alias = "SAFE")]
    Safe,
    #[serde(alias = "Unsafe", alias = "UNSAFE")]
    Unsafe,
    #[serde(alias = "ManualOnly", alias = "MANUAL_ONLY")]
    ManualOnly,
}

const fn default_applicability() -> YamlApplicability {
    YamlApplicability::Safe
}

impl From<YamlApplicability> for Applicability {
    fn from(value: YamlApplicability) -> Self {
        match value {
            YamlApplicability::Safe => Applicability::Safe,
            YamlApplicability::Unsafe => Applicability::Unsafe,
            YamlApplicability::ManualOnly => Applicability::ManualOnly,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum YamlPurity {
    #[serde(alias = "Pure", alias = "PURE")]
    Pure,
    #[serde(alias = "ReadOnly", alias = "READ_ONLY")]
    ReadOnly,
    #[serde(alias = "SideEffecting", alias = "SIDE_EFFECTING")]
    SideEffecting,
}

const fn default_purity() -> YamlPurity {
    YamlPurity::Pure
}

const fn default_true() -> bool {
    true
}

impl From<YamlPurity> for Purity {
    fn from(value: YamlPurity) -> Self {
        match value {
            YamlPurity::Pure => Purity::Pure,
            YamlPurity::ReadOnly => Purity::ReadOnly,
            YamlPurity::SideEffecting => Purity::SideEffecting,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RefasterGuard {
    FqnameIn {
        #[serde(default)]
        target: Option<String>,
        allowed: Vec<String>,
    },
    TypeAssignableTo {
        target: String,
        expected_type: String,
    },
    Purity {
        target: String,
        #[serde(default = "default_purity")]
        expected: YamlPurity,
    },
    CallableAcceptsKwarg {
        target: String,
        kwarg: String,
        #[serde(default = "default_true")]
        expected: bool,
    },
    #[serde(alias = "enclosing_async_fn")]
    InsideAsyncDef {
        #[serde(default = "default_true")]
        expected: bool,
    },
    InsideSubclassOf {
        expected_type: String,
    },
    NotOverriddenBelow {
        target: String,
        method: String,
        base_type: String,
        #[serde(default)]
        downgrade_to_unsafe: bool,
    },
    NotInsideWith {
        callees: Vec<String>,
    },
}

/// Evaluates a declarative Refaster YAML rule by bridging `PatternMatcherPort` (`ast-grep`)
/// byte spans to `TypeOraclePort` semantic/purity guards (`R05`).
pub struct RefasterYamlRule {
    static_id: &'static str,
    spec: RefasterRuleSpec,
}

impl RefasterYamlRule {
    pub fn new(spec: RefasterRuleSpec) -> Self {
        let static_id = intern_rule_id(&spec.id);
        Self { static_id, spec }
    }

    pub fn from_yaml_str(yaml: &str) -> Result<Vec<Box<dyn RulePlugin>>, String> {
        let parsed: YamlDocument =
            serde_yaml_ng::from_str(yaml).map_err(|e| format!("invalid Refaster YAML: {e}"))?;
        let specs = match parsed {
            YamlDocument::Envelope { rules } => rules,
            YamlDocument::List(rules) => rules,
        };
        let mut plugins: Vec<Box<dyn RulePlugin>> = Vec::with_capacity(specs.len());
        for spec in specs {
            if spec.id.trim().is_empty() {
                return Err("Refaster rule `id` must not be empty".to_string());
            }
            if spec.pattern.trim().is_empty() {
                return Err(format!("Refaster rule `{}` has an empty `pattern`", spec.id));
            }
            plugins.push(Box::new(Self::new(spec)));
        }
        Ok(plugins)
    }

    pub fn from_yaml_file(path: &Path) -> Result<Vec<Box<dyn RulePlugin>>, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read Refaster YAML `{}`: {e}", path.display()))?;
        Self::from_yaml_str(&content)
    }
}

impl RulePlugin for RefasterYamlRule {
    fn id(&self) -> &'static str {
        self.static_id
    }

    fn target_node_kinds(&self) -> &[NodeKind] {
        const MODULE_ONLY: &[NodeKind] = &[NodeKind::Module];
        MODULE_ONLY
    }

    fn evaluate(&self, _node: &CanonicalNode, view: &AnalysisView) -> Vec<Diagnostic> {
        let Some(matcher) = view.pattern_matcher else {
            return Vec::new();
        };
        let Ok(matches) = matcher.find_matches(&view.tree.source, &self.spec.pattern) else {
            return Vec::new();
        };

        let mut diagnostics = Vec::new();
        for m in matches {
            let Some((type_proof, force_unsafe)) = evaluate_guards(&self.spec.guards, &m, view)
            else {
                continue;
            };

            let message = interpolate_template(&self.spec.message, &m.captures, &view.tree.source);
            let mut fix = self
                .spec
                .replace
                .as_deref()
                .and_then(|tpl| build_purity_guarded_fix(&self.spec, tpl, &m, view));
            if force_unsafe
                && let Some(ref mut f) = fix
                && f.applicability == Applicability::Safe
            {
                f.applicability = Applicability::Unsafe;
            }

            diagnostics.push(Diagnostic {
                rule_id: self.spec.id.clone(),
                rule_name: self.spec.name.clone(),
                severity: self.spec.severity.into(),
                file_id: view.file_id,
                range: m.range,
                message,
                type_proof,
                fix,
            });
        }
        diagnostics
    }
}

fn resolve_target_range(target: Option<&str>, m: &PatternMatch) -> Option<TextRange> {
    match target {
        None | Some("") | Some("$MATCH") => Some(m.range),
        Some(raw) => {
            let clean = raw
                .trim()
                .strip_prefix("$$$")
                .or_else(|| raw.trim().strip_prefix('$'))
                .unwrap_or_else(|| raw.trim());
            m.captures.get(clean).map(|c| c.range)
        }
    }
}

fn evaluate_guards(
    guards: &[RefasterGuard],
    m: &PatternMatch,
    view: &AnalysisView<'_>,
) -> Option<(Option<serde_json::Value>, bool)> {
    if guards.is_empty() {
        return Some((None, false));
    }

    let mut proof_map = serde_json::Map::new();
    let mut force_unsafe = false;
    for guard in guards {
        match guard {
            RefasterGuard::FqnameIn { target, allowed } => {
                let span = resolve_target_range(target.as_deref(), m)?;
                let resolved = query_fqname_on_or_callee(view, span)?;
                if !allowed.iter().any(|a| a == &resolved) {
                    return None;
                }
                proof_map.insert(
                    "resolved_fqname".to_string(),
                    serde_json::Value::String(resolved),
                );
            }
            RefasterGuard::TypeAssignableTo {
                target,
                expected_type,
            } => {
                let span = resolve_target_range(Some(target), m)?;
                let actual = view.oracle.type_of(view.file_id, span);
                if actual.is_unknown_or_any() {
                    // Enforce `R03`: Zero-False-Positive Soundness
                    return None;
                }
                let expected = parse_expected_canonical_type(expected_type);
                if view.oracle.is_subtype(&actual, &expected) != TriState::Yes {
                    return None;
                }
                proof_map.insert(
                    format!("type_of_{}", target.trim_start_matches('$')),
                    serde_json::Value::String(actual.display_short()),
                );
            }
            RefasterGuard::Purity { target, expected } => {
                let span = resolve_target_range(Some(target), m)?;
                let actual = view.oracle.expression_purity(view.file_id, span);
                let required: Purity = (*expected).into();
                if actual > required {
                    return None;
                }
                proof_map.insert(
                    format!("purity_of_{}", target.trim_start_matches('$')),
                    serde_json::Value::String(format!("{actual:?}")),
                );
            }
            RefasterGuard::CallableAcceptsKwarg {
                target,
                kwarg,
                expected,
            } => {
                let span = resolve_target_range(Some(target), m)?;
                let callee_ty = view.oracle.type_of(view.file_id, span);
                if callee_ty.is_unknown_or_any() {
                    // Enforce `R03`: suppress rather than guess when callable signature is unknown
                    return None;
                }
                let CanonicalType::Callable {
                    params,
                    has_var_kwargs,
                    ..
                } = callee_ty
                else {
                    return None;
                };
                let accepts = has_var_kwargs || params.iter().any(|p| p.name == *kwarg);
                if accepts != *expected {
                    return None;
                }
                proof_map.insert(
                    "callable_accepts_kwarg".to_string(),
                    serde_json::json!({
                        "kwarg": kwarg,
                        "accepts": accepts,
                        "has_var_kwargs": has_var_kwargs,
                    }),
                );
            }
            RefasterGuard::InsideAsyncDef { expected } => {
                let is_async = is_span_inside_async_def(view.tree, m.range);
                if is_async != *expected {
                    return None;
                }
                proof_map.insert(
                    "inside_async_def".to_string(),
                    serde_json::Value::Bool(is_async),
                );
            }
            RefasterGuard::InsideSubclassOf { expected_type } => {
                let expected = parse_expected_canonical_type(expected_type);
                let matched_cls = find_enclosing_subclass(view, m.range, &expected)?;
                proof_map.insert(
                    "enclosing_subclass".to_string(),
                    serde_json::Value::String(matched_cls),
                );
            }
            RefasterGuard::NotOverriddenBelow {
                target,
                method,
                base_type,
                downgrade_to_unsafe,
            } => {
                let span = resolve_target_range(Some(target), m)?;
                let risk =
                    view.oracle
                        .method_override_risk(view.file_id, span, method, base_type);
                match risk {
                    OverrideRisk::SealedEquivalent => {
                        proof_map.insert(
                            "override_risk".to_string(),
                            serde_json::Value::String("SealedEquivalent".to_string()),
                        );
                    }
                    OverrideRisk::PolymorphicOrOverridden | OverrideRisk::Unknown => {
                        if !*downgrade_to_unsafe {
                            return None;
                        }
                        force_unsafe = true;
                        proof_map.insert(
                            "override_risk".to_string(),
                            serde_json::Value::String(format!("{risk:?}")),
                        );
                    }
                }
            }
            RefasterGuard::NotInsideWith { callees } => {
                if is_span_inside_matching_with(view, m.range, callees) {
                    return None;
                }
                proof_map.insert(
                    "not_inside_with".to_string(),
                    serde_json::json!(callees),
                );
            }
        }
    }

    if proof_map.is_empty() {
        Some((None, force_unsafe))
    } else {
        Some((Some(serde_json::Value::Object(proof_map)), force_unsafe))
    }
}

fn is_span_inside_matching_with(
    view: &AnalysisView<'_>,
    span: TextRange,
    callees: &[String],
) -> bool {
    for node in view.tree.iter_nodes() {
        if node.kind != NodeKind::With || !node.range.contains_range(span) {
            continue;
        }
        let Some(NodeMeta::With(with_meta)) = view.tree.node_meta(node.id) else {
            continue;
        };
        for item in &with_meta.items {
            let resolved_fqn = query_fqname_on_or_callee(view, item.context_expr_range);
            let raw_callee = item
                .context_expr_range
                .slice(&view.tree.source)
                .map(|s| s.split('(').next().unwrap_or(s).trim());
            for want in callees {
                if resolved_fqn.as_deref() == Some(want.as_str()) {
                    return true;
                }
                if !want.contains('.') {
                    if resolved_fqn
                        .as_deref()
                        .and_then(|f| f.rsplit('.').next())
                        == Some(want.as_str())
                    {
                        return true;
                    }
                    if raw_callee.and_then(|r| r.rsplit('.').next()) == Some(want.as_str()) {
                        return true;
                    }
                } else if raw_callee == Some(want.as_str()) {
                    return true;
                }
            }
        }
    }
    false
}

fn is_span_inside_async_def(tree: &CanonicalSyntaxTree, span: TextRange) -> bool {
    let mut best: Option<(u32, NodeKind)> = None;
    for node in tree.iter_nodes() {
        if matches!(node.kind, NodeKind::FunctionDef | NodeKind::AsyncFunctionDef)
            && node.range.contains_range(span)
        {
            let len = node.range.len();
            if best.is_none_or(|(prev_len, _)| len <= prev_len) {
                best = Some((len, node.kind));
            }
        }
    }
    matches!(best, Some((_, NodeKind::AsyncFunctionDef)))
}

fn find_enclosing_subclass(
    view: &AnalysisView<'_>,
    span: TextRange,
    expected: &CanonicalType,
) -> Option<String> {
    let mut best: Option<(u32, String)> = None;
    for node in view.tree.iter_nodes() {
        if node.kind == NodeKind::ClassDef && node.range.contains_range(span) {
            let cls_ty = view.oracle.type_of(view.file_id, node.range);
            if !cls_ty.is_unknown_or_any()
                && view.oracle.is_subtype(&cls_ty, expected) == TriState::Yes
            {
                let len = node.range.len();
                if best.as_ref().is_none_or(|(prev_len, _)| len <= *prev_len) {
                    best = Some((len, cls_ty.display_short()));
                }
            }
        }
    }
    best.map(|(_, fqn)| fqn)
}

fn query_fqname_on_or_callee(view: &AnalysisView<'_>, span: TextRange) -> Option<String> {
    if let Some(direct) = view.oracle.resolve_fqname(view.file_id, span) {
        return Some(direct);
    }
    // If `span` covers a full `Call` expression (`pkg.func(...)`), inspect its callee prefix before `(`
    let slice = span.slice(&view.tree.source)?;
    let open_paren = slice.find('(')?;
    let callee_len = slice[..open_paren].trim_end().len() as u32;
    if callee_len == 0 {
        return None;
    }
    let callee_range = TextRange::new(span.start.0, span.start.0 + callee_len);
    view.oracle.resolve_fqname(view.file_id, callee_range)
}

/// Enforces `R05` (Purity-Guarded Metavariable Duplication):
/// If any captured metavariable (`$VAR` or `$$$ARGS`) is referenced 2 or more times in `template`,
/// its `expression_purity` MUST equal `Purity::Pure`; otherwise no `Fix` is emitted so impure
/// expressions are never duplicated!
fn build_purity_guarded_fix(
    spec: &RefasterRuleSpec,
    template: &str,
    m: &PatternMatch,
    view: &AnalysisView<'_>,
) -> Option<Fix> {
    let counts = count_template_metavar_occurrences(template);
    let mut walrus_vars: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (var_name, &count) in &counts {
        if count >= 2 {
            let cap = m.captures.get(var_name)?;
            let purity = view.oracle.expression_purity(view.file_id, cap.range);
            if purity != Purity::Pure {
                if spec.bind_impure_once {
                    walrus_vars.insert(var_name.clone());
                } else {
                    return None;
                }
            }
        }
    }

    let indent = view.tree.line_indent_at(m.range.start);
    let rendered =
        interpolate_replacement(template, &m.captures, &view.tree.source, indent, &walrus_vars);
    let title = spec
        .fix_title
        .clone()
        .unwrap_or_else(|| format!("Apply Refaster rewrite `{}`", spec.id));

    Some(Fix {
        title,
        edits: vec![Edit {
            range: m.range,
            replacement: rendered,
        }],
        required_imports: spec.imports.clone(),
        applicability: spec.applicability.into(),
    })
}

pub fn count_template_metavar_occurrences(template: &str) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let bytes = template.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"$$$") {
            let start = i + 3;
            let mut end = start;
            while end < bytes.len() && is_metavar_char(bytes[end]) {
                end += 1;
            }
            if end > start {
                let name = &template[start..end];
                if name != "INDENT" {
                    *counts.entry(name.to_string()).or_insert(0) += 1;
                }
                i = end;
                continue;
            }
        } else if bytes[i] == b'$' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && is_metavar_char(bytes[end]) {
                end += 1;
            }
            if end > start && bytes[start].is_ascii_uppercase() {
                let name = &template[start..end];
                if name != "INDENT" {
                    *counts.entry(name.to_string()).or_insert(0) += 1;
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    counts
}

fn interpolate_template(
    template: &str,
    captures: &HashMap<String, CapturedMetavar>,
    source: &str,
) -> String {
    interpolate_replacement(template, captures, source, "", &std::collections::HashSet::new())
}

fn interpolate_replacement(
    template: &str,
    captures: &HashMap<String, CapturedMetavar>,
    source: &str,
    indent: &str,
    walrus_vars: &std::collections::HashSet<String>,
) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut seen_walrus: std::collections::HashSet<String> = std::collections::HashSet::new();
    let bytes = template.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i..].starts_with(b"$$$") {
            let start = i + 3;
            let mut end = start;
            while end < bytes.len() && is_metavar_char(bytes[end]) {
                end += 1;
            }
            if end > start {
                let name = &template[start..end];
                if let Some(cap) = captures.get(name) {
                    let slice = cap.range.slice(source).unwrap_or("");
                    if slice.is_empty() {
                        trim_dangling_comma_before_empty_variadic(&mut out);
                    } else {
                        out.push_str(slice);
                    }
                    i = end;
                    continue;
                }
            }
        } else if bytes[i] == b'$' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && is_metavar_char(bytes[end]) {
                end += 1;
            }
            if end > start && bytes[start].is_ascii_uppercase() {
                let name = &template[start..end];
                if name == "INDENT" {
                    out.push_str(indent);
                    i = end;
                    continue;
                }
                if let Some(cap) = captures.get(name) {
                    let slice = cap.range.slice(source).unwrap_or("");
                    if walrus_vars.contains(name) {
                        let binding = format!("_riffty_{}", name.to_ascii_lowercase());
                        if seen_walrus.insert(name.to_string()) {
                            out.push_str(&format!("({binding} := {slice})"));
                        } else {
                            out.push_str(&binding);
                        }
                    } else {
                        out.push_str(slice);
                    }
                    i = end;
                    continue;
                }
            }
        }
        let ch = template[i..].chars().next().expect("valid UTF-8");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn trim_dangling_comma_before_empty_variadic(out: &mut String) {
    let trimmed_spaces = out.trim_end_matches(' ');
    if let Some(without_comma) = trimmed_spaces.strip_suffix(',') {
        out.truncate(without_comma.len());
    }
}

fn parse_expected_canonical_type(raw: &str) -> CanonicalType {
    match raw.trim() {
        "int" | "builtins.int" => CanonicalType::Primitive(PrimitiveKind::Int),
        "str" | "builtins.str" => CanonicalType::Primitive(PrimitiveKind::Str),
        "float" | "builtins.float" => CanonicalType::Primitive(PrimitiveKind::Float),
        "bool" | "builtins.bool" => CanonicalType::Primitive(PrimitiveKind::Bool),
        "bytes" | "builtins.bytes" => CanonicalType::Primitive(PrimitiveKind::Bytes),
        "None" | "NoneType" => CanonicalType::NoneType,
        other => {
            if let Some(bracket) = other.find('[')
                && other.ends_with(']')
            {
                let head = other[..bracket].trim();
                let inner = &other[bracket + 1..other.len() - 1];
                let args = inner
                    .split(',')
                    .map(parse_expected_canonical_type)
                    .collect();
                CanonicalType::Nominal {
                    fqname: head.to_string(),
                    args,
                }
            } else {
                CanonicalType::Nominal {
                    fqname: other.to_string(),
                    args: Vec::new(),
                }
            }
        }
    }
}

const fn is_metavar_char(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use riffty_adapter_ast_grep::AstGrepAdapter;
    use riffty_adapter_ruff_ast::{RuffAstAdapter, RuffImportAdapter};
    use riffty_adapter_ty::TyOracleAdapter;
    use riffty_core::{FileId, WorkspaceFile};
    use riffty_pipeline::{PipelineConfig, RifftyEngine};

    use super::*;

    #[test]
    fn r05_refaster_blocks_metavar_duplication_when_impure_and_allows_when_pure() {
        let yaml = r#"
rules:
  - id: RFT-YAML-001
    name: clamp-zero-floor
    severity: warning
    applicability: safe
    message: "Replace ternary zero-clamp on `$X` with `max(0, $X)`"
    pattern: "$X if $X > 0 else 0"
    guards:
      - kind: type_assignable_to
        target: "$X"
        expected_type: int
    replace: "max(0, $X)"
  - id: RFT-YAML-002
    name: square-via-mul
    severity: info
    applicability: safe
    message: "Square `$VAL` directly"
    pattern: "pow($VAL, 2)"
    guards:
      - kind: type_assignable_to
        target: "$VAL"
        expected_type: int
    replace: "($VAL * $VAL)"
"#;
        let rules = RefasterYamlRule::from_yaml_str(yaml).expect("valid YAML");

        let py_src = concat!(
            "def next_counter() -> int:\n",
            "    return 3\n",
            "\n",
            "def compute(n: int) -> tuple[int, int]:\n",
            "    pure_sq = pow(n, 2)\n",
            "    impure_sq = pow(next_counter(), 2)\n",
            "    return (pure_sq, impure_sq)\n",
        );

        let parser = RuffAstAdapter;
        let oracle = TyOracleAdapter::new();
        let import_mgr = RuffImportAdapter;
        let matcher = AstGrepAdapter;
        let engine = RifftyEngine {
            parser: &parser,
            oracle: &oracle,
            import_manager: &import_mgr,
            pattern_matcher: Some(&matcher),
            rules,
        };

        let results = engine
            .run_workspace(
                &[WorkspaceFile {
                    file_id: FileId(1),
                    path: PathBuf::from("sample.py"),
                    relative_path: "sample.py".to_string(),
                    source: py_src.to_string(),
                }],
                PipelineConfig {
                    apply_fixes: true,
                    allow_unsafe_fixes: false,
                    max_passes: 5,
                },
                None,
            )
            .expect("pipeline ok");

        let final_src = &results[0].final_source;
        // Pure variable `n` WAS rewritten (`(n * n)`)
        assert!(final_src.contains("pure_sq = (n * n)"), "got:\n{final_src}");
        // Impure call `next_counter()` was NEVER duplicated (`pow(next_counter(), 2)` preserved!)
        assert!(
            final_src.contains("impure_sq = pow(next_counter(), 2)"),
            "R05 violated! Impure call duplicated:\n{final_src}"
        );
    }

    #[test]
    fn r05_plus_bind_impure_once_lowers_duplicated_impure_call_to_walrus() {
        let yaml = r#"
rules:
  - id: RFT-POW-WALRUS
    name: pow-to-mul-walrus
    severity: warning
    applicability: safe
    bind_impure_once: true
    message: "Use `x * x` (with walrus binding for impure expressions) instead of `pow(x, 2)`."
    pattern: "pow($X, 2)"
    replace: "($X * $X)"
"#;
        let rules = RefasterYamlRule::from_yaml_str(yaml).expect("valid yaml");
        let source = concat!(
            "counter = 0\n",
            "def next_counter() -> int:\n",
            "    global counter\n",
            "    counter += 1\n",
            "    return counter\n\n",
            "def compute(n: int) -> tuple[int, int]:\n",
            "    pure_sq = pow(n, 2)\n",
            "    impure_sq = pow(next_counter(), 2)\n",
            "    return (pure_sq, impure_sq)\n",
        );

        let parser = RuffAstAdapter;
        let oracle = TyOracleAdapter::new();
        let import_mgr = RuffImportAdapter;
        let matcher = AstGrepAdapter;
        let engine = RifftyEngine {
            parser: &parser,
            oracle: &oracle,
            import_manager: &import_mgr,
            pattern_matcher: Some(&matcher),
            rules,
        };

        let results = engine
            .run_workspace(
                &[WorkspaceFile {
                    file_id: FileId(1),
                    path: PathBuf::from("app.py"),
                    relative_path: "app.py".to_string(),
                    source: source.to_string(),
                }],
                PipelineConfig {
                    apply_fixes: true,
                    allow_unsafe_fixes: false,
                    max_passes: 5,
                },
                None,
            )
            .expect("pipeline ok");

        let final_src = &results[0].final_source;
        assert!(final_src.contains("pure_sq = (n * n)"), "got:\n{final_src}");
        assert!(
            final_src.contains("impure_sq = ((_riffty_x := next_counter()) * _riffty_x)"),
            "Expected walrus binding for impure call, got:\n{final_src}"
        );
    }

    #[test]
    fn refaster_inside_subclass_of_guard_solves_bump_pydantic_issue_50_without_false_positive() {
        let yaml = r#"
rules:
  - id: RFT-PYD-205
    name: pydantic-v2-inner-config-orm-mode
    severity: warning
    applicability: safe
    message: "Pydantic v1 inner `class Config: orm_mode = True` is deprecated; use `model_config = ConfigDict(from_attributes=True)`."
    pattern: |
      class Config:
          orm_mode = True
    guards:
      - kind: inside_subclass_of
        expected_type: "pydantic.BaseModel"
    replace: "model_config = ConfigDict(from_attributes=True)"
    imports:
      - module: pydantic
        symbol: ConfigDict
"#;
        let rules = RefasterYamlRule::from_yaml_str(yaml).expect("valid YAML");

        let base_src = concat!(
            "from pydantic import BaseModel\n",
            "\n",
            "class DomainEntity(BaseModel):\n",
            "    id: int\n",
            "\n",
            "class Controller:\n",
            "    pass\n",
        );
        let app_src = concat!(
            "from base import Controller, DomainEntity\n",
            "\n",
            "class UserResponse(DomainEntity):\n",
            "    class Config:\n",
            "        orm_mode = True\n",
            "\n",
            "class MyController(Controller):\n",
            "    class Config:\n",
            "        orm_mode = True\n",
        );

        let parser = RuffAstAdapter;
        let oracle = TyOracleAdapter::new();
        let import_mgr = RuffImportAdapter;
        let matcher = AstGrepAdapter;
        let engine = RifftyEngine {
            parser: &parser,
            oracle: &oracle,
            import_manager: &import_mgr,
            pattern_matcher: Some(&matcher),
            rules,
        };

        let results = engine
            .run_workspace(
                &[
                    WorkspaceFile {
                        file_id: FileId(1),
                        path: PathBuf::from("base.py"),
                        relative_path: "base.py".to_string(),
                        source: base_src.to_string(),
                    },
                    WorkspaceFile {
                        file_id: FileId(2),
                        path: PathBuf::from("app.py"),
                        relative_path: "app.py".to_string(),
                        source: app_src.to_string(),
                    },
                ],
                PipelineConfig {
                    apply_fixes: true,
                    allow_unsafe_fixes: false,
                    max_passes: 5,
                },
                None,
            )
            .expect("pipeline ok");

        let final_app = &results[1].final_source;
        assert!(
            final_app.contains("class UserResponse(DomainEntity):\n    model_config = ConfigDict(from_attributes=True)"),
            "Expected BaseModel subclass inner Config to migrate:\n{final_app}"
        );
        assert!(
            final_app.contains("class MyController(Controller):\n    class Config:\n        orm_mode = True"),
            "Expected non-Pydantic Controller inner Config to remain untouched (#50):\n{final_app}"
        );
    }

    #[test]
    fn r17_not_overridden_below_downgrades_polymorphic_compat_and_auto_heals_closed_concrete() {
        let yaml = r#"
rules:
  - id: RFT-LC-38054-DICT
    name: langchain-pydantic-v2-model-dump
    severity: warning
    applicability: safe
    message: "Migrate `$MODEL.dict()` to `.model_dump()`."
    pattern: "$MODEL.dict($$$ARGS)"
    guards:
      - kind: type_assignable_to
        target: "$MODEL"
        expected_type: "pydantic.BaseModel"
      - kind: not_overridden_below
        target: "$MODEL"
        method: "dict"
        base_type: "pydantic.BaseModel"
        downgrade_to_unsafe: true
      - kind: not_inside_with
        callees:
          - "suppress_deprecation"
    replace: "$MODEL.model_dump($$$ARGS)"
"#;
        let rules = RefasterYamlRule::from_yaml_str(yaml).expect("valid YAML");

        let py_src = concat!(
            "from contextlib import contextmanager\n",
            "from pydantic import BaseModel\n",
            "\n",
            "@contextmanager\n",
            "def suppress_deprecation():\n",
            "    yield\n",
            "\n",
            "class BasePromptTemplate(BaseModel):\n",
            "    def _dict_for_compat(self, **kwargs):\n",
            "        return self.dict(**kwargs)\n",
            "\n",
            "class PromptTemplate(BasePromptTemplate):\n",
            "    def dict(self, **kwargs):\n",
            "        d = super().dict(**kwargs)\n",
            "        d['_type'] = 'prompt'\n",
            "        return d\n",
            "\n",
            "class ToolSpec(BaseModel):\n",
            "    name: str\n",
            "\n",
            "def export(spec: ToolSpec, prompt: BasePromptTemplate):\n",
            "    with suppress_deprecation():\n",
            "        legacy = spec.dict()\n",
            "    clean = spec.dict(exclude_none=True)\n",
            "    poly = prompt.dict()\n",
            "    return legacy, clean, poly\n",
        );

        let parser = RuffAstAdapter;
        let oracle = TyOracleAdapter::new();
        let import_mgr = RuffImportAdapter;
        let matcher = AstGrepAdapter;
        let engine = RifftyEngine {
            parser: &parser,
            oracle: &oracle,
            import_manager: &import_mgr,
            pattern_matcher: Some(&matcher),
            rules,
        };

        let results = engine
            .run_workspace(
                &[WorkspaceFile {
                    file_id: FileId(1),
                    path: PathBuf::from("prompts.py"),
                    relative_path: "prompts.py".to_string(),
                    source: py_src.to_string(),
                }],
                PipelineConfig {
                    apply_fixes: true,
                    allow_unsafe_fixes: false,
                    max_passes: 5,
                },
                None,
            )
            .expect("pipeline ok");

        let res = &results[0];
        // 1. Closed concrete `spec.dict(exclude_none=True)` WAS safely auto-rewritten!
        assert!(
            res.final_source
                .contains("clean = spec.model_dump(exclude_none=True)"),
            "Closed concrete BaseModel instance should auto-migrate:\n{}",
            res.final_source
        );
        // 2. Suppressed `with suppress_deprecation(): legacy = spec.dict()` was NOT rewritten or flagged!
        assert!(
            res.final_source.contains("legacy = spec.dict()"),
            "not_inside_with should suppress rewrite inside suppression context:\n{}",
            res.final_source
        );
        // 3. Polymorphic `self.dict(**kwargs)` and `prompt.dict()` were NOT rewritten under safe `--fix`,
        //    and remain as `Applicability::Unsafe` diagnostics!
        assert!(
            res.final_source.contains("return self.dict(**kwargs)")
                && res.final_source.contains("poly = prompt.dict()"),
            "Polymorphic dispatch sites must NOT be overwritten under safe --fix:\n{}",
            res.final_source
        );
        assert_eq!(res.remaining_diagnostics.len(), 2);
        for d in &res.remaining_diagnostics {
            assert_eq!(
                d.fix.as_ref().map(|f| f.applicability),
                Some(Applicability::Unsafe)
            );
        }
    }
}


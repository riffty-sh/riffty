use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use riffty_adapter_ast_grep::{AstGrepJavaAdapter, AstGrepJsTsAdapter};
use riffty_core::{
    AnalysisProvenance, AnalysisTier, AnalysisView, AnalyzePassRequest, AnalyzePassResponse,
    Applicability, CanonicalNode, CanonicalSyntaxTree, CanonicalType, CapturedMetavar, Diagnostic,
    Edit, FileId, Fix, ImportSpec, Language, LanguageAnalyzerPort, LineIndex, NodeId, NodeKind,
    NodeMeta, OverrideRisk, PatternMatch, Purity, RulePlugin, Severity, StoredNode, TextRange,
    TriState, TypeOraclePort, WorkspaceFile,
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
    #[serde(default = "default_language")]
    pub language: Language,
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

const fn default_language() -> Language {
    Language::Python
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

    pub fn language(&self) -> Language {
        self.spec.language
    }

    pub fn spec(&self) -> &RefasterRuleSpec {
        &self.spec
    }

    pub fn load_specs_from_str(yaml: &str) -> Result<Vec<RefasterRuleSpec>, String> {
        let parsed: YamlDocument =
            serde_yaml_ng::from_str(yaml).map_err(|e| format!("invalid Refaster YAML: {e}"))?;
        let specs = match parsed {
            YamlDocument::Envelope { rules } => rules,
            YamlDocument::List(rules) => rules,
        };
        for spec in &specs {
            if spec.id.trim().is_empty() {
                return Err("Refaster rule `id` must not be empty".to_string());
            }
            if spec.pattern.trim().is_empty() {
                return Err(format!(
                    "Refaster rule `{}` has an empty `pattern`",
                    spec.id
                ));
            }
        }
        Ok(specs)
    }

    pub fn load_specs_from_file(path: &Path) -> Result<Vec<RefasterRuleSpec>, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read Refaster YAML `{}`: {e}", path.display()))?;
        Self::load_specs_from_str(&content)
    }

    pub fn from_yaml_str(yaml: &str) -> Result<Vec<Box<dyn RulePlugin>>, String> {
        let specs = Self::load_specs_from_str(yaml)?;
        Ok(specs
            .into_iter()
            .map(|spec| Box::new(Self::new(spec)) as Box<dyn RulePlugin>)
            .collect())
    }

    pub fn from_yaml_str_for_language(
        yaml: &str,
        language: Language,
    ) -> Result<Vec<Box<dyn RulePlugin>>, String> {
        let specs = Self::load_specs_from_str(yaml)?;
        Ok(specs
            .into_iter()
            .filter(|spec| spec.language == language)
            .map(|spec| Box::new(Self::new(spec)) as Box<dyn RulePlugin>)
            .collect())
    }

    pub fn from_yaml_file(path: &Path) -> Result<Vec<Box<dyn RulePlugin>>, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read Refaster YAML `{}`: {e}", path.display()))?;
        Self::from_yaml_str(&content)
    }

    pub fn from_yaml_file_for_language(
        path: &Path,
        language: Language,
    ) -> Result<Vec<Box<dyn RulePlugin>>, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read Refaster YAML `{}`: {e}", path.display()))?;
        Self::from_yaml_str_for_language(&content, language)
    }

    pub fn jsts_rules_from_file(path: &Path) -> Result<Vec<Self>, String> {
        let specs = Self::load_specs_from_file(path)?;
        Ok(specs
            .into_iter()
            .filter(|spec| spec.language == Language::JsTs)
            .map(Self::new)
            .collect())
    }

    pub fn jsts_rules_from_str(yaml: &str) -> Result<Vec<Self>, String> {
        let specs = Self::load_specs_from_str(yaml)?;
        Ok(specs
            .into_iter()
            .filter(|spec| spec.language == Language::JsTs)
            .map(Self::new)
            .collect())
    }

    pub fn java_rules_from_file(path: &Path) -> Result<Vec<Self>, String> {
        let specs = Self::load_specs_from_file(path)?;
        Ok(specs
            .into_iter()
            .filter(|spec| spec.language == Language::Java)
            .map(Self::new)
            .collect())
    }

    pub fn java_rules_from_str(yaml: &str) -> Result<Vec<Self>, String> {
        let specs = Self::load_specs_from_str(yaml)?;
        Ok(specs
            .into_iter()
            .filter(|spec| spec.language == Language::Java)
            .map(Self::new)
            .collect())
    }
}

/// Evaluates `language: java` Refaster YAML rules against a single Java source file
/// using `AstGrepJavaAdapter` (`tree-sitter-java`).
pub fn evaluate_java_refaster_rules(
    file_id: FileId,
    relative_path: &str,
    source: &str,
    rules: &[RefasterYamlRule],
) -> Vec<Diagnostic> {
    if rules.is_empty() {
        return Vec::new();
    }
    let matcher = AstGrepJavaAdapter;
    let oracle = NoOpTypeOracle;
    let stored_root = StoredNode {
        id: NodeId(0),
        kind: NodeKind::Module,
        range: TextRange::new(0, source.len() as u32),
        parent: None,
        children: Vec::new(),
        meta: NodeMeta::None,
    };
    let root_node = CanonicalNode {
        id: NodeId(0),
        kind: NodeKind::Module,
        range: TextRange::new(0, source.len() as u32),
        parent: None,
        raw_text: source,
    };
    let tree = CanonicalSyntaxTree::new(file_id, source.to_string(), vec![stored_root], Vec::new());
    let view = AnalysisView {
        file_id,
        file_path: relative_path,
        tree: &tree,
        oracle: &oracle,
        pattern_matcher: Some(&matcher),
    };
    let line_index = LineIndex::new(source);
    let mut out = Vec::new();
    for rule in rules {
        if rule.language() != Language::Java {
            continue;
        }
        for d in rule.evaluate(&root_node, &view) {
            let line = line_index.line_of(d.range.start);
            let line_text = line_index.line_slice(source, line);
            if is_jsts_line_suppressed(line_text, &d.rule_id) {
                continue;
            }
            out.push(d);
        }
    }
    out
}

/// `LanguageAnalyzerPort` executing custom `language: java` Refaster YAML rule packs.
pub struct JavaRefasterAnalyzer {
    pub rules: Vec<RefasterYamlRule>,
}

impl LanguageAnalyzerPort for JavaRefasterAnalyzer {
    fn language(&self) -> Language {
        Language::Java
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["java"]
    }

    fn max_fixpoint_passes(&self) -> usize {
        3
    }

    fn analyze_pass(&self, req: &AnalyzePassRequest<'_>) -> Result<AnalyzePassResponse, String> {
        let mut diagnostics = Vec::new();
        let mut files_analyzed = 0usize;
        for wf in req.files {
            if let Some(scopes) = req.scoped_ranges
                && !scopes.contains_key(&wf.file_id)
            {
                continue;
            }
            files_analyzed += 1;
            let mut diags = evaluate_java_refaster_rules(
                wf.file_id,
                &wf.relative_path,
                &wf.source,
                &self.rules,
            );
            if let Some(scopes) = req.scoped_ranges {
                let allowed = scopes.get(&wf.file_id).map(Vec::as_slice).unwrap_or(&[]);
                diags.retain(|d| allowed.iter().any(|sp| sp.intersects(d.range)));
            }
            diagnostics.extend(diags);
        }
        Ok(AnalyzePassResponse {
            diagnostics,
            provenance: AnalysisProvenance {
                language: Language::Java,
                tier: AnalysisTier::Tier1Syntactic,
                files_analyzed,
                runtime_detail: Some(format!(
                    "tree-sitter-java refaster ({} custom YAML rule(s))",
                    self.rules.len()
                )),
                degraded_reason: None,
                remediation_hint: None,
            },
        })
    }
}

struct NoOpTypeOracle;

impl TypeOraclePort for NoOpTypeOracle {
    fn index_workspace(&self, _files: &[WorkspaceFile]) {}
    fn type_of(&self, _file: FileId, _range: TextRange) -> CanonicalType {
        CanonicalType::Unknown
    }
    fn resolve_fqname(&self, _file: FileId, _range: TextRange) -> Option<String> {
        None
    }
    fn can_be_none(&self, _file: FileId, _range: TextRange) -> TriState {
        TriState::Unknown
    }
    fn expression_purity(&self, _file: FileId, _range: TextRange) -> Purity {
        Purity::SideEffecting
    }
    fn is_subtype(&self, _sub: &CanonicalType, _sup: &CanonicalType) -> TriState {
        TriState::Unknown
    }
    fn reverse_dependents(
        &self,
        _file: FileId,
        _changed: &[TextRange],
    ) -> Vec<(FileId, TextRange)> {
        Vec::new()
    }
}

/// Evaluates `language: jsts` Refaster YAML rules against a single JS/TS source file using
/// `AstGrepJsTsAdapter` (`TypeScript` / `Tsx` / `JavaScript` grammar inferred from extension).
pub fn evaluate_jsts_refaster_rules(
    file_id: FileId,
    file_path: &Path,
    relative_path: &str,
    source: &str,
    rules: &[RefasterYamlRule],
) -> Vec<Diagnostic> {
    if rules.is_empty() {
        return Vec::new();
    }
    let matcher = AstGrepJsTsAdapter::for_path(file_path);
    let oracle = NoOpTypeOracle;
    let stored_root = StoredNode {
        id: NodeId(0),
        kind: NodeKind::Module,
        range: TextRange::new(0, source.len() as u32),
        parent: None,
        children: Vec::new(),
        meta: NodeMeta::None,
    };
    let root_node = CanonicalNode {
        id: NodeId(0),
        kind: NodeKind::Module,
        range: TextRange::new(0, source.len() as u32),
        parent: None,
        raw_text: source,
    };
    let tree = CanonicalSyntaxTree::new(file_id, source.to_string(), vec![stored_root], Vec::new());
    let view = AnalysisView {
        file_id,
        file_path: relative_path,
        tree: &tree,
        oracle: &oracle,
        pattern_matcher: Some(&matcher),
    };
    let line_index = LineIndex::new(source);
    let mut out = Vec::new();
    for rule in rules {
        if rule.language() != Language::JsTs {
            continue;
        }
        let diags = rule.evaluate(&root_node, &view);
        for d in diags {
            let line = line_index.line_of(d.range.start);
            let line_text = line_index.line_slice(source, line);
            if is_jsts_line_suppressed(line_text, &d.rule_id) {
                continue;
            }
            if line > 1 {
                let prev_text = line_index.line_slice(source, line - 1);
                if prev_text.trim_start().starts_with("//")
                    && is_jsts_line_suppressed(prev_text, &d.rule_id)
                {
                    continue;
                }
            }
            out.push(d);
        }
    }
    out
}

fn is_jsts_line_suppressed(line_text: &str, rule_id: &str) -> bool {
    let Some(comment_idx) = line_text.find("//") else {
        return false;
    };
    let comment = line_text[comment_idx + 2..].trim();
    let Some(rest) = comment.strip_prefix("riffty:") else {
        return false;
    };
    let rest = rest.trim();
    if let Some(after_ignore) = rest.strip_prefix("ignore") {
        let after_ignore = after_ignore.trim();
        if let Some(inner) = after_ignore.strip_prefix('[')
            && let Some(end) = inner.find(']')
        {
            return inner[..end]
                .split(',')
                .any(|tok| tok.trim().eq_ignore_ascii_case(rule_id) || tok.trim() == "*");
        }
        if after_ignore.is_empty() || after_ignore.starts_with("--") {
            return true;
        }
    }
    false
}

/// Minimal Tier 1 JS/TS analyzer for Milestone 0 that evaluates `language: jsts` Refaster YAML rules.
#[derive(Default)]
pub struct JsTsRefasterAnalyzer {
    pub rules: Vec<RefasterYamlRule>,
}

impl LanguageAnalyzerPort for JsTsRefasterAnalyzer {
    fn language(&self) -> Language {
        Language::JsTs
    }

    fn extensions(&self) -> &'static [&'static str] {
        Language::JSTS_EXTENSIONS
    }

    fn analyze_pass(&self, req: &AnalyzePassRequest<'_>) -> Result<AnalyzePassResponse, String> {
        let mut loaded_rules: Vec<RefasterYamlRule> = Vec::new();
        for rp in req.rules_paths {
            loaded_rules.extend(RefasterYamlRule::jsts_rules_from_file(rp)?);
        }
        let active_rules: &[RefasterYamlRule] = if self.rules.is_empty() {
            &loaded_rules
        } else {
            &self.rules
        };

        let mut diagnostics = Vec::new();
        let mut files_analyzed = 0usize;
        for wf in req.files {
            if let Some(scopes) = req.scoped_ranges
                && !scopes.contains_key(&wf.file_id)
            {
                continue;
            }
            files_analyzed += 1;
            let mut diags = evaluate_jsts_refaster_rules(
                wf.file_id,
                &wf.path,
                &wf.relative_path,
                &wf.source,
                active_rules,
            );
            if let Some(scopes) = req.scoped_ranges {
                let allowed = scopes.get(&wf.file_id).map(Vec::as_slice).unwrap_or(&[]);
                diags.retain(|d| allowed.iter().any(|sp| sp.intersects(d.range)));
            }
            diagnostics.extend(diags);
        }

        Ok(AnalyzePassResponse {
            diagnostics,
            provenance: AnalysisProvenance {
                language: Language::JsTs,
                tier: AnalysisTier::Tier1Syntactic,
                files_analyzed,
                runtime_detail: Some("ast-grep (jsts refaster)".to_string()),
                degraded_reason: Some(
                    "Tier 2 JS/TS semantic checker not active; syntactic Refaster rules only"
                        .to_string(),
                ),
                remediation_hint: Some(
                    "Install node_modules (`npm ci --ignore-scripts`) and enable Tier 2 tsrs"
                        .to_string(),
                ),
            },
        })
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
                let risk = view
                    .oracle
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
                proof_map.insert("not_inside_with".to_string(), serde_json::json!(callees));
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
                    if resolved_fqn.as_deref().and_then(|f| f.rsplit('.').next())
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
        if matches!(
            node.kind,
            NodeKind::FunctionDef | NodeKind::AsyncFunctionDef
        ) && node.range.contains_range(span)
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
    let rendered = interpolate_replacement(
        template,
        &m.captures,
        &view.tree.source,
        indent,
        &walrus_vars,
    );
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
    interpolate_replacement(
        template,
        captures,
        source,
        "",
        &std::collections::HashSet::new(),
    )
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
    CanonicalType::parse_type_expr(raw)
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
            final_app.contains(
                "class MyController(Controller):\n    class Config:\n        orm_mode = True"
            ),
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

    #[test]
    fn issue_6_refaster_type_assignable_to_handles_variadic_tuples_and_covariant_sequence_abcs() {
        let yaml = r#"
rules:
  - id: RFT-YAML-TUPLE-VAR
    name: join-variadic-str-tuple
    severity: info
    applicability: safe
    message: "Join variadic or fixed str tuple `$PARTS` directly."
    pattern: "consume_parts($PARTS)"
    guards:
      - kind: type_assignable_to
        target: "$PARTS"
        expected_type: "tuple[str, ...]"
    replace: "':'.join($PARTS)"
  - id: RFT-YAML-SEQ-STR
    name: join-str-sequence
    severity: info
    applicability: safe
    message: "Normalize `Sequence[str]` `$SEQ`."
    pattern: "consume_seq($SEQ)"
    guards:
      - kind: type_assignable_to
        target: "$SEQ"
        expected_type: "Sequence[str]"
    replace: "','.join($SEQ)"
"#;
        let rules = RefasterYamlRule::from_yaml_str(yaml).expect("valid YAML");

        let py_src = concat!(
            "def consume_parts(x: object) -> str:\n",
            "    return ''\n",
            "\n",
            "def consume_seq(x: object) -> str:\n",
            "    return ''\n",
            "\n",
            "def run(\n",
            "    annotated_var: tuple[str, ...],\n",
            "    fixed_pair: tuple[str, str],\n",
            "    str_list: list[str],\n",
            "    int_tuple: tuple[int, ...],\n",
            "    line: str,\n",
            ") -> None:\n",
            "    inferred_var = tuple(line.split(':'))\n",
            "    a = consume_parts(annotated_var)\n",
            "    b = consume_parts(inferred_var)\n",
            "    c = consume_parts(fixed_pair)\n",
            "    d = consume_parts(int_tuple)\n",
            "    e = consume_seq(fixed_pair)\n",
            "    f = consume_seq(str_list)\n",
            "    g = consume_seq(int_tuple)\n",
            "    _ = (a, b, c, d, e, f, g)\n",
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

        let out = &results[0].final_source;
        assert!(out.contains("a = ':'.join(annotated_var)"), "got:\n{out}");
        assert!(out.contains("b = ':'.join(inferred_var)"), "got:\n{out}");
        assert!(out.contains("c = ':'.join(fixed_pair)"), "got:\n{out}");
        assert!(out.contains("d = consume_parts(int_tuple)"), "got:\n{out}");
        assert!(out.contains("e = ','.join(fixed_pair)"), "got:\n{out}");
        assert!(out.contains("f = ','.join(str_list)"), "got:\n{out}");
        assert!(out.contains("g = consume_seq(int_tuple)"), "got:\n{out}");
    }

    #[test]
    fn req_m0_04_refaster_accepts_language_jsts_and_fixes_ts_tsx_js() {
        let yaml = r#"
rules:
  - id: JSTS-REF-001
    name: nan-equality-refaster
    language: jsts
    severity: error
    applicability: safe
    message: "Use Number.isNaN($X) instead of $X === NaN"
    pattern: "$X === NaN"
    replace: "Number.isNaN($X)"
"#;
        let rules = RefasterYamlRule::jsts_rules_from_str(yaml).expect("parse jsts yaml");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].language(), Language::JsTs);

        let mut registry = riffty_pipeline::AnalyzerRegistry::new();
        registry.register(JsTsRefasterAnalyzer { rules });
        let orch = riffty_pipeline::WorkspaceOrchestrator {
            registry: &registry,
        };
        let files = vec![
            WorkspaceFile {
                file_id: FileId(1),
                path: PathBuf::from("src/math.ts"),
                relative_path: "src/math.ts".to_string(),
                source: "export function bad(x: number): boolean { return x === NaN; }\n"
                    .to_string(),
            },
            WorkspaceFile {
                file_id: FileId(2),
                path: PathBuf::from("src/View.tsx"),
                relative_path: "src/View.tsx".to_string(),
                source: "export const C = ({ n }: { n: number }) => <span>{n === NaN}</span>;\n"
                    .to_string(),
            },
        ];

        let out = orch
            .run(
                Path::new("."),
                &files,
                &[],
                PipelineConfig {
                    apply_fixes: true,
                    allow_unsafe_fixes: false,
                    max_passes: 5,
                },
                None,
            )
            .expect("orchestrator ok");

        assert_eq!(out.provenance.len(), 1);
        assert_eq!(out.provenance[0].language, Language::JsTs);
        assert!(
            out.files[0]
                .final_source
                .contains("return Number.isNaN(x);")
        );
        assert!(
            out.files[1]
                .final_source
                .contains("<span>{Number.isNaN(n)}</span>")
        );
    }
}

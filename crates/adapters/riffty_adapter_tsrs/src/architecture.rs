use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, CallExpression, ExportAllDeclaration, ExportFromDeclaration, Expression,
    ImportDeclaration, ImportExpression,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use riffty_adapter_jsts_fast::source_type_for_path;
use riffty_core::{
    Applicability, Diagnostic, Fix, LineIndex, Severity, TextRange, WorkspaceFile,
};
use serde::{Deserialize, Serialize};

fn default_arch_rule_id() -> String {
    "JSTS-ARCH-FORBID".to_string()
}

/// Declarative JS/TS module/import boundary rule parsed from `[[jsts.architecture.forbid]]` in `riffty.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsTsArchitectureForbidRule {
    #[serde(default = "default_arch_rule_id")]
    pub id: String,
    /// Glob matching source files relative to workspace root (e.g., `"src/domain/**"`).
    pub from: String,
    /// Glob list matching forbidden import specifiers or resolved workspace paths
    /// (e.g., `["next/**", "@prisma/client", "src/infra/**"]`).
    pub target: Vec<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Parsed `compilerOptions.baseUrl` + `compilerOptions.paths` from `tsconfig.json`.
#[derive(Debug, Clone, Default)]
pub struct TsPathResolver {
    pub base_url: Option<String>,
    /// Pattern (e.g. `"@/*"`) -> list of target templates (e.g. `["src/*"]`).
    pub paths: BTreeMap<String, Vec<String>>,
}

impl TsPathResolver {
    pub fn from_tsconfig(tsconfig_path: &Path) -> Self {
        let Ok(raw) = std::fs::read_to_string(tsconfig_path) else {
            return Self::default();
        };
        let stripped = strip_jsonc_comments_and_trailing_commas(&raw);
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&stripped) else {
            return Self::default();
        };
        let Some(opts) = val.get("compilerOptions") else {
            return Self::default();
        };
        let base_url = opts
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .map(normalize_rel_path);
        let mut paths = BTreeMap::new();
        if let Some(obj) = opts.get("paths").and_then(|v| v.as_object()) {
            for (k, v) in obj {
                if let Some(arr) = v.as_array() {
                    let targets: Vec<String> = arr
                        .iter()
                        .filter_map(|item| item.as_str().map(normalize_rel_path))
                        .collect();
                    if !targets.is_empty() {
                        paths.insert(k.clone(), targets);
                    }
                }
            }
        }
        Self { base_url, paths }
    }

    /// Resolves a module specifier (static import, re-export, dynamic import, or require)
    /// into zero or more candidate workspace-relative paths (handling relative imports & `tsconfig` `paths`).
    pub fn resolve_specifier(&self, from_rel_path: &str, raw_spec: &str) -> Vec<String> {
        let mut candidates = Vec::new();
        let spec = raw_spec.trim();
        if spec.is_empty() {
            return candidates;
        }

        // 1. Relative import (`./...` or `../...`)
        if spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == ".." {
            let parent = Path::new(from_rel_path)
                .parent()
                .unwrap_or_else(|| Path::new(""));
            let joined = normalize_lexically(&parent.join(spec));
            candidates.push(joined);
            return candidates;
        }

        // 2. tsconfig.json `paths` alias matching (e.g. `@/*` -> `src/*`)
        for (pattern, targets) in &self.paths {
            if let Some(star_idx) = pattern.find('*') {
                let prefix = &pattern[..star_idx];
                let suffix = &pattern[star_idx + 1..];
                if spec.starts_with(prefix)
                    && spec.ends_with(suffix)
                    && spec.len() >= prefix.len() + suffix.len()
                {
                    let matched = &spec[prefix.len()..spec.len() - suffix.len()];
                    for tpl in targets {
                        let replaced = tpl.replace('*', matched);
                        let resolved = if let Some(ref base) = self.base_url {
                            if base.is_empty() || base == "." {
                                normalize_rel_path(&replaced)
                            } else {
                                normalize_lexically(&Path::new(base).join(&replaced))
                            }
                        } else {
                            normalize_rel_path(&replaced)
                        };
                        candidates.push(resolved);
                    }
                }
            } else if pattern == spec {
                for tpl in targets {
                    let resolved = if let Some(ref base) = self.base_url {
                        if base.is_empty() || base == "." {
                            normalize_rel_path(tpl)
                        } else {
                            normalize_lexically(&Path::new(base).join(tpl))
                        }
                    } else {
                        normalize_rel_path(tpl)
                    };
                    candidates.push(resolved);
                }
            }
        }

        // 3. baseUrl fallback for bare internal paths
        if candidates.is_empty()
            && let Some(ref base) = self.base_url
            && !base.is_empty()
            && base != "."
        {
            candidates.push(normalize_lexically(&Path::new(base).join(spec)));
        }

        candidates
    }
}

struct ExtractedImport {
    specifier: String,
    kind: &'static str,
    range: TextRange,
}

struct ImportExtractor {
    imports: Vec<ExtractedImport>,
}

impl<'a> Visit<'a> for ImportExtractor {
    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        if let Some(spec) = it.source.value.as_str() {
            self.imports.push(ExtractedImport {
                specifier: spec.to_string(),
                kind: "import",
                range: TextRange::new(it.span.start, it.span.end),
            });
        }
        walk::walk_import_declaration(self, it);
    }

    fn visit_export_from_declaration(&mut self, it: &ExportFromDeclaration<'a>) {
        if let Some(spec) = it.source.value.as_str() {
            self.imports.push(ExtractedImport {
                specifier: spec.to_string(),
                kind: "export-from",
                range: TextRange::new(it.span.start, it.span.end),
            });
        }
        walk::walk_export_from_declaration(self, it);
    }

    fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
        if let Some(spec) = it.source.value.as_str() {
            self.imports.push(ExtractedImport {
                specifier: spec.to_string(),
                kind: "export-all",
                range: TextRange::new(it.span.start, it.span.end),
            });
        }
        walk::walk_export_all_declaration(self, it);
    }

    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        if let Expression::StringLiteral(lit) = it.source.without_parentheses()
            && let Some(spec) = lit.value.as_str()
        {
            self.imports.push(ExtractedImport {
                specifier: spec.to_string(),
                kind: "dynamic-import",
                range: TextRange::new(it.span.start, it.span.end),
            });
        }
        walk::walk_import_expression(self, it);
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if matches!(
            it.callee.without_parentheses(),
            Expression::Identifier(id) if id.name.as_str() == "require"
        ) && let Some(first) = it.arguments.first().and_then(Argument::as_expression)
            && let Expression::StringLiteral(lit) = first.without_parentheses()
            && let Some(spec) = lit.value.as_str()
        {
            self.imports.push(ExtractedImport {
                specifier: spec.to_string(),
                kind: "require",
                range: TextRange::new(it.span.start, it.span.end),
            });
        }
        walk::walk_call_expression(self, it);
    }
}

/// Evaluates all `[[jsts.architecture.forbid]]` rules on a single JS/TS file across
/// static `import`, `export ... from`, dynamic `import()`, and `require()`.
pub fn check_jsts_architecture_rules(
    wf: &WorkspaceFile,
    rules: &[JsTsArchitectureForbidRule],
    path_resolver: &TsPathResolver,
) -> Vec<Diagnostic> {
    if rules.is_empty() {
        return Vec::new();
    }

    let rel_norm = normalize_rel_path(&wf.relative_path);
    let applicable_rules: Vec<&JsTsArchitectureForbidRule> = rules
        .iter()
        .filter(|r| glob_matches(&r.from, &rel_norm))
        .collect();
    if applicable_rules.is_empty() {
        return Vec::new();
    }

    let allocator = Allocator::default();
    let source_type = source_type_for_path(&wf.path);
    let parsed = Parser::new(&allocator, &wf.source, source_type).parse();
    let mut extractor = ImportExtractor {
        imports: Vec::new(),
    };
    extractor.visit_program(&parsed.program);

    let line_index = LineIndex::new(&wf.source);
    let mut diagnostics = Vec::new();

    for imp in extractor.imports {
        let resolved_candidates = path_resolver.resolve_specifier(&rel_norm, &imp.specifier);
        for rule in &applicable_rules {
            let mut matched_target: Option<(&str, String)> = None;
            for target_glob in &rule.target {
                if glob_matches(target_glob, &imp.specifier) {
                    matched_target = Some((target_glob.as_str(), imp.specifier.clone()));
                    break;
                }
                for cand in &resolved_candidates {
                    if glob_matches(target_glob, cand) {
                        matched_target = Some((target_glob.as_str(), cand.clone()));
                        break;
                    }
                }
                if matched_target.is_some() {
                    break;
                }
            }

            if let Some((forbidden_glob, resolved_spec)) = matched_target {
                if is_suppressed_at(&wf.source, &line_index, imp.range, &rule.id) {
                    continue;
                }
                let msg = rule.message.clone().unwrap_or_else(|| {
                    format!(
                        "Architecture boundary violation (`{}`): `{}` must not import `{}` (resolved: `{}`; matches forbidden `{}`).",
                        rule.id, rel_norm, imp.specifier, resolved_spec, forbidden_glob
                    )
                });
                diagnostics.push(Diagnostic {
                    rule_id: rule.id.clone(),
                    rule_name: "architecture-boundary-violation".to_string(),
                    severity: Severity::Error,
                    file_id: wf.file_id,
                    range: imp.range,
                    message: msg,
                    type_proof: Some(serde_json::json!({
                        "from_file": rel_norm,
                        "from_pattern": rule.from,
                        "import_kind": imp.kind,
                        "raw_specifier": imp.specifier,
                        "resolved_specifier": resolved_spec,
                        "forbidden_target": forbidden_glob,
                    })),
                    fix: Some(Fix {
                        title: format!(
                            "Remove or invert forbidden import `{}` from `{}`",
                            imp.specifier, rel_norm
                        ),
                        applicability: Applicability::ManualOnly,
                        edits: Vec::new(),
                        required_imports: Vec::new(),
                    }),
                });
            }
        }
    }

    diagnostics
}

fn is_suppressed_at(source: &str, line_index: &LineIndex, range: TextRange, rule_id: &str) -> bool {
    let line = line_index.line_of(range.start);
    if crate::is_jsts_line_suppressed(line_index.line_slice(source, line), rule_id) {
        return true;
    }
    if line > 1 {
        let prev = line_index.line_slice(source, line - 1);
        if prev.trim_start().starts_with("//") && crate::is_jsts_line_suppressed(prev, rule_id) {
            return true;
        }
    }
    false
}

/// Glob matcher supporting `**` (zero or more path segments), `*` (within segment), and `?`.
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let pat = normalize_rel_path(pattern);
    let txt = normalize_rel_path(text);
    glob_match_bytes(pat.as_bytes(), txt.as_bytes())
}

fn glob_match_bytes(p: &[u8], t: &[u8]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    // Handle `**/` (matches zero or more directories) or trailing `**`
    if p.starts_with(b"**/") {
        let rest = &p[3..];
        if glob_match_bytes(rest, t) {
            return true;
        }
        if !t.is_empty() {
            return glob_match_bytes(p, &t[1..]);
        }
        return false;
    }
    if p == b"**" {
        return true;
    }
    if p.starts_with(b"**") {
        let rest = &p[2..];
        if glob_match_bytes(rest, t) {
            return true;
        }
        if !t.is_empty() {
            return glob_match_bytes(p, &t[1..]);
        }
        return false;
    }
    if p[0] == b'*' {
        if glob_match_bytes(&p[1..], t) {
            return true;
        }
        if !t.is_empty() && t[0] != b'/' {
            return glob_match_bytes(p, &t[1..]);
        }
        return false;
    }
    if p[0] == b'?' {
        return !t.is_empty() && t[0] != b'/' && glob_match_bytes(&p[1..], &t[1..]);
    }
    if !t.is_empty() && p[0] == t[0] {
        return glob_match_bytes(&p[1..], &t[1..]);
    }
    false
}

fn normalize_rel_path(raw: &str) -> String {
    let s = raw.replace('\\', "/");
    s.trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

fn normalize_lexically(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(os) => {
                parts.push(os.to_string_lossy().to_string());
            }
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    PathBuf::from(parts.join("/"))
        .to_string_lossy()
        .replace('\\', "/")
}

/// Minimal JSONC comment & trailing-comma stripper for `tsconfig.json`.
fn strip_jsonc_comments_and_trailing_commas(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            out.push(b as char);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            out.push('"');
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        out.push(b as char);
        i += 1;
    }

    // Remove simple trailing commas before `}` or `]`
    let ob = out.as_bytes();
    let mut cleaned = String::with_capacity(out.len());
    let mut j = 0usize;
    in_string = false;
    escaped = false;
    while j < ob.len() {
        let b = ob[j];
        if in_string {
            cleaned.push(b as char);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            j += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            cleaned.push('"');
            j += 1;
            continue;
        }
        if b == b',' {
            let mut k = j + 1;
            while k < ob.len() && ob[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < ob.len() && matches!(ob[k], b'}' | b']') {
                j += 1;
                continue;
            }
        }
        cleaned.push(b as char);
        j += 1;
    }
    cleaned
}

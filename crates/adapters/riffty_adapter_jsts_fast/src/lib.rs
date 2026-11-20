pub mod imports;

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, BinaryExpression, BinaryOperator, CallExpression,
    Expression, ForInStatement, ForOfStatement, ForStatement, Function, ArrowFunctionExpression,
    NewExpression, ObjectPropertyKind, PropertyKey, ReturnStatement, TryStatement,
    WhileStatement, DoWhileStatement,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_semantic::{Scoping, ScopeId, SemanticBuilder};
use oxc_span::{GetSpan, SourceType};
use riffty_adapter_refaster::{RefasterYamlRule, evaluate_jsts_refaster_rules};
use riffty_core::{
    AnalysisProvenance, AnalysisTier, AnalyzePassRequest, AnalyzePassResponse, Applicability,
    Diagnostic, Edit, FileId, Fix, ImportManagerPort, Language, LanguageAnalyzerPort, LineIndex,
    Severity, TextRange,
};

pub use imports::OxcImportAdapter;

#[derive(Debug, Clone, Copy)]
pub struct JsTsRuleDoc {
    pub id: &'static str,
    pub name: &'static str,
    pub language: Language,
    pub severity: Severity,
    pub default_applicability: &'static str,
    pub summary: &'static str,
    pub rationale: &'static str,
    pub bad_example: &'static str,
    pub good_example: &'static str,
}

/// Documentation metadata for all built-in Tier 1 JS/TS rules (used by `riffty explain` and SARIF).
pub fn jsts_fast_rule_catalog() -> &'static [JsTsRuleDoc] {
    &[
        JsTsRuleDoc {
            id: "JSTS-NAN-EQ-001",
            name: "nan-equality-comparison",
            language: Language::JsTs,
            severity: Severity::Error,
            default_applicability: "Safe",
            summary: "Comparison against `NaN` (`x === NaN` / `x !== NaN`) always evaluates to `false` / `true`.",
            rationale: "Per IEEE-754 and ECMAScript semantics, `NaN` is never equal to itself. Comparing `x === NaN` silently disables NaN guards and corrupts financial or statistical calculations. Use `Number.isNaN(x)`.",
            bad_example: "if (total === NaN) { return 0; }",
            good_example: "if (Number.isNaN(total)) { return 0; }",
        },
        JsTsRuleDoc {
            id: "JSTS-CLONE-001",
            name: "json-roundtrip-deep-clone",
            language: Language::JsTs,
            severity: Severity::Warning,
            default_applicability: "Safe",
            summary: "Replace `JSON.parse(JSON.stringify(x))` with `structuredClone(x)`.",
            rationale: "JSON round-tripping allocates two full serialized/parsed representations (high CPU/GC cost) and silently drops `Date`, `Map`, `Set`, `RegExp`, `undefined`, and `BigInt` values. `structuredClone(x)` is native, faster, and preserves rich JS types.",
            bad_example: "const copy = JSON.parse(JSON.stringify(payload));",
            good_example: "const copy = structuredClone(payload);",
        },
        JsTsRuleDoc {
            id: "JSTS-REGEX-LOOP-001",
            name: "regexp-allocated-in-loop",
            language: Language::JsTs,
            severity: Severity::Warning,
            default_applicability: "Safe (stateless literals only; skips /g and /y)",
            summary: "Avoid allocating constant `new RegExp(\"...\")` inside loops or hot array callbacks.",
            rationale: "Compiling a constant regular expression on every loop iteration wastes CPU and heap allocations. Hoist stateless regexes to a module/outer `const`. Stateful `/g` and `/y` regexes are intentionally skipped to prevent `lastIndex` bugs.",
            bad_example: "for (const line of lines) {\n  if (new RegExp(\"^[0-9]+$\", \"i\").test(line)) { count++; }\n}",
            good_example: "const __RIFFTY_RE_1 = new RegExp(\"^[0-9]+$\", \"i\");\nfor (const line of lines) {\n  if (__RIFFTY_RE_1.test(line)) { count++; }\n}",
        },
        JsTsRuleDoc {
            id: "JSTS-INTL-LOOP-001",
            name: "intl-formatter-allocated-in-loop",
            language: Language::JsTs,
            severity: Severity::Warning,
            default_applicability: "Safe",
            summary: "Avoid allocating `new Intl.DateTimeFormat(...)` or `new Intl.NumberFormat(...)` inside loops.",
            rationale: "Constructing `Intl.DateTimeFormat` or `Intl.NumberFormat` parses ICU locale tables on every iteration (10x–100x slower than reusing a formatter). Hoist constant formatters outside the loop.",
            bad_example: "const labels = dates.map((d) => new Intl.DateTimeFormat(\"en-US\").format(d));",
            good_example: "const __RIFFTY_INTL_1 = new Intl.DateTimeFormat(\"en-US\");\nconst labels = dates.map((d) => __RIFFTY_INTL_1.format(d));",
        },
        JsTsRuleDoc {
            id: "JSTS-PROMISE-EXEC-001",
            name: "async-promise-executor",
            language: Language::JsTs,
            severity: Severity::Error,
            default_applicability: "ManualOnly",
            summary: "Avoid passing an `async` executor function to `new Promise(async (resolve, reject) => ...)`.",
            rationale: "Errors thrown inside an `async` Promise executor are NOT automatically caught by the outer `Promise` constructor, leading to unhandled rejections or hanging promises that never settle.",
            bad_example: "const p = new Promise(async (resolve, reject) => {\n  const data = await fetchData();\n  resolve(data);\n});",
            good_example: "async function loadData() {\n  return fetchData();\n}",
        },
        JsTsRuleDoc {
            id: "JSTS-DUP-AWAIT-001",
            name: "redundant-return-await",
            language: Language::JsTs,
            severity: Severity::Warning,
            default_applicability: "Safe",
            summary: "Remove redundant `return await` outside `try` blocks in `async` functions.",
            rationale: "Outside a `try { ... }` block, `return await expr` adds an extra microtask tick without catching rejections. Returning `expr` directly preserves async semantics with lower overhead.",
            bad_example: "async function getUser(id: string) {\n  return await fetchUser(id);\n}",
            good_example: "async function getUser(id: string) {\n  return fetchUser(id);\n}",
        },
    ]
}

/// Infers `oxc_span::SourceType` deterministically across all 8 JS/TS extensions.
pub fn source_type_for_path(path: &Path) -> SourceType {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("ts")
        .to_ascii_lowercase();
    match ext.as_str() {
        "ts" => SourceType::ts(),
        "tsx" => SourceType::tsx(),
        "mts" => SourceType::ts().with_module(true),
        "cts" => SourceType::ts().with_commonjs(true),
        "jsx" => SourceType::jsx(),
        "mjs" => SourceType::mjs(),
        "cjs" => SourceType::cjs(),
        "js" => SourceType::mjs(),
        _ => SourceType::ts(),
    }
}

/// Verifies that `source` parses cleanly with `oxc_parser` (0 syntax errors).
pub fn verify_jsts_syntax(path: &Path, source: &str) -> Result<(), String> {
    let allocator = Allocator::default();
    let source_type = source_type_for_path(path);
    let ret = Parser::new(&allocator, source, source_type).parse();
    if ret.diagnostics.is_empty() {
        Ok(())
    } else {
        let msgs: Vec<String> = ret.diagnostics.iter().map(|e| e.to_string()).collect();
        Err(format!("oxc parse error(s): {}", msgs.join("; ")))
    }
}

/// Tier 1 pure-Rust JS/TS analyzer powered by `oxc_parser` + `oxc_semantic` + `ast-grep` Refaster.
#[derive(Default)]
pub struct JsTsFastAnalyzer {
    pub refaster_rules: Vec<RefasterYamlRule>,
}

impl LanguageAnalyzerPort for JsTsFastAnalyzer {
    fn language(&self) -> Language {
        Language::JsTs
    }

    fn extensions(&self) -> &'static [&'static str] {
        Language::JSTS_EXTENSIONS
    }

    fn max_fixpoint_passes(&self) -> usize {
        3
    }

    fn import_manager(&self) -> Option<&dyn ImportManagerPort> {
        static IMPORT_MGR: OxcImportAdapter = OxcImportAdapter;
        Some(&IMPORT_MGR)
    }

    fn analyze_pass(&self, req: &AnalyzePassRequest<'_>) -> Result<AnalyzePassResponse, String> {
        let mut loaded_refaster: Vec<RefasterYamlRule> = Vec::new();
        for rp in req.rules_paths {
            loaded_refaster.extend(RefasterYamlRule::jsts_rules_from_file(rp)?);
        }
        let active_refaster: &[RefasterYamlRule] = if self.refaster_rules.is_empty() {
            &loaded_refaster
        } else {
            &self.refaster_rules
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
            let mut file_diags = analyze_jsts_source(wf.file_id, &wf.path, &wf.source);
            if !active_refaster.is_empty() {
                file_diags.extend(evaluate_jsts_refaster_rules(
                    wf.file_id,
                    &wf.path,
                    &wf.relative_path,
                    &wf.source,
                    active_refaster,
                ));
            }
            if let Some(scopes) = req.scoped_ranges {
                let allowed = scopes.get(&wf.file_id).map(Vec::as_slice).unwrap_or(&[]);
                file_diags.retain(|d| allowed.iter().any(|sp| sp.intersects(d.range)));
            }
            diagnostics.extend(file_diags);
        }

        Ok(AnalyzePassResponse {
            diagnostics,
            provenance: AnalysisProvenance {
                language: Language::JsTs,
                tier: AnalysisTier::Tier1Syntactic,
                files_analyzed,
                runtime_detail: Some("oxc_semantic (in-process, zero-node)".to_string()),
                degraded_reason: Some(
                    "Tier 2 TS7 semantic checker not active; type-dependent rules skipped"
                        .to_string(),
                ),
                remediation_hint: Some(
                    "Ensure tsconfig.json and node_modules (`npm ci --ignore-scripts`) are present for Tier 2 analysis"
                        .to_string(),
                ),
            },
        })
    }
}

/// Runs all 6 Tier 1 `oxc_semantic` rules on a single JS/TS source file.
pub fn analyze_jsts_source(file_id: FileId, path: &Path, source: &str) -> Vec<Diagnostic> {
    let allocator = Allocator::default();
    let source_type = source_type_for_path(path);
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.program.body.is_empty() && !parsed.diagnostics.is_empty() {
        return Vec::new();
    }
    let semantic_ret = SemanticBuilder::new().build(&parsed.program);
    let scoping = semantic_ret.semantic.scoping();

    let mut collector = JsTsTier1Visitor {
        file_id,
        source,
        scoping,
        line_index: LineIndex::new(source),
        scope_stack: vec![scoping.root_scope_id()],
        loop_depth: 0,
        async_depth: 0,
        try_block_depth: 0,
        top_level_stmt_start: None,
        hoist_counter: 0,
        diagnostics: Vec::new(),
    };

    for stmt in &parsed.program.body {
        collector.top_level_stmt_start = Some(stmt.span().start);
        collector.visit_statement(stmt);
    }

    collector.diagnostics
}

struct JsTsTier1Visitor<'a> {
    file_id: FileId,
    source: &'a str,
    scoping: &'a Scoping,
    line_index: LineIndex,
    scope_stack: Vec<ScopeId>,
    loop_depth: usize,
    async_depth: usize,
    try_block_depth: usize,
    top_level_stmt_start: Option<u32>,
    hoist_counter: usize,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> JsTsTier1Visitor<'a> {
    fn current_scope(&self) -> ScopeId {
        *self
            .scope_stack
            .last()
            .unwrap_or(&self.scoping.root_scope_id())
    }

    fn is_ident_shadowed(&self, name: &str) -> bool {
        self.scoping
            .find_binding(self.current_scope(), name.into())
            .is_some()
    }

    fn is_suppressed(&self, range: TextRange, rule_id: &str) -> bool {
        let line = self.line_index.line_of(range.start);
        if is_jsts_line_suppressed(self.line_index.line_slice(self.source, line), rule_id) {
            return true;
        }
        if line > 1 {
            let prev = self.line_index.line_slice(self.source, line - 1);
            if prev.trim_start().starts_with("//") && is_jsts_line_suppressed(prev, rule_id) {
                return true;
            }
        }
        false
    }

    fn push_diagnostic(&mut self, diag: Diagnostic) {
        if !self.is_suppressed(diag.range, &diag.rule_id) {
            self.diagnostics.push(diag);
        }
    }

    fn next_hoist_name(&mut self, prefix: &str) -> String {
        loop {
            self.hoist_counter += 1;
            let candidate = format!("{prefix}_{}", self.hoist_counter);
            if !self.source.contains(&candidate) {
                return candidate;
            }
        }
    }

    fn hoist_insert_offset_and_indent(&self) -> (u32, String) {
        let raw_start = self.top_level_stmt_start.unwrap_or(0);
        let line = self.line_index.line_of(riffty_core::ByteOffset(raw_start));
        let line_text = self.line_index.line_slice(self.source, line);
        let indent: String = line_text
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        // Walk back to start of the line so the hoisted `const` aligns cleanly.
        let bytes = self.source.as_bytes();
        let mut line_start = (raw_start as usize).min(bytes.len());
        while line_start > 0 && bytes[line_start - 1] != b'\n' {
            line_start -= 1;
        }
        (line_start as u32, indent)
    }

    fn check_nan_equality(&mut self, expr: &BinaryExpression<'_>) {
        let is_eq = matches!(
            expr.operator,
            BinaryOperator::StrictEquality | BinaryOperator::Equality
        );
        let is_neq = matches!(
            expr.operator,
            BinaryOperator::StrictInequality | BinaryOperator::Inequality
        );
        if !is_eq && !is_neq {
            return;
        }

        let left_is_nan = is_nan_ident(&expr.left);
        let right_is_nan = is_nan_ident(&expr.right);
        if left_is_nan == right_is_nan {
            return;
        }
        if self.is_ident_shadowed("NaN") || self.is_ident_shadowed("Number") {
            return;
        }

        let other = if left_is_nan { &expr.right } else { &expr.left };
        let other_span = other.span();
        let Some(other_src) = slice_span(self.source, other_span.start, other_span.end) else {
            return;
        };

        let replacement = if is_eq {
            format!("Number.isNaN({other_src})")
        } else {
            format!("!Number.isNaN({other_src})")
        };
        let range = TextRange::new(expr.span.start, expr.span.end);
        self.push_diagnostic(Diagnostic {
            rule_id: "JSTS-NAN-EQ-001".to_string(),
            rule_name: "nan-equality-comparison".to_string(),
            severity: Severity::Error,
            file_id: self.file_id,
            range,
            message: format!(
                "Comparison with `NaN` is always {}; use `{replacement}`.",
                if is_eq { "false" } else { "true" }
            ),
            type_proof: None,
            fix: Some(Fix {
                title: format!("Replace NaN comparison with `{replacement}`"),
                applicability: Applicability::Safe,
                edits: vec![Edit { range, replacement }],
                required_imports: Vec::new(),
            }),
        });
    }

    fn check_json_roundtrip_clone(&mut self, call: &CallExpression<'_>) {
        if !is_static_member_call(&call.callee, "JSON", "parse") {
            return;
        }
        if call.arguments.len() != 1 {
            return;
        }
        let Some(inner_expr) = call.arguments[0].as_expression() else {
            return;
        };
        let Expression::CallExpression(inner_call) = inner_expr.without_parentheses() else {
            return;
        };
        if !is_static_member_call(&inner_call.callee, "JSON", "stringify") {
            return;
        }
        if inner_call.arguments.len() != 1 {
            return;
        }
        let Some(arg_expr) = inner_call.arguments[0].as_expression() else {
            return;
        };
        if self.is_ident_shadowed("JSON") || self.is_ident_shadowed("structuredClone") {
            return;
        }
        let arg_span = arg_expr.span();
        let Some(arg_src) = slice_span(self.source, arg_span.start, arg_span.end) else {
            return;
        };

        let range = TextRange::new(call.span.start, call.span.end);
        let replacement = format!("structuredClone({arg_src})");
        self.push_diagnostic(Diagnostic {
            rule_id: "JSTS-CLONE-001".to_string(),
            rule_name: "json-roundtrip-deep-clone".to_string(),
            severity: Severity::Warning,
            file_id: self.file_id,
            range,
            message: "Avoid `JSON.parse(JSON.stringify(...))`; use `structuredClone(...)` to prevent CPU/GC overhead and Date/Map/Set data loss.".to_string(),
            type_proof: None,
            fix: Some(Fix {
                title: "Replace `JSON.parse(JSON.stringify(...))` with `structuredClone(...)`".to_string(),
                applicability: Applicability::Safe,
                edits: vec![Edit { range, replacement }],
                required_imports: Vec::new(),
            }),
        });
    }

    fn check_new_expression(&mut self, new_expr: &NewExpression<'_>) {
        // 1. JSTS-PROMISE-EXEC-001: `new Promise(async (...) => ...)`
        if is_ident_expr(&new_expr.callee, "Promise") && !self.is_ident_shadowed("Promise") {
            if let Some(first_arg) = new_expr.arguments.first().and_then(Argument::as_expression) {
                let is_async_exec = match first_arg.without_parentheses() {
                    Expression::ArrowFunctionExpression(f) => f.r#async,
                    Expression::FunctionExpression(f) => f.r#async,
                    _ => false,
                };
                if is_async_exec {
                    let range = TextRange::new(new_expr.span.start, new_expr.span.end);
                    self.push_diagnostic(Diagnostic {
                        rule_id: "JSTS-PROMISE-EXEC-001".to_string(),
                        rule_name: "async-promise-executor".to_string(),
                        severity: Severity::Error,
                        file_id: self.file_id,
                        range,
                        message: "Promise executor functions should not be `async`; thrown errors will not reject the outer Promise.".to_string(),
                        type_proof: None,
                        fix: Some(Fix {
                            title: "Refactor `new Promise(async ...)` into an `async` function or non-async executor".to_string(),
                            applicability: Applicability::ManualOnly,
                            edits: Vec::new(),
                            required_imports: Vec::new(),
                        }),
                    });
                }
            }
        }

        if self.loop_depth == 0 {
            return;
        }

        // 2. JSTS-REGEX-LOOP-001: `new RegExp("lit", "flags")` inside loop (skip /g and /y!)
        if is_ident_expr(&new_expr.callee, "RegExp") && !self.is_ident_shadowed("RegExp") {
            if matches!(new_expr.arguments.len(), 1 | 2) {
                let first_str = new_expr
                    .arguments
                    .first()
                    .and_then(Argument::as_expression)
                    .and_then(extract_string_literal);
                let second_str = if new_expr.arguments.len() == 2 {
                    new_expr
                        .arguments
                        .get(1)
                        .and_then(Argument::as_expression)
                        .and_then(extract_string_literal)
                } else {
                    Some("")
                };

                if let (Some(_pattern), Some(flags)) = (first_str, second_str) {
                    // Stateful `/g` and `/y` regexes mutate `lastIndex` and MUST NOT be hoisted!
                    if !flags.contains('g') && !flags.contains('y') {
                        let range = TextRange::new(new_expr.span.start, new_expr.span.end);
                        if let Some(expr_src) =
                            slice_span(self.source, new_expr.span.start, new_expr.span.end)
                        {
                            let var_name = self.next_hoist_name("__RIFFTY_RE");
                            let (insert_at, indent) = self.hoist_insert_offset_and_indent();
                            let decl = format!("{indent}const {var_name} = {expr_src};\n");
                            self.push_diagnostic(Diagnostic {
                                rule_id: "JSTS-REGEX-LOOP-001".to_string(),
                                rule_name: "regexp-allocated-in-loop".to_string(),
                                severity: Severity::Warning,
                                file_id: self.file_id,
                                range,
                                message: format!(
                                    "Avoid allocating constant `{expr_src}` inside a loop; hoist to `{var_name}`."
                                ),
                                type_proof: None,
                                fix: Some(Fix {
                                    title: format!("Hoist constant RegExp to `const {var_name}`"),
                                    applicability: Applicability::Safe,
                                    edits: vec![
                                        Edit {
                                            range: TextRange::new(insert_at, insert_at),
                                            replacement: decl,
                                        },
                                        Edit {
                                            range,
                                            replacement: var_name,
                                        },
                                    ],
                                    required_imports: Vec::new(),
                                }),
                            });
                        }
                    }
                }
            }
        }

        // 3. JSTS-INTL-LOOP-001: `new Intl.DateTimeFormat(...)` / `new Intl.NumberFormat(...)` inside loop
        if (is_static_member_call(&new_expr.callee, "Intl", "DateTimeFormat")
            || is_static_member_call(&new_expr.callee, "Intl", "NumberFormat"))
            && !self.is_ident_shadowed("Intl")
            && new_expr
                .arguments
                .iter()
                .all(|arg| arg.as_expression().is_some_and(is_constant_literal_expr))
        {
            let range = TextRange::new(new_expr.span.start, new_expr.span.end);
            if let Some(expr_src) =
                slice_span(self.source, new_expr.span.start, new_expr.span.end)
            {
                let var_name = self.next_hoist_name("__RIFFTY_INTL");
                let (insert_at, indent) = self.hoist_insert_offset_and_indent();
                let decl = format!("{indent}const {var_name} = {expr_src};\n");
                self.push_diagnostic(Diagnostic {
                    rule_id: "JSTS-INTL-LOOP-001".to_string(),
                    rule_name: "intl-formatter-allocated-in-loop".to_string(),
                    severity: Severity::Warning,
                    file_id: self.file_id,
                    range,
                    message: format!(
                        "Avoid allocating constant `{expr_src}` inside a loop; hoist to `{var_name}`."
                    ),
                    type_proof: None,
                    fix: Some(Fix {
                        title: format!("Hoist constant Intl formatter to `const {var_name}`"),
                        applicability: Applicability::Safe,
                        edits: vec![
                            Edit {
                                range: TextRange::new(insert_at, insert_at),
                                replacement: decl,
                            },
                            Edit {
                                range,
                                replacement: var_name,
                            },
                        ],
                        required_imports: Vec::new(),
                    }),
                });
            }
        }
    }

    fn check_return_await(&mut self, ret: &ReturnStatement<'_>) {
        if self.async_depth == 0 || self.try_block_depth > 0 {
            return;
        }
        let Some(arg) = &ret.argument else {
            return;
        };
        let Expression::AwaitExpression(await_expr) = arg.without_parentheses() else {
            return;
        };
        let await_start = await_expr.span.start;
        let inner_start = await_expr.argument.span().start;
        if inner_start <= await_start {
            return;
        }
        let diag_range = TextRange::new(ret.span.start, ret.span.end);
        let remove_range = TextRange::new(await_start, inner_start);
        self.push_diagnostic(Diagnostic {
            rule_id: "JSTS-DUP-AWAIT-001".to_string(),
            rule_name: "redundant-return-await".to_string(),
            severity: Severity::Warning,
            file_id: self.file_id,
            range: diag_range,
            message: "Redundant `return await` outside a `try` block; return the promise directly.".to_string(),
            type_proof: None,
            fix: Some(Fix {
                title: "Remove redundant `await` in `return await`".to_string(),
                applicability: Applicability::Safe,
                edits: vec![Edit {
                    range: remove_range,
                    replacement: String::new(),
                }],
                required_imports: Vec::new(),
            }),
        });
    }
}

impl<'a> Visit<'a> for JsTsTier1Visitor<'_> {
    fn visit_binary_expression(&mut self, it: &BinaryExpression<'a>) {
        self.check_nan_equality(it);
        walk::walk_binary_expression(self, it);
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        self.check_json_roundtrip_clone(it);
        let is_iter_method = is_array_iterator_call(&it.callee);
        if is_iter_method {
            self.visit_expression(&it.callee);
            self.loop_depth += 1;
            for arg in &it.arguments {
                self.visit_argument(arg);
            }
            self.loop_depth -= 1;
        } else {
            walk::walk_call_expression(self, it);
        }
    }

    fn visit_new_expression(&mut self, it: &NewExpression<'a>) {
        self.check_new_expression(it);
        walk::walk_new_expression(self, it);
    }

    fn visit_return_statement(&mut self, it: &ReturnStatement<'a>) {
        self.check_return_await(it);
        walk::walk_return_statement(self, it);
    }

    fn visit_for_statement(&mut self, it: &ForStatement<'a>) {
        self.loop_depth += 1;
        walk::walk_for_statement(self, it);
        self.loop_depth -= 1;
    }

    fn visit_for_in_statement(&mut self, it: &ForInStatement<'a>) {
        self.loop_depth += 1;
        walk::walk_for_in_statement(self, it);
        self.loop_depth -= 1;
    }

    fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
        self.loop_depth += 1;
        walk::walk_for_of_statement(self, it);
        self.loop_depth -= 1;
    }

    fn visit_while_statement(&mut self, it: &WhileStatement<'a>) {
        self.loop_depth += 1;
        walk::walk_while_statement(self, it);
        self.loop_depth -= 1;
    }

    fn visit_do_while_statement(&mut self, it: &DoWhileStatement<'a>) {
        self.loop_depth += 1;
        walk::walk_do_while_statement(self, it);
        self.loop_depth -= 1;
    }

    fn visit_try_statement(&mut self, it: &TryStatement<'a>) {
        self.try_block_depth += 1;
        self.visit_block_statement(&it.block);
        self.try_block_depth -= 1;
        if let Some(handler) = &it.handler {
            self.visit_catch_clause(handler);
        }
        if let Some(finalizer) = &it.finalizer {
            self.visit_block_statement(finalizer);
        }
    }

    fn visit_function(&mut self, it: &Function<'a>, flags: oxc_semantic::ScopeFlags) {
        let prev_try = self.try_block_depth;
        let prev_async = self.async_depth;
        self.try_block_depth = 0;
        self.async_depth = if it.r#async { 1 } else { 0 };
        let pushed_scope = it.scope_id.get();
        if let Some(sid) = pushed_scope {
            self.scope_stack.push(sid);
        }
        walk::walk_function(self, it, flags);
        if pushed_scope.is_some() {
            self.scope_stack.pop();
        }
        self.try_block_depth = prev_try;
        self.async_depth = prev_async;
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        let prev_try = self.try_block_depth;
        let prev_async = self.async_depth;
        self.try_block_depth = 0;
        self.async_depth = if it.r#async { 1 } else { 0 };
        let pushed_scope = it.scope_id.get();
        if let Some(sid) = pushed_scope {
            self.scope_stack.push(sid);
        }
        walk::walk_arrow_function_expression(self, it);
        if pushed_scope.is_some() {
            self.scope_stack.pop();
        }
        self.try_block_depth = prev_try;
        self.async_depth = prev_async;
    }

    fn visit_block_statement(&mut self, it: &oxc_ast::ast::BlockStatement<'a>) {
        let pushed_scope = it.scope_id.get();
        if let Some(sid) = pushed_scope {
            self.scope_stack.push(sid);
        }
        walk::walk_block_statement(self, it);
        if pushed_scope.is_some() {
            self.scope_stack.pop();
        }
    }
}

fn slice_span(source: &str, start: u32, end: u32) -> Option<&str> {
    let s = start as usize;
    let e = end as usize;
    if s <= e && e <= source.len() && source.is_char_boundary(s) && source.is_char_boundary(e) {
        Some(&source[s..e])
    } else {
        None
    }
}

fn is_ident_expr(expr: &Expression<'_>, name: &str) -> bool {
    matches!(
        expr.without_parentheses(),
        Expression::Identifier(id) if id.name.as_str() == name
    )
}

fn is_nan_ident(expr: &Expression<'_>) -> bool {
    is_ident_expr(expr, "NaN")
}

fn is_static_member_call(callee: &Expression<'_>, obj: &str, prop: &str) -> bool {
    match callee.without_parentheses() {
        Expression::StaticMemberExpression(mem) => {
            is_ident_expr(&mem.object, obj) && mem.property.name.as_str() == prop
        }
        _ => false,
    }
}

fn is_array_iterator_call(callee: &Expression<'_>) -> bool {
    match callee.without_parentheses() {
        Expression::StaticMemberExpression(mem) => matches!(
            mem.property.name.as_str(),
            "map"
                | "filter"
                | "forEach"
                | "some"
                | "every"
                | "find"
                | "findIndex"
                | "flatMap"
                | "reduce"
                | "reduceRight"
        ),
        _ => false,
    }
}

fn extract_string_literal<'a>(expr: &'a Expression<'a>) -> Option<&'a str> {
    match expr.without_parentheses() {
        Expression::StringLiteral(lit) => lit.value.as_str(),
        _ => None,
    }
}

fn is_constant_literal_expr(expr: &Expression<'_>) -> bool {
    match expr.without_parentheses() {
        Expression::StringLiteral(_)
        | Expression::NumericLiteral(_)
        | Expression::BooleanLiteral(_)
        | Expression::NullLiteral(_) => true,
        Expression::ArrayExpression(arr) => arr.elements.iter().all(|el| match el {
            ArrayExpressionElement::SpreadElement(_) | ArrayExpressionElement::Elision(_) => false,
            other => other.as_expression().is_some_and(is_constant_literal_expr),
        }),
        Expression::ObjectExpression(obj) => obj.properties.iter().all(|prop| match prop {
            ObjectPropertyKind::ObjectProperty(p) => {
                !p.computed
                    && matches!(
                        p.key,
                        PropertyKey::StaticIdentifier(_) | PropertyKey::StringLiteral(_)
                    )
                    && is_constant_literal_expr(&p.value)
            }
            ObjectPropertyKind::SpreadProperty(_) => false,
        }),
        _ => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use riffty_core::{ImportSpec, WorkspaceFile};
    use riffty_pipeline::{
        AnalyzerRegistry, PipelineConfig, WorkspaceOrchestrator, apply_edits_bottom_to_top,
    };
    use std::path::PathBuf;

    fn run_fix_on(filename: &str, source: &str) -> (String, Vec<Diagnostic>, usize) {
        let mut registry = AnalyzerRegistry::new();
        registry.register(JsTsFastAnalyzer::default());
        let orch = WorkspaceOrchestrator {
            registry: &registry,
        };
        let files = vec![WorkspaceFile {
            file_id: FileId(1),
            path: PathBuf::from(filename),
            relative_path: filename.to_string(),
            source: source.to_string(),
        }];
        let out = orch
            .run(
                Path::new("."),
                &files,
                &[],
                PipelineConfig {
                    apply_fixes: true,
                    allow_unsafe_fixes: false,
                    max_passes: 3,
                },
                None,
            )
            .expect("orchestrator run ok");
        let res = &out.files[0];
        // REQ-M1-07: Syntax re-parse via oxc_parser must have 0 errors after every fix!
        verify_jsts_syntax(Path::new(filename), &res.final_source)
            .expect("post-fix source must parse with 0 oxc errors");
        (
            res.final_source.clone(),
            res.remaining_diagnostics.clone(),
            res.applied_fixes.len(),
        )
    }

    #[test]
    fn req_m1_01_all_8_extensions_parsed_and_analyzed() {
        let samples = [
            ("a.ts", "export const f = (x: number): boolean => x === NaN;\n"),
            ("b.tsx", "export const V = ({ x }: { x: number }) => <div>{x === NaN}</div>;\n"),
            ("c.mts", "export const f = (x: number) => x === NaN;\n"),
            ("d.cts", "export const f = (x: number) => x === NaN;\n"),
            ("e.js", "export const f = (x) => x === NaN;\n"),
            ("f.jsx", "export const V = ({ x }) => <div>{x === NaN}</div>;\n"),
            ("g.mjs", "export const f = (x) => x === NaN;\n"),
            ("h.cjs", "module.exports = function(x) { return x === NaN; };\n"),
        ];
        for (name, src) in samples {
            let (fixed, remaining, count) = run_fix_on(name, src);
            assert_eq!(count, 1, "expected 1 fix on {name}");
            assert!(remaining.is_empty(), "expected 0 remaining on {name}");
            assert!(fixed.contains("Number.isNaN(x)"), "got:\n{fixed}");
        }
    }

    #[test]
    fn req_m1_02_all_6_rules_positive_negative_and_shadow_safety() {
        // 1. JSTS-NAN-EQ-001 (positive + negative + shadow)
        let (fixed, rem, cnt) = run_fix_on(
            "nan.ts",
            "export function f(x: number) {\n  const a = x === NaN;\n  const b = x !== NaN;\n  return a || b;\n}\n",
        );
        assert_eq!(cnt, 2);
        assert!(rem.is_empty());
        assert!(fixed.contains("Number.isNaN(x)"));
        assert!(fixed.contains("!Number.isNaN(x)"));

        // Shadowed Number must NOT fire
        let (_, _, cnt_shadow) = run_fix_on(
            "nan_shadow.ts",
            "export function f(x: number, Number: any) { return x === NaN; }\n",
        );
        assert_eq!(cnt_shadow, 0);

        // 2. JSTS-CLONE-001 (positive + shadow + multi-arg negative)
        let (fixed, rem, cnt) = run_fix_on(
            "clone.ts",
            "export function c<T>(obj: T): T {\n  return JSON.parse(JSON.stringify(obj));\n}\n",
        );
        assert_eq!(cnt, 1);
        assert!(rem.is_empty());
        assert!(fixed.contains("return structuredClone(obj);"));

        let (_, _, cnt_shadow_clone) = run_fix_on(
            "clone_shadow.ts",
            "export function c(obj: any) {\n  const structuredClone = (v: any) => v;\n  return JSON.parse(JSON.stringify(obj));\n}\n",
        );
        assert_eq!(cnt_shadow_clone, 0);

        // 3. JSTS-REGEX-LOOP-001 (stateless hoisted; stateful /g and /y MUST NOT be hoisted!)
        let (fixed_re, rem_re, cnt_re) = run_fix_on(
            "regex.ts",
            r#"export function countDigits(lines: string[]): number {
  let n = 0;
  for (const s of lines) {
    if (new RegExp("^[0-9]+$", "i").test(s)) n++;
    if (new RegExp("[0-9]+", "g").test(s)) n++;
    if (new RegExp("[0-9]+", "y").test(s)) n++;
  }
  return n;
}
"#,
        );
        assert_eq!(cnt_re, 1, "only stateless regex should be hoisted");
        assert!(rem_re.is_empty());
        assert!(fixed_re.contains("const __RIFFTY_RE_1 = new RegExp(\"^[0-9]+$\", \"i\");"));
        assert!(fixed_re.contains("new RegExp(\"[0-9]+\", \"g\")"), "/g must NOT be hoisted");
        assert!(fixed_re.contains("new RegExp(\"[0-9]+\", \"y\")"), "/y must NOT be hoisted");

        // 4. JSTS-INTL-LOOP-001 (constant hoisted; dynamic locale NOT hoisted)
        let (fixed_intl, rem_intl, cnt_intl) = run_fix_on(
            "intl.ts",
            r#"export function fmt(dates: Date[], locale: string): string[] {
  const a = dates.map((d) => new Intl.DateTimeFormat("en-US", { dateStyle: "short" }).format(d));
  const b = dates.map((d) => new Intl.DateTimeFormat(locale).format(d));
  return [...a, ...b];
}
"#,
        );
        assert_eq!(cnt_intl, 1);
        assert!(rem_intl.is_empty());
        assert!(fixed_intl.contains("const __RIFFTY_INTL_1 = new Intl.DateTimeFormat(\"en-US\", { dateStyle: \"short\" });"));
        assert!(fixed_intl.contains("new Intl.DateTimeFormat(locale)"));

        // 5. JSTS-PROMISE-EXEC-001 (ManualOnly error; non-async executor NOT flagged)
        let (_, rem_p, cnt_p) = run_fix_on(
            "promise.ts",
            r#"export function bad() {
  return new Promise(async (resolve) => {
    resolve(await Promise.resolve(1));
  });
}
export function good() {
  return new Promise((resolve) => resolve(1));
}
"#,
        );
        assert_eq!(cnt_p, 0);
        assert_eq!(rem_p.len(), 1);
        assert_eq!(rem_p[0].rule_id, "JSTS-PROMISE-EXEC-001");
        assert_eq!(rem_p[0].severity, Severity::Error);

        // 6. JSTS-DUP-AWAIT-001 (outside try -> fixed; inside try -> preserved!)
        let (fixed_aw, rem_aw, cnt_aw) = run_fix_on(
            "await.ts",
            r#"export async function outsideTry(fn: () => Promise<number>) {
  return await fn();
}
export async function insideTry(fn: () => Promise<number>) {
  try {
    return await fn();
  } catch {
    return 0;
  }
}
"#,
        );
        assert_eq!(cnt_aw, 1);
        assert!(rem_aw.is_empty());
        assert!(fixed_aw.contains("export async function outsideTry(fn: () => Promise<number>) {\n  return fn();\n}"));
        assert!(fixed_aw.contains("try {\n    return await fn();\n  }"));
    }

    #[test]
    fn req_m1_03_oxc_import_adapter_preserves_directives_merges_and_checks_shadow() {
        let mgr = OxcImportAdapter;

        // 1. Preserves 'use client' directive and merges into existing import
        let src1 = "'use client';\nimport { useState } from \"react\";\nexport const x = useState(0);\n";
        let edits1 = mgr.resolve_import_edits(
            src1,
            &[
                ImportSpec {
                    module: "react".to_string(),
                    symbol: Some("useEffect".to_string()),
                },
                ImportSpec {
                    module: "zod".to_string(),
                    symbol: Some("z".to_string()),
                },
            ],
        );
        let out1 = apply_edits_bottom_to_top(src1, &edits1).unwrap();
        assert!(out1.starts_with("'use client';\n"), "directive must stay first:\n{out1}");
        assert!(out1.contains("import { useState, useEffect } from \"react\";"), "got:\n{out1}");
        assert!(out1.contains("import { z } from \"zod\";\n"), "got:\n{out1}");

        // 2. Shadowed local binding prevents conflicting import
        let src2 = "'use strict';\nconst z = 42;\nexport const y = z;\n";
        let edits2 = mgr.resolve_import_edits(
            src2,
            &[ImportSpec {
                module: "zod".to_string(),
                symbol: Some("z".to_string()),
            }],
        );
        assert!(edits2.is_empty(), "shadowed `z` must not emit duplicate import");
    }

    #[test]
    fn req_m1_04_riffty_ignore_suppression_works_in_jsts() {
        let src = r#"export function f(x: number) {
  const a = x === NaN; // riffty: ignore[JSTS-NAN-EQ-001] -- intentional test
  // riffty: ignore[JSTS-CLONE-001] -- legacy compat
  const b = JSON.parse(JSON.stringify({ x }));
  return [a, b];
}
"#;
        let diags = analyze_jsts_source(FileId(1), Path::new("suppressed.ts"), src);
        assert!(diags.is_empty(), "expected 0 diagnostics, got: {diags:?}");
    }

    #[test]
    fn req_m1_07_fixpoint_convergence_and_idempotency() {
        let src = r#"export async function processItems(items: string[], n: number) {
  const copy = JSON.parse(JSON.stringify(items));
  if (n === NaN) return [];
  const out: string[] = [];
  for (const s of copy) {
    if (new RegExp("^[a-z]+$").test(s)) {
      out.push(new Intl.NumberFormat("en-US").format(s.length));
    }
  }
  return await Promise.resolve(out);
}
"#;
        let (pass1, rem1, cnt1) = run_fix_on("idempotent.ts", src);
        assert_eq!(cnt1, 5);
        assert!(rem1.is_empty());

        // Second run MUST produce 0 diagnostics and 0 edits!
        let (pass2, rem2, cnt2) = run_fix_on("idempotent.ts", &pass1);
        assert_eq!(cnt2, 0);
        assert!(rem2.is_empty());
        assert_eq!(pass1, pass2);
    }
}

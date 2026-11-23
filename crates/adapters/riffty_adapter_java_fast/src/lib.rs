use ast_grep_core::{Node, Pattern};
use ast_grep_language::{Java, LanguageExt};
use riffty_core::{
    AnalysisProvenance, AnalysisTier, AnalyzePassRequest, AnalyzePassResponse, Applicability,
    ByteOffset, Diagnostic, Edit, FileId, Fix, Language, LanguageAnalyzerPort, LineIndex, Severity,
    TextRange,
};

#[derive(Debug, Clone, Copy)]
pub struct RuleDoc {
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

/// Returns documentation metadata for all built-in Tier 1 Java rules (used by `riffty explain` and SARIF).
pub fn java_fast_rule_catalog() -> &'static [RuleDoc] {
    &[
        RuleDoc {
            id: "JAVA-BIGDEC-001",
            name: "bigdecimal-double-literal",
            language: Language::Java,
            severity: Severity::Error,
            default_applicability: "Safe",
            summary: "Avoid `new BigDecimal(double)` which preserves IEEE-754 binary floating-point rounding error.",
            rationale: "Constructing `new BigDecimal(0.1)` yields `0.1000000000000000055511151231257827021181583404541015625`, causing silent financial/billing drift. `BigDecimal.valueOf(0.1)` uses the canonical decimal string representation.",
            bad_example: "BigDecimal price = new BigDecimal(0.1);",
            good_example: "BigDecimal price = BigDecimal.valueOf(0.1);",
        },
        RuleDoc {
            id: "JAVA-BOX-001",
            name: "deprecated-primitive-constructor",
            language: Language::Java,
            severity: Severity::Warning,
            default_applicability: "Safe",
            summary: "Replace deprecated wrapper constructors (`new Integer`, `new Long`, `new Double`, `new Boolean`) with `valueOf(...)`.",
            rationale: "Primitive wrapper constructors were deprecated for removal in Java 9+ and always allocate a new heap object, bypassing JVM integer/boolean caches and increasing GC pressure.",
            bad_example: "Integer count = new Integer(42);",
            good_example: "Integer count = Integer.valueOf(42);",
        },
        RuleDoc {
            id: "JAVA-STREAM-001",
            name: "stream-collect-is-empty",
            language: Language::Java,
            severity: Severity::Warning,
            default_applicability: "Safe",
            summary: "Avoid materializing an entire `List` via `.collect(Collectors.toList()).isEmpty()` or `.toList().isEmpty()`.",
            rationale: "Collecting a full stream into a temporary `List` only to check emptiness allocates O(N) heap memory and drains the entire pipeline; `.findAny().isEmpty()` short-circuits on the first element with zero list allocation.",
            bad_example: "boolean none = items.stream().filter(Item::isActive).collect(Collectors.toList()).isEmpty();",
            good_example: "boolean none = items.stream().filter(Item::isActive).findAny().isEmpty();",
        },
        RuleDoc {
            id: "JAVA-OBJMAP-001",
            name: "per-call-object-mapper",
            language: Language::Java,
            severity: Severity::Warning,
            default_applicability: "ManualOnly (Tier 1) / Safe (Tier 2)",
            summary: "Avoid allocating `new ObjectMapper()` inside hot method bodies.",
            rationale: "Each Jackson `ObjectMapper` instantiation initializes reflection caches and serializer/deserializer tables, burning significant CPU and GC memory per request. Hoist stateless instances to `private static final` or inject a shared bean.",
            bad_example: "public String toJson(Order o) throws Exception { return new ObjectMapper().writeValueAsString(o); }",
            good_example: "private static final ObjectMapper MAPPER = new ObjectMapper();\npublic String toJson(Order o) throws Exception { return MAPPER.writeValueAsString(o); }",
        },
        RuleDoc {
            id: "JAVA-HTTP-001",
            name: "unpooled-http-client",
            language: Language::Java,
            severity: Severity::Warning,
            default_applicability: "ManualOnly",
            summary: "Avoid creating unpooled `HttpClient.newHttpClient()` or `new OkHttpClient()` inside method bodies.",
            rationale: "Per-call HTTP clients bypass TCP/TLS connection pooling, spawn internal selector/dispatcher threads, and frequently cause socket/thread exhaustion under production load.",
            bad_example: "public HttpResponse<String> fetch(HttpRequest req) throws Exception { return HttpClient.newHttpClient().send(req, BodyHandlers.ofString()); }",
            good_example: "private static final HttpClient HTTP = HttpClient.newHttpClient();\npublic HttpResponse<String> fetch(HttpRequest req) throws Exception { return HTTP.send(req, BodyHandlers.ofString()); }",
        },
        RuleDoc {
            id: "JAVA-REGEX-001",
            name: "regex-compile-in-loop",
            language: Language::Java,
            severity: Severity::Warning,
            default_applicability: "ManualOnly",
            summary: "Avoid compiling constant `Pattern.compile(\"...\")` regexes inside loops.",
            rationale: "Compiling a constant regex on every loop iteration repeatedly builds NFA/DFA state machines on the heap. Hoist to a `private static final Pattern` constant.",
            bad_example: "for (String s : lines) { if (Pattern.compile(\"^[0-9]+$\").matcher(s).matches()) { ... } }",
            good_example: "private static final Pattern DIGITS = Pattern.compile(\"^[0-9]+$\");\nfor (String s : lines) { if (DIGITS.matcher(s).matches()) { ... } }",
        },
        RuleDoc {
            id: "JAVA-STR-001",
            name: "string-concat-in-loop",
            language: Language::Java,
            severity: Severity::Warning,
            default_applicability: "ManualOnly",
            summary: "Avoid `String` concatenation with `+=` inside loops.",
            rationale: "Repeated `+=` string concatenation inside loops allocates O(N^2) intermediate `String` / `byte[]` buffers; use `StringBuilder` instead.",
            bad_example: "String out = \"\";\nfor (String s : items) { out += \",\" + s; }",
            good_example: "StringBuilder out = new StringBuilder();\nfor (String s : items) { out.append(',').append(s); }",
        },
        RuleDoc {
            id: "JAVA-LEAK-001",
            name: "unclosed-files-stream",
            language: Language::Java,
            severity: Severity::Error,
            default_applicability: "ManualOnly",
            summary: "`Files.walk`, `Files.lines`, and `Files.list` hold open OS file descriptors and must be closed with `try-with-resources`.",
            rationale: "Unlike collection streams, `java.nio.file.Files` streams hold native directory/file handles until `.close()` is called. Failing to wrap them in `try-with-resources` causes `TooManyOpenFiles` production outages.",
            bad_example: "List<Path> paths = Files.walk(root).filter(Files::isRegularFile).toList();",
            good_example: "try (Stream<Path> walk = Files.walk(root)) {\n    List<Path> paths = walk.filter(Files::isRegularFile).toList();\n}",
        },
    ]
}

/// Tier 1 zero-JVM Java analyzer powered by `tree-sitter-java` (`ast-grep-language`).
#[derive(Default, Debug, Clone, Copy)]
pub struct JavaFastAnalyzer;

impl LanguageAnalyzerPort for JavaFastAnalyzer {
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
            let mut file_diags = analyze_java_source(wf.file_id, &wf.source);
            if let Some(scopes) = req.scoped_ranges {
                let allowed = scopes.get(&wf.file_id).map(Vec::as_slice).unwrap_or(&[]);
                file_diags.retain(|d| allowed.iter().any(|sp| sp.intersects(d.range)));
            }
            diagnostics.extend(file_diags);
        }

        Ok(AnalyzePassResponse {
            diagnostics,
            provenance: AnalysisProvenance {
                language: Language::Java,
                tier: AnalysisTier::Tier1Syntactic,
                files_analyzed,
                runtime_detail: Some("tree-sitter-java (in-process, zero-jvm)".to_string()),
                degraded_reason: Some(
                    "Tier 2 javac+ErrorProne sidecar not configured (running Tier 1 syntactic rules)"
                        .to_string(),
                ),
                remediation_hint: Some(
                    "Enable riffty-java Tier 2 sidecar (Milestone 2) for full type-resolved analysis"
                        .to_string(),
                ),
            },
        })
    }
}

pub fn analyze_java_source(file_id: FileId, source: &str) -> Vec<Diagnostic> {
    let grep = Java.ast_grep(source);
    let root = grep.root();
    let line_index = LineIndex::new(source);
    let mut diags = Vec::new();

    check_bigdecimal_double(&root, file_id, source, &line_index, &mut diags);
    check_deprecated_primitive_box(&root, file_id, source, &line_index, &mut diags);
    check_stream_collect_is_empty(&root, file_id, source, &line_index, &mut diags);
    check_per_call_object_mapper(&root, file_id, source, &line_index, &mut diags);
    check_unpooled_http_client(&root, file_id, source, &line_index, &mut diags);
    check_regex_compile_in_loop(&root, file_id, source, &line_index, &mut diags);
    check_string_concat_in_loop(&root, file_id, source, &line_index, &mut diags);
    check_unclosed_files_stream(&root, file_id, source, &line_index, &mut diags);

    diags.sort_by(|a, b| {
        a.file_id
            .cmp(&b.file_id)
            .then_with(|| a.range.start.cmp(&b.range.start))
            .then_with(|| a.range.end.cmp(&b.range.end))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });
    diags
}

fn check_bigdecimal_double<D: ast_grep_core::Doc>(
    root: &Node<'_, D>,
    file_id: FileId,
    source: &str,
    line_index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Ok(pat) = Pattern::try_new("new BigDecimal($ARG)", Java) else {
        return;
    };
    for m in root.find_all(&pat) {
        let Some(arg_node) = m.get_env().get_match("ARG") else {
            continue;
        };
        let arg_kind = arg_node.kind();
        let arg_text = arg_node.text();
        let is_float_lit = arg_kind == "decimal_floating_point_literal"
            || (arg_kind == "unary_expression"
                && arg_text.contains('.')
                && !arg_text.contains('"'));
        if !is_float_lit {
            continue;
        }
        let range = node_range(&m);
        if is_suppressed(&m, source, line_index, range.start, "JAVA-BIGDEC-001") {
            continue;
        }
        out.push(Diagnostic {
            rule_id: "JAVA-BIGDEC-001".to_string(),
            rule_name: "bigdecimal-double-literal".to_string(),
            severity: Severity::Error,
            file_id,
            range,
            message: format!(
                "`new BigDecimal({arg_text})` introduces IEEE-754 binary floating-point rounding error; use `BigDecimal.valueOf({arg_text})`."
            ),
            type_proof: Some(serde_json::json!({
                "tier": "tier1_syntactic",
                "arg_kind": arg_kind.as_ref(),
                "literal": arg_text.as_ref(),
            })),
            fix: Some(Fix {
                title: format!("Replace `new BigDecimal({arg_text})` with `BigDecimal.valueOf({arg_text})`"),
                edits: vec![Edit {
                    range,
                    replacement: format!("BigDecimal.valueOf({arg_text})"),
                }],
                required_imports: Vec::new(),
                applicability: Applicability::Safe,
            }),
        });
    }
}

fn check_deprecated_primitive_box<D: ast_grep_core::Doc>(
    root: &Node<'_, D>,
    file_id: FileId,
    source: &str,
    line_index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    for wrapper in ["Integer", "Long", "Double", "Boolean"] {
        let pat_str = format!("new {wrapper}($ARG)");
        let Ok(pat) = Pattern::try_new(&pat_str, Java) else {
            continue;
        };
        for m in root.find_all(&pat) {
            let Some(arg_node) = m.get_env().get_match("ARG") else {
                continue;
            };
            let arg_text = arg_node.text();
            let range = node_range(&m);
            if is_suppressed(&m, source, line_index, range.start, "JAVA-BOX-001") {
                continue;
            }
            out.push(Diagnostic {
                rule_id: "JAVA-BOX-001".to_string(),
                rule_name: "deprecated-primitive-constructor".to_string(),
                severity: Severity::Warning,
                file_id,
                range,
                message: format!(
                    "Deprecated `new {wrapper}({arg_text})` allocates a new wrapper instance; use `{wrapper}.valueOf({arg_text})` to enable JVM caching."
                ),
                type_proof: Some(serde_json::json!({
                    "tier": "tier1_syntactic",
                    "wrapper": wrapper,
                })),
                fix: Some(Fix {
                    title: format!("Replace `new {wrapper}({arg_text})` with `{wrapper}.valueOf({arg_text})`"),
                    edits: vec![Edit {
                        range,
                        replacement: format!("{wrapper}.valueOf({arg_text})"),
                    }],
                    required_imports: Vec::new(),
                    applicability: Applicability::Safe,
                }),
            });
        }
    }
}

fn check_stream_collect_is_empty<D: ast_grep_core::Doc>(
    root: &Node<'_, D>,
    file_id: FileId,
    source: &str,
    line_index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let patterns = [
        (
            "$STREAM.collect(Collectors.toList()).isEmpty()",
            "collect(Collectors.toList()).isEmpty()",
        ),
        (
            "$STREAM.collect(Collectors.toList()).size() == 0",
            "collect(Collectors.toList()).size() == 0",
        ),
    ];

    for (pat_str, label) in patterns {
        let Ok(pat) = Pattern::try_new(pat_str, Java) else {
            continue;
        };
        for m in root.find_all(&pat) {
            let Some(stream_node) = m.get_env().get_match("STREAM") else {
                continue;
            };
            let stream_text = stream_node.text();
            let range = node_range(&m);
            if is_suppressed(&m, source, line_index, range.start, "JAVA-STREAM-001") {
                continue;
            }
            out.push(Diagnostic {
                rule_id: "JAVA-STREAM-001".to_string(),
                rule_name: "stream-collect-is-empty".to_string(),
                severity: Severity::Warning,
                file_id,
                range,
                message: format!(
                    "`{label}` materializes a full temporary List just to test emptiness; use `.findAny().isEmpty()` to short-circuit with zero list allocation."
                ),
                type_proof: Some(serde_json::json!({
                    "tier": "tier1_syntactic",
                    "pattern": label,
                })),
                fix: Some(Fix {
                    title: "Replace with short-circuiting `.findAny().isEmpty()`".to_string(),
                    edits: vec![Edit {
                        range,
                        replacement: format!("{stream_text}.findAny().isEmpty()"),
                    }],
                    required_imports: Vec::new(),
                    applicability: Applicability::Safe,
                }),
            });
        }
    }
}

fn check_per_call_object_mapper<D: ast_grep_core::Doc>(
    root: &Node<'_, D>,
    file_id: FileId,
    source: &str,
    line_index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Ok(pat) = Pattern::try_new("new ObjectMapper()", Java) else {
        return;
    };
    for m in root.find_all(&pat) {
        if !is_inside_hot_method(&m) {
            continue;
        }
        let range = node_range(&m);
        if is_suppressed(&m, source, line_index, range.start, "JAVA-OBJMAP-001") {
            continue;
        }
        out.push(Diagnostic {
            rule_id: "JAVA-OBJMAP-001".to_string(),
            rule_name: "per-call-object-mapper".to_string(),
            severity: Severity::Warning,
            file_id,
            range,
            message: "Allocating `new ObjectMapper()` inside a method body rebuilds Jackson reflection caches on every call; hoist to `private static final` or inject a shared bean.".to_string(),
            type_proof: Some(serde_json::json!({
                "tier": "tier1_syntactic",
                "requires_tier2_for_safe_hoist": true,
            })),
            fix: Some(Fix {
                title: "Hoist stateless ObjectMapper to `private static final` field".to_string(),
                edits: Vec::new(),
                required_imports: Vec::new(),
                applicability: Applicability::ManualOnly,
            }),
        });
    }
}

fn check_unpooled_http_client<D: ast_grep_core::Doc>(
    root: &Node<'_, D>,
    file_id: FileId,
    source: &str,
    line_index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    for pat_str in ["HttpClient.newHttpClient()", "new OkHttpClient()"] {
        let Ok(pat) = Pattern::try_new(pat_str, Java) else {
            continue;
        };
        for m in root.find_all(&pat) {
            if !is_inside_hot_method(&m) {
                continue;
            }
            let range = node_range(&m);
            if is_suppressed(&m, source, line_index, range.start, "JAVA-HTTP-001") {
                continue;
            }
            out.push(Diagnostic {
                rule_id: "JAVA-HTTP-001".to_string(),
                rule_name: "unpooled-http-client".to_string(),
                severity: Severity::Warning,
                file_id,
                range,
                message: format!(
                    "`{pat_str}` allocated inside a method body bypasses connection pooling and risks thread/socket exhaustion; reuse a shared client instance."
                ),
                type_proof: Some(serde_json::json!({
                    "tier": "tier1_syntactic",
                    "client_expr": pat_str,
                })),
                fix: Some(Fix {
                    title: "Reuse a shared pooled HTTP client instance".to_string(),
                    edits: Vec::new(),
                    required_imports: Vec::new(),
                    applicability: Applicability::ManualOnly,
                }),
            });
        }
    }
}

fn check_regex_compile_in_loop<D: ast_grep_core::Doc>(
    root: &Node<'_, D>,
    file_id: FileId,
    source: &str,
    line_index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Ok(pat) = Pattern::try_new("Pattern.compile($REGEX)", Java) else {
        return;
    };
    for m in root.find_all(&pat) {
        let Some(regex_arg) = m.get_env().get_match("REGEX") else {
            continue;
        };
        if regex_arg.kind() != "string_literal" {
            continue;
        }
        if !has_ancestor_kind(
            &m,
            &[
                "for_statement",
                "enhanced_for_statement",
                "while_statement",
                "do_statement",
            ],
        ) {
            continue;
        }
        let range = node_range(&m);
        if is_suppressed(&m, source, line_index, range.start, "JAVA-REGEX-001") {
            continue;
        }
        out.push(Diagnostic {
            rule_id: "JAVA-REGEX-001".to_string(),
            rule_name: "regex-compile-in-loop".to_string(),
            severity: Severity::Warning,
            file_id,
            range,
            message: format!(
                "Constant regex `Pattern.compile({})` is recompiled on every loop iteration; hoist to a `private static final Pattern` field.",
                regex_arg.text()
            ),
            type_proof: Some(serde_json::json!({
                "tier": "tier1_syntactic",
                "inside_loop": true,
            })),
            fix: Some(Fix {
                title: "Hoist constant `Pattern.compile(...)` to a `static final Pattern` field".to_string(),
                edits: Vec::new(),
                required_imports: Vec::new(),
                applicability: Applicability::ManualOnly,
            }),
        });
    }
}

fn check_string_concat_in_loop<D: ast_grep_core::Doc>(
    root: &Node<'_, D>,
    file_id: FileId,
    source: &str,
    line_index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Ok(pat) = Pattern::try_new("$LHS += $RHS", Java) else {
        return;
    };
    for m in root.find_all(&pat) {
        let Some(rhs) = m.get_env().get_match("RHS") else {
            continue;
        };
        let rhs_is_string = rhs.kind() == "string_literal"
            || (rhs.kind() == "binary_expression" && rhs.text().contains('"'));
        if !rhs_is_string {
            continue;
        }
        if !has_ancestor_kind(
            &m,
            &[
                "for_statement",
                "enhanced_for_statement",
                "while_statement",
                "do_statement",
            ],
        ) {
            continue;
        }
        let range = node_range(&m);
        if is_suppressed(&m, source, line_index, range.start, "JAVA-STR-001") {
            continue;
        }
        out.push(Diagnostic {
            rule_id: "JAVA-STR-001".to_string(),
            rule_name: "string-concat-in-loop".to_string(),
            severity: Severity::Warning,
            file_id,
            range,
            message: "String concatenation with `+=` inside a loop causes O(N^2) allocations; accumulate with `StringBuilder`.".to_string(),
            type_proof: Some(serde_json::json!({
                "tier": "tier1_syntactic",
                "inside_loop": true,
            })),
            fix: Some(Fix {
                title: "Replace loop `+=` string concatenation with `StringBuilder`".to_string(),
                edits: Vec::new(),
                required_imports: Vec::new(),
                applicability: Applicability::ManualOnly,
            }),
        });
    }
}

fn check_unclosed_files_stream<D: ast_grep_core::Doc>(
    root: &Node<'_, D>,
    file_id: FileId,
    source: &str,
    line_index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    for method in ["walk", "lines", "list"] {
        let pat_str = format!("Files.{method}($$$ARGS)");
        let Ok(pat) = Pattern::try_new(&pat_str, Java) else {
            continue;
        };
        for m in root.find_all(&pat) {
            // Safe if directly bound inside a try-with-resources header (`resource_specification`)
            // or returned directly to a caller (`return_statement`).
            if has_ancestor_kind(&m, &["resource_specification", "return_statement"]) {
                continue;
            }
            let range = node_range(&m);
            if is_suppressed(&m, source, line_index, range.start, "JAVA-LEAK-001") {
                continue;
            }
            out.push(Diagnostic {
                rule_id: "JAVA-LEAK-001".to_string(),
                rule_name: "unclosed-files-stream".to_string(),
                severity: Severity::Error,
                file_id,
                range,
                message: format!(
                    "`Files.{method}(...)` opens an OS file descriptor stream that is not wrapped in `try-with-resources`; close it deterministically to prevent FD exhaustion."
                ),
                type_proof: Some(serde_json::json!({
                    "tier": "tier1_syntactic",
                    "api": format!("java.nio.file.Files.{method}"),
                })),
                fix: Some(Fix {
                    title: format!("Wrap `Files.{method}(...)` in `try-with-resources`"),
                    edits: Vec::new(),
                    required_imports: Vec::new(),
                    applicability: Applicability::ManualOnly,
                }),
            });
        }
    }
}

fn node_range<D: ast_grep_core::Doc>(node: &Node<'_, D>) -> TextRange {
    let r = node.range();
    TextRange::new(r.start as u32, r.end as u32)
}

fn has_ancestor_kind<D: ast_grep_core::Doc>(node: &Node<'_, D>, kinds: &[&str]) -> bool {
    let mut cur = node.parent();
    while let Some(parent) = cur {
        if kinds.contains(&parent.kind().as_ref()) {
            return true;
        }
        cur = parent.parent();
    }
    false
}

/// Returns true if `node` is inside a method body and NOT inside a field declaration,
/// static initializer, constructor, or `@Bean`-annotated Spring factory method.
fn is_inside_hot_method<D: ast_grep_core::Doc>(node: &Node<'_, D>) -> bool {
    let mut cur = node.parent();
    while let Some(parent) = cur {
        let kind = parent.kind();
        if matches!(
            kind.as_ref(),
            "field_declaration" | "static_initializer" | "constructor_declaration"
        ) {
            return false;
        }
        if kind.as_ref() == "method_declaration" {
            let text = parent.text();
            if text.contains("@Bean") {
                return false;
            }
            return true;
        }
        cur = parent.parent();
    }
    false
}

fn is_suppressed<D: ast_grep_core::Doc>(
    node: &Node<'_, D>,
    source: &str,
    line_index: &LineIndex,
    offset: ByteOffset,
    rule_id: &str,
) -> bool {
    // 1. Inline `// riffty: ignore` or `// riffty: ignore[JAVA-...]` on same line
    let line_num = line_index.line_of(offset);
    let line_text = line_index.line_slice(source, line_num);
    if let Some(comment_pos) = line_text.find("//") {
        let comment = line_text[comment_pos + 2..].trim();
        if let Some(rest) = comment.strip_prefix("riffty:") {
            let rest = rest.trim_start();
            if let Some(after_ignore) = rest.strip_prefix("ignore") {
                let after_ignore = after_ignore.trim_start();
                if let Some(bracketed) = after_ignore.strip_prefix('[') {
                    if let Some(end) = bracketed.find(']') {
                        let matched = bracketed[..end]
                            .split(',')
                            .any(|r| r.trim().eq_ignore_ascii_case(rule_id));
                        if matched {
                            return true;
                        }
                    }
                } else {
                    return true;
                }
            }
        }
    }

    // 2. Enclosing `@SuppressWarnings("riffty")` or `@SuppressWarnings("riffty:<RULE_ID>")`
    let needle_specific = format!("riffty:{rule_id}");
    let mut cur = node.parent();
    while let Some(parent) = cur {
        if matches!(
            parent.kind().as_ref(),
            "method_declaration" | "class_declaration" | "local_variable_declaration"
        ) {
            let text = parent.text();
            if text.contains("@SuppressWarnings")
                && (text.contains("\"riffty\"") || text.contains(&needle_specific))
            {
                return true;
            }
        }
        cur = parent.parent();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bigdecimal_double_and_primitive_box_and_stream_collect_emit_safe_fixes() {
        let src = r#"
import java.math.BigDecimal;
import java.util.List;
import java.util.stream.Collectors;

class BillingService {
    void process(List<String> items) {
        BigDecimal fee = new BigDecimal(0.1);
        BigDecimal exact = new BigDecimal("0.1");
        Integer count = new Integer(42);
        boolean empty = items.stream().filter(s -> !s.isEmpty()).collect(Collectors.toList()).isEmpty();
    }
}
"#;
        let diags = analyze_java_source(FileId(1), src);
        let ids: Vec<&str> = diags.iter().map(|d| d.rule_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["JAVA-BIGDEC-001", "JAVA-BOX-001", "JAVA-STREAM-001"]
        );

        assert_eq!(
            diags[0].fix.as_ref().unwrap().edits[0].replacement,
            "BigDecimal.valueOf(0.1)"
        );
        assert_eq!(
            diags[1].fix.as_ref().unwrap().edits[0].replacement,
            "Integer.valueOf(42)"
        );
        assert_eq!(
            diags[2].fix.as_ref().unwrap().edits[0].replacement,
            "items.stream().filter(s -> !s.isEmpty()).findAny().isEmpty()"
        );
    }

    #[test]
    fn finops_and_leak_rules_detect_hot_methods_and_respect_suppressions() {
        let src = r#"
import com.fasterxml.jackson.databind.ObjectMapper;
import java.net.http.HttpClient;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.regex.Pattern;

class OrderController {
    private static final ObjectMapper SHARED = new ObjectMapper();

    @Bean
    ObjectMapper beanMapper() {
        return new ObjectMapper();
    }

    void handle(Path root, String[] lines) throws Exception {
        ObjectMapper m = new ObjectMapper();
        HttpClient c = HttpClient.newHttpClient();
        String acc = "";
        for (String line : lines) {
            Pattern p = Pattern.compile("^[0-9]+$");
            acc += "," + line;
        }
        var count = Files.walk(root).count();
        try (var safe = Files.lines(root)) {
            safe.count();
        }
        BigDecimal ignored = new BigDecimal(0.2); // riffty: ignore[JAVA-BIGDEC-001]
    }
}
"#;
        let diags = analyze_java_source(FileId(1), src);
        let ids: Vec<&str> = diags.iter().map(|d| d.rule_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "JAVA-OBJMAP-001",
                "JAVA-HTTP-001",
                "JAVA-REGEX-001",
                "JAVA-STR-001",
                "JAVA-LEAK-001",
            ]
        );
    }
}

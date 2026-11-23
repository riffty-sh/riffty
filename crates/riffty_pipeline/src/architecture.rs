use std::collections::BTreeSet;
use std::path::Path;

use riffty_core::architecture::{ArchitectureConfig, DependencyEdge, evaluate_architecture};
use riffty_core::{Diagnostic, FileId, Language, TextRange, WorkspaceFile};

/// Extracts uniform cross-language `DependencyEdge`s from workspace files and evaluates
/// `[architecture]` (`forbid`, `layers`, `forbid_cycles`).
///
/// Any extra semantic edges (for example, resolved `javac` `Trees` edges from `riffty-java`)
/// can be supplied via `extra_edges`; files already represented in `extra_edges` will not be
/// double-extracted by the syntactic fallback.
pub fn evaluate_workspace_architecture(
    config: &ArchitectureConfig,
    files: &[WorkspaceFile],
    extra_edges: &[DependencyEdge],
) -> Vec<Diagnostic> {
    if config.is_empty() {
        return Vec::new();
    }

    let covered_files: BTreeSet<FileId> = extra_edges.iter().map(|e| e.file_id).collect();
    let mut all_edges: Vec<DependencyEdge> = extra_edges.to_vec();

    for wf in files {
        if covered_files.contains(&wf.file_id) {
            continue;
        }
        let Some(ext) = wf.path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let Some(lang) = Language::from_extension(ext) else {
            continue;
        };
        match lang {
            Language::Python => extract_python_edges(wf, &mut all_edges),
            Language::JsTs => extract_jsts_edges(wf, &mut all_edges),
            Language::Java => extract_java_fallback_edges(wf, &mut all_edges),
        }
    }

    evaluate_architecture(config, &all_edges)
}

fn python_module_from_rel_path(rel_path: &str) -> String {
    let norm = rel_path
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();
    let stripped = norm
        .strip_suffix("/__init__.py")
        .or_else(|| norm.strip_suffix(".py"))
        .or_else(|| norm.strip_suffix(".pyi"))
        .unwrap_or(&norm);
    stripped.replace('/', ".")
}

fn resolve_python_relative_import(from_module: &str, dots: usize, rest: &str) -> String {
    let parts: Vec<&str> = from_module.split('.').collect();
    // 1 dot = current package (drop leaf module), 2 dots = parent package, etc.
    let drop = dots.min(parts.len());
    let base = &parts[..parts.len().saturating_sub(drop)];
    let mut out: Vec<&str> = base.to_vec();
    if !rest.is_empty() {
        out.extend(rest.split('.').filter(|s| !s.is_empty()));
    }
    out.join(".")
}

fn extract_python_edges(wf: &WorkspaceFile, out: &mut Vec<DependencyEdge>) {
    let from_module = python_module_from_rel_path(&wf.relative_path);
    let mut offset = 0usize;
    for raw_line in wf.source.split_inclusive('\n') {
        let line_len = raw_line.len();
        let trimmed = raw_line.trim_start();
        let indent = raw_line.len() - trimmed.len();
        let code = trimmed.split('#').next().unwrap_or("").trim_end();
        let line_start = (offset + indent) as u32;
        let line_end = (offset + indent + code.len()) as u32;

        if let Some(rest) = code.strip_prefix("import ") {
            for item in rest.split(',') {
                let target = item.split_whitespace().next().unwrap_or("").trim();
                if !target.is_empty() {
                    out.push(DependencyEdge {
                        file_id: wf.file_id,
                        language: Language::Python,
                        from_module: from_module.clone(),
                        to_target: target.to_string(),
                        edge_kind: "import".to_string(),
                        range: TextRange::new(line_start, line_end),
                    });
                }
            }
        } else if let Some(rest) = code.strip_prefix("from ")
            && let Some((mod_part, _)) = rest.split_once(" import ")
        {
            let mod_trimmed = mod_part.trim();
            let dots = mod_trimmed.bytes().take_while(|&b| b == b'.').count();
            let target = if dots > 0 {
                resolve_python_relative_import(&from_module, dots, &mod_trimmed[dots..])
            } else {
                mod_trimmed.to_string()
            };
            if !target.is_empty() {
                out.push(DependencyEdge {
                    file_id: wf.file_id,
                    language: Language::Python,
                    from_module: from_module.clone(),
                    to_target: target,
                    edge_kind: "import".to_string(),
                    range: TextRange::new(line_start, line_end),
                });
            }
        }

        offset += line_len;
    }
}

fn normalize_jsts_module(rel_path: &str) -> String {
    let norm = rel_path
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();
    for ext in [
        "/index.ts",
        "/index.tsx",
        "/index.js",
        "/index.jsx",
        ".ts",
        ".tsx",
        ".mts",
        ".cts",
        ".js",
        ".jsx",
        ".mjs",
        ".cjs",
    ] {
        if let Some(stripped) = norm.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    norm
}

fn resolve_jsts_specifier(from_rel: &str, spec: &str) -> String {
    if spec.starts_with("./") || spec.starts_with("../") {
        let parent = Path::new(from_rel)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let joined = parent.join(spec);
        let mut stack: Vec<String> = Vec::new();
        for comp in joined.components() {
            match comp {
                std::path::Component::Normal(s) => stack.push(s.to_string_lossy().to_string()),
                std::path::Component::ParentDir => {
                    stack.pop();
                }
                _ => {}
            }
        }
        return normalize_jsts_module(&stack.join("/"));
    }
    spec.to_string()
}

fn extract_quoted_specifier(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'\'' || b == b'"' {
            let quote = b;
            if let Some(end_rel) = bytes[idx + 1..].iter().position(|&c| c == quote) {
                return Some(&s[idx + 1..idx + 1 + end_rel]);
            }
        }
    }
    None
}

fn extract_jsts_edges(wf: &WorkspaceFile, out: &mut Vec<DependencyEdge>) {
    let from_module = normalize_jsts_module(&wf.relative_path);
    let rel_norm = wf
        .relative_path
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string();
    let mut offset = 0usize;

    for raw_line in wf.source.split_inclusive('\n') {
        let line_len = raw_line.len();
        let trimmed = raw_line.trim_start();
        let indent = raw_line.len() - trimmed.len();
        let code = trimmed.trim_end_matches(['\r', '\n']);
        let line_start = (offset + indent) as u32;
        let line_end = (offset + indent + code.len()) as u32;

        if code.starts_with("import ") || code.starts_with("export ") {
            let spec_opt = if let Some((_, after_from)) = code.split_once(" from ") {
                extract_quoted_specifier(after_from)
            } else if code.starts_with("import ") {
                extract_quoted_specifier(code)
            } else {
                None
            };
            if let Some(spec) = spec_opt {
                let target = resolve_jsts_specifier(&rel_norm, spec);
                let kind = if code.starts_with("export ") {
                    "export-from"
                } else {
                    "import"
                };
                out.push(DependencyEdge {
                    file_id: wf.file_id,
                    language: Language::JsTs,
                    from_module: from_module.clone(),
                    to_target: target,
                    edge_kind: kind.to_string(),
                    range: TextRange::new(line_start, line_end),
                });
            }
        } else if let Some(req_idx) = code.find("require(")
            && let Some(spec) = extract_quoted_specifier(&code[req_idx + 8..])
        {
            let target = resolve_jsts_specifier(&rel_norm, spec);
            out.push(DependencyEdge {
                file_id: wf.file_id,
                language: Language::JsTs,
                from_module: from_module.clone(),
                to_target: target,
                edge_kind: "require".to_string(),
                range: TextRange::new(line_start, line_end),
            });
        }

        offset += line_len;
    }
}

fn extract_java_fallback_edges(wf: &WorkspaceFile, out: &mut Vec<DependencyEdge>) {
    let mut pkg = String::new();
    let mut offset = 0usize;
    let mut imports: Vec<(String, TextRange)> = Vec::new();

    for raw_line in wf.source.split_inclusive('\n') {
        let line_len = raw_line.len();
        let trimmed = raw_line.trim_start();
        let indent = raw_line.len() - trimmed.len();
        let code = trimmed.split("//").next().unwrap_or("").trim();
        let line_start = (offset + indent) as u32;
        let line_end = (offset + indent + code.len()) as u32;

        if let Some(rest) = code.strip_prefix("package ")
            && let Some(p) = rest.strip_suffix(';')
        {
            pkg = p.trim().to_string();
        } else if let Some(rest) = code.strip_prefix("import ")
            && let Some(imp) = rest.strip_suffix(';')
        {
            let cleaned = imp
                .trim()
                .strip_prefix("static ")
                .unwrap_or(imp.trim())
                .trim();
            if !cleaned.is_empty() {
                imports.push((cleaned.to_string(), TextRange::new(line_start, line_end)));
            }
        }
        offset += line_len;
    }

    for (target, range) in imports {
        out.push(DependencyEdge {
            file_id: wf.file_id,
            language: Language::Java,
            from_module: pkg.clone(),
            to_target: target,
            edge_kind: "import".to_string(),
            range,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use riffty_core::architecture::{ArchForbidRule, ArchLayer};
    use std::path::PathBuf;

    fn wf(id: u32, rel: &str, src: &str) -> WorkspaceFile {
        WorkspaceFile {
            file_id: FileId(id),
            path: PathBuf::from(rel),
            relative_path: rel.to_string(),
            source: src.to_string(),
        }
    }

    #[test]
    fn enforces_python_and_jsts_layers_and_cycles() {
        let cfg = ArchitectureConfig {
            forbid: vec![ArchForbidRule {
                id: "PY-NO-LEGACY".to_string(),
                from: "app.**".to_string(),
                target: vec!["legacy_sdk.**".to_string()],
                message: Some("Do not use deprecated legacy_sdk".to_string()),
            }],
            layers: vec![
                ArchLayer {
                    name: "domain".to_string(),
                    matches: vec!["app.domain.**".to_string(), "src/domain/**".to_string()],
                    may_access: vec![],
                },
                ArchLayer {
                    name: "infra".to_string(),
                    matches: vec!["app.infra.**".to_string(), "src/infra/**".to_string()],
                    may_access: vec!["domain".to_string()],
                },
            ],
            forbid_cycles: true,
            cycle_slice_depth: None,
        };

        let files = vec![
            wf(
                1,
                "app/domain/order.py",
                "import legacy_sdk.client\nfrom app.infra.db import save\n",
            ),
            wf(2, "app/infra/db.py", "from app.domain.order import Order\n"),
            wf(
                3,
                "src/domain/user.ts",
                "import { repo } from '../infra/repo';\n",
            ),
            wf(
                4,
                "src/infra/repo.ts",
                "import type { User } from '../domain/user';\n",
            ),
        ];

        let diags = evaluate_workspace_architecture(&cfg, &files, &[]);
        let ids: Vec<&str> = diags.iter().map(|d| d.rule_id.as_str()).collect();
        assert!(ids.contains(&"PY-NO-LEGACY"), "got: {diags:#?}");
        assert!(
            ids.iter().filter(|&&id| id == "ARCH-LAYER-DOMAIN").count() == 2,
            "expected domain->infra violation in both Python and TS; got: {diags:#?}"
        );
        assert!(ids.contains(&"ARCH-CYCLE-001"), "got: {diags:#?}");
    }
}

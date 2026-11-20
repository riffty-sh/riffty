use std::collections::HashSet;

use oxc_allocator::Allocator;
use oxc_ast::ast::{ImportDeclarationSpecifier, Statement};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType};
use riffty_core::{Edit, ImportManagerPort, ImportSpec, TextRange};

/// Surgical ESM `import` manager powered by `oxc_parser` + `oxc_semantic` (`REQ-M1-03`).
///
/// Guarantees:
/// - Zero import reordering churn.
/// - Preserves leading directives (`'use strict'`, `'use client'`, `'use server'`) and shebangs.
/// - Checks lexical scope bindings to avoid emitting conflicting imports when a symbol is
///   already imported or shadowed in the root scope.
#[derive(Default, Debug, Clone, Copy)]
pub struct OxcImportAdapter;

impl ImportManagerPort for OxcImportAdapter {
    fn resolve_import_edits(&self, source: &str, required: &[ImportSpec]) -> Vec<Edit> {
        if required.is_empty() {
            return Vec::new();
        }

        let allocator = Allocator::default();
        let source_type = SourceType::tsx().with_module(true);
        let parsed = Parser::new(&allocator, source, source_type).parse();
        let semantic_ret = SemanticBuilder::new().build(&parsed.program);
        let scoping = semantic_ret.semantic.scoping();
        let root_scope = scoping.root_scope_id();

        let mut existing_root_bindings: HashSet<String> = HashSet::new();
        for (name, _) in scoping.get_bindings(root_scope) {
            existing_root_bindings.insert(name.to_string());
        }

        // Track existing named import declarations by module specifier:
        // module -> (insert_offset_before_closing_brace, has_trailing_comma, existing_local_names)
        let mut named_import_sites: Vec<(String, u32, bool)> = Vec::new();
        let mut last_import_end: Option<u32> = None;

        for stmt in &parsed.program.body {
            if let Statement::ImportDeclaration(decl) = stmt {
                last_import_end = Some(decl.span.end);
                let Some(module_name) = decl.source.value.as_str().map(str::to_string) else {
                    continue;
                };
                if let Some(specifiers) = &decl.specifiers {
                    let mut last_named_end: Option<u32> = None;
                    for spec in specifiers {
                        if let ImportDeclarationSpecifier::ImportSpecifier(named) = spec {
                            last_named_end = Some(named.span.end);
                        }
                    }
                    if let Some(last_end) = last_named_end {
                        // Locate closing `}` after `last_end` inside the import declaration.
                        let decl_end = (decl.span.end as usize).min(source.len());
                        let search_start = (last_end as usize).min(decl_end);
                        if let Some(rel_brace) = source[search_start..decl_end].find('}') {
                            let brace_pos = (search_start + rel_brace) as u32;
                            let between = source[search_start..brace_pos as usize].trim();
                            let has_trailing_comma = between.ends_with(',');
                            named_import_sites.push((module_name, last_end, has_trailing_comma));
                        }
                    }
                }
            }
        }

        let insert_pos = compute_esm_import_insertion_offset(source, &parsed.program, last_import_end);

        let mut edits = Vec::new();
        let mut already_planned: HashSet<String> = HashSet::new();
        let mut new_import_lines: Vec<String> = Vec::new();

        for spec in required {
            let Some(symbol) = spec.symbol.as_deref() else {
                continue;
            };
            let local_name = symbol;
            // Shadow safety: skip if symbol is already imported or bound in root scope.
            if existing_root_bindings.contains(local_name) || already_planned.contains(local_name) {
                continue;
            }
            already_planned.insert(local_name.to_string());

            let specifier_text = symbol.to_string();

            if let Some((_, last_named_end, has_trailing_comma)) = named_import_sites
                .iter()
                .find(|(m, _, _)| m == &spec.module)
            {
                let replacement = if *has_trailing_comma {
                    format!(" {specifier_text},")
                } else {
                    format!(", {specifier_text}")
                };
                edits.push(Edit {
                    range: TextRange::new(*last_named_end, *last_named_end),
                    replacement,
                });
            } else {
                new_import_lines.push(format!(
                    "import {{ {specifier_text} }} from \"{}\";\n",
                    spec.module
                ));
            }
        }

        if !new_import_lines.is_empty() {
            let mut block = String::new();
            if insert_pos > 0 {
                let prev = source.as_bytes().get((insert_pos - 1) as usize).copied();
                if prev != Some(b'\n') {
                    block.push('\n');
                }
            }
            for line in new_import_lines {
                block.push_str(&line);
            }
            edits.push(Edit {
                range: TextRange::new(insert_pos, insert_pos),
                replacement: block,
            });
        }

        edits
    }
}

fn compute_esm_import_insertion_offset(
    source: &str,
    program: &oxc_ast::ast::Program<'_>,
    last_import_end: Option<u32>,
) -> u32 {
    if let Some(end) = last_import_end {
        return advance_past_line_ending(source, end);
    }

    // Preserve `'use strict'`, `'use client'`, `'use server'` directives at the top.
    if let Some(last_dir) = program.directives.last() {
        return advance_past_line_ending(source, last_dir.span().end);
    }

    // Preserve shebang `#!...` if present.
    if let Some(hashbang) = &program.hashbang {
        return advance_past_line_ending(source, hashbang.span.end);
    }

    // Otherwise insert at start of first statement (or 0 if empty).
    if let Some(first_stmt) = program.body.first() {
        return first_stmt.span().start;
    }

    0
}

fn advance_past_line_ending(source: &str, offset: u32) -> u32 {
    let bytes = source.as_bytes();
    let mut pos = (offset as usize).min(bytes.len());
    while pos < bytes.len() && bytes[pos] != b'\n' {
        pos += 1;
    }
    if pos < bytes.len() && bytes[pos] == b'\n' {
        pos += 1;
    }
    pos as u32
}

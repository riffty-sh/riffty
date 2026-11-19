use std::collections::HashMap;

use ast_grep_core::Pattern;
use ast_grep_language::{LanguageExt, Python};
use riffty_core::{CapturedMetavar, PatternMatch, PatternMatcherPort, TextRange};

/// Primary `PatternMatcherPort` adapter powered by `ast-grep-core` + `ast-grep-language::Python`.
#[derive(Default, Debug, Clone, Copy)]
pub struct AstGrepAdapter;

impl PatternMatcherPort for AstGrepAdapter {
    fn find_matches(&self, source: &str, pattern: &str) -> Result<Vec<PatternMatch>, String> {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            return Err("pattern must not be empty".to_string());
        }
        Pattern::try_new(trimmed, Python)
            .map_err(|e| format!("invalid ast-grep pattern `{trimmed}`: {e}"))?;

        let (metavars, variadic_vars) = extract_pattern_variables(trimmed);
        let mut candidate_queries = vec![(trimmed.to_string(), None)];
        if let Some((zero_var_query, omitted_var)) = strip_trailing_comma_variadic(trimmed) {
            candidate_queries.push((zero_var_query, Some(omitted_var)));
        }

        let grep = Python.ast_grep(source);
        let mut results: Vec<PatternMatch> = Vec::new();

        for (query_str, omitted_variadic) in candidate_queries {
            let Ok(pat) = Pattern::try_new(&query_str, Python) else {
                continue;
            };
            for node_match in grep.root().find_all(&pat) {
                let r = node_match.range();
                let full_range = TextRange::new(r.start as u32, r.end as u32);
                if results.iter().any(|existing| existing.range == full_range) {
                    continue;
                }

                let env = node_match.get_env();
                let mut captures = HashMap::new();

                for var in &metavars {
                    if let Some(m) = env.get_match(var) {
                        let mr = m.range();
                        let tr = TextRange::new(mr.start as u32, mr.end as u32);
                        captures.insert(
                            var.clone(),
                            CapturedMetavar {
                                name: var.clone(),
                                range: tr,
                                is_variadic: false,
                                item_ranges: vec![tr],
                            },
                        );
                    }
                }

                for var in &variadic_vars {
                    if omitted_variadic.as_deref() == Some(var.as_str()) {
                        let empty_pos = full_range.end.0.saturating_sub(1);
                        captures.insert(
                            var.clone(),
                            CapturedMetavar {
                                name: var.clone(),
                                range: TextRange::new(empty_pos, empty_pos),
                                is_variadic: true,
                                item_ranges: Vec::new(),
                            },
                        );
                        continue;
                    }

                    let multi = env.get_multiple_matches(var);
                    let named_items: Vec<TextRange> = multi
                        .iter()
                        .filter(|n| n.is_named())
                        .map(|n| {
                            let nr = n.range();
                            TextRange::new(nr.start as u32, nr.end as u32)
                        })
                        .collect();

                    let span = match (multi.first(), multi.last()) {
                        (Some(first), Some(last)) => {
                            let mut s = first.range().start;
                            let mut e = last.range().end;
                            while s < e && matches!(source.as_bytes()[s], b',' | b' ' | b'\t') {
                                s += 1;
                            }
                            while e > s && matches!(source.as_bytes()[e - 1], b',' | b' ' | b'\t') {
                                e -= 1;
                            }
                            TextRange::new(s as u32, e as u32)
                        }
                        _ => TextRange::new(full_range.end.0, full_range.end.0),
                    };

                    captures.insert(
                        var.clone(),
                        CapturedMetavar {
                            name: var.clone(),
                            range: span,
                            is_variadic: true,
                            item_ranges: named_items,
                        },
                    );
                }

                results.push(PatternMatch {
                    range: full_range,
                    captures,
                });
            }
        }

        results.sort_by_key(|m| (m.range.start, m.range.end));
        Ok(results)
    }
}

pub fn extract_pattern_variables(pattern: &str) -> (Vec<String>, Vec<String>) {
    let mut single = Vec::new();
    let mut variadic = Vec::new();
    let bytes = pattern.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i..].starts_with(b"$$$") {
            let start = i + 3;
            let mut end = start;
            while end < bytes.len() && is_metavar_char(bytes[end]) {
                end += 1;
            }
            if end > start {
                let name = pattern[start..end].to_string();
                if !variadic.contains(&name) {
                    variadic.push(name);
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
                let name = pattern[start..end].to_string();
                if !single.contains(&name) {
                    single.push(name);
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    (single, variadic)
}

fn strip_trailing_comma_variadic(raw: &str) -> Option<(String, String)> {
    for close in [')', ']', '}'] {
        if let Some(without_close) = raw.strip_suffix(close)
            && let Some(comma_idx) = without_close.rfind(',')
        {
            let tail = without_close[comma_idx + 1..].trim();
            if let Some(var_name) = tail.strip_prefix("$$$")
                && !var_name.is_empty()
                && var_name.bytes().all(is_metavar_char)
            {
                let mut rebuilt = without_close[..comma_idx].trim_end().to_string();
                rebuilt.push(close);
                return Some((rebuilt, var_name.to_string()));
            }
        }
    }
    None
}

const fn is_metavar_char(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'
}

use riffty_core::{
    AnalysisView, Applicability, CallMeta, CanonicalNode, CanonicalSyntaxTree, CanonicalType,
    CompareOp, Diagnostic, Edit, Fix, ImportSpec, NodeId, NodeKind, NodeMeta, RulePlugin, Severity,
    TextRange, are_types_provably_disjoint,
};

/// Returns all Tier-1 zero-false-positive native Rust `RulePlugin`s (`docs/ach.md` §3 & §7, `R20`):
/// - `RFT-004`: `UnawaitedCoroutineRule`
/// - `RFT-015`: `CollectionIncompatibleTypeRule`
/// - `RFT-118`: `WarningsMissingStacklevelRule`
/// - `RFT-119`: `WarningsNarrowSkipFilePrefixesRule`
/// - `RFT-301` / `RFT-202`: `AssertWarnsMigrationRule`
/// - `RFT-302`: `SqlAlchemyLegacySessionQueryRule`
pub fn builtin_rules() -> Vec<Box<dyn RulePlugin>> {
    vec![
        Box::new(UnawaitedCoroutineRule),
        Box::new(CollectionIncompatibleTypeRule),
        Box::new(WarningsMissingStacklevelRule),
        Box::new(WarningsNarrowSkipFilePrefixesRule),
        Box::new(AssertWarnsMigrationRule),
        Box::new(SqlAlchemyLegacySessionQueryRule),
    ]
}

/// `RFT-004`: Flags bare expression-statement calls to `Callable { is_async: true }`
/// that are neither `await`ed, `return`ed, nor assigned (`R12`).
#[derive(Default, Debug, Clone, Copy)]
pub struct UnawaitedCoroutineRule;

impl RulePlugin for UnawaitedCoroutineRule {
    fn id(&self) -> &'static str {
        "RFT-004"
    }

    fn target_node_kinds(&self) -> &[NodeKind] {
        const TARGETS: &[NodeKind] = &[NodeKind::Call];
        TARGETS
    }

    fn evaluate(&self, node: &CanonicalNode, view: &AnalysisView) -> Vec<Diagnostic> {
        let Some(NodeMeta::Call(call_meta)) = view.tree.node_meta(node.id) else {
            return Vec::new();
        };
        if !call_meta.is_expr_stmt {
            return Vec::new();
        }

        let callee_ty = view.oracle.type_of(view.file_id, call_meta.callee_range);
        if callee_ty.is_unknown_or_any() {
            // `R03`: Zero-False-Positive Soundness
            return Vec::new();
        }

        let CanonicalType::Callable {
            is_async: true,
            return_type,
            ..
        } = callee_ty
        else {
            return Vec::new();
        };

        let callee_name = call_meta
            .callee_range
            .slice(&view.tree.source)
            .unwrap_or(node.raw_text);

        let inside_async_fn = has_enclosing_async_function(view.tree, node.parent);
        let fix = if inside_async_fn {
            Some(Fix {
                title: format!("Await coroutine `{callee_name}(...)`."),
                edits: vec![Edit {
                    range: TextRange::new(node.range.start.0, node.range.start.0),
                    replacement: "await ".to_string(),
                }],
                required_imports: Vec::new(),
                applicability: Applicability::Unsafe,
            })
        } else {
            None
        };

        vec![Diagnostic {
            rule_id: "RFT-004".to_string(),
            rule_name: "unawaited-coroutine".to_string(),
            severity: Severity::Error,
            file_id: view.file_id,
            range: node.range,
            message: format!(
                "Coroutine call `{}` is never awaited; the coroutine object will be discarded without executing.",
                node.raw_text
            ),
            type_proof: Some(serde_json::json!({
                "callee": callee_name,
                "is_async": true,
                "return_type": return_type.display_short(),
            })),
            fix,
        }]
    }
}

fn has_enclosing_async_function(tree: &CanonicalSyntaxTree, mut current: Option<NodeId>) -> bool {
    while let Some(id) = current {
        let Some(n) = tree.node(id) else {
            break;
        };
        match n.kind {
            NodeKind::AsyncFunctionDef => return true,
            NodeKind::FunctionDef => return false,
            _ => current = n.parent,
        }
    }
    false
}

/// `RFT-015`: Flags `x in container` and `x not in container` when `x` and container element type
/// are concrete, provably disjoint types (`R13`).
#[derive(Default, Debug, Clone, Copy)]
pub struct CollectionIncompatibleTypeRule;

impl RulePlugin for CollectionIncompatibleTypeRule {
    fn id(&self) -> &'static str {
        "RFT-015"
    }

    fn target_node_kinds(&self) -> &[NodeKind] {
        const TARGETS: &[NodeKind] = &[NodeKind::Compare];
        TARGETS
    }

    fn evaluate(&self, node: &CanonicalNode, view: &AnalysisView) -> Vec<Diagnostic> {
        let Some(NodeMeta::Compare(cmp_meta)) = view.tree.node_meta(node.id) else {
            return Vec::new();
        };

        let mut diagnostics = Vec::new();
        for (idx, &(op, rhs_range)) in cmp_meta.comparators.iter().enumerate() {
            if !matches!(op, CompareOp::In | CompareOp::NotIn) {
                continue;
            }
            let lhs_range = if idx == 0 {
                cmp_meta.left_range
            } else {
                cmp_meta.comparators[idx - 1].1
            };

            let lhs_ty = view.oracle.type_of(view.file_id, lhs_range);
            let rhs_ty = view.oracle.type_of(view.file_id, rhs_range);

            // `R03`: Suppress whenever either operand is `Unknown` or `Any`
            if lhs_ty.is_unknown_or_any() || rhs_ty.is_unknown_or_any() {
                continue;
            }

            let Some(elem_ty) = rhs_ty.container_element_type() else {
                continue;
            };
            if elem_ty.is_unknown_or_any() {
                continue;
            }

            if !are_types_provably_disjoint(&lhs_ty, &elem_ty, view.oracle) {
                continue;
            }

            let op_str = if op == CompareOp::In { "in" } else { "not in" };
            let lhs_text = lhs_range.slice(&view.tree.source).unwrap_or("<expr>");
            let rhs_text = rhs_range.slice(&view.tree.source).unwrap_or("<container>");

            diagnostics.push(Diagnostic {
                rule_id: "RFT-015".to_string(),
                rule_name: "collection-incompatible-type".to_string(),
                severity: Severity::Error,
                file_id: view.file_id,
                range: node.range,
                message: format!(
                    "Membership test `{lhs_text} {op_str} {rhs_text}` compares disjoint types `{}` and container element `{}` (always `{}`).",
                    lhs_ty.display_short(),
                    elem_ty.display_short(),
                    op == CompareOp::NotIn,
                ),
                type_proof: Some(serde_json::json!({
                    "left_type": lhs_ty.display_short(),
                    "container_type": rhs_ty.display_short(),
                    "element_type": elem_ty.display_short(),
                })),
                fix: None,
            });
        }

        diagnostics
    }
}

/// `RFT-118`: Injects `stacklevel=2` into `warnings.warn(...)` calls that omit `stacklevel=`
/// and do not unpack `**kwargs` (`R11`).
#[derive(Default, Debug, Clone, Copy)]
pub struct WarningsMissingStacklevelRule;

impl RulePlugin for WarningsMissingStacklevelRule {
    fn id(&self) -> &'static str {
        "RFT-118"
    }

    fn target_node_kinds(&self) -> &[NodeKind] {
        const TARGETS: &[NodeKind] = &[NodeKind::Call];
        TARGETS
    }

    fn evaluate(&self, node: &CanonicalNode, view: &AnalysisView) -> Vec<Diagnostic> {
        let Some(NodeMeta::Call(call_meta)) = view.tree.node_meta(node.id) else {
            return Vec::new();
        };

        let Some(fqn) = view
            .oracle
            .resolve_fqname(view.file_id, call_meta.callee_range)
        else {
            return Vec::new();
        };
        if fqn != "warnings.warn" && fqn != "_warnings.warn" {
            return Vec::new();
        }

        // Suppress if `stacklevel=` or Python 3.12+ `skip_file_prefixes=` is already provided,
        // or if `**kwargs` unpacking is present (`R11`, `R20`).
        for kw in &call_meta.keyword_args {
            match kw.name.as_deref() {
                Some("stacklevel" | "skip_file_prefixes") | None => return Vec::new(),
                Some(_) => {}
            }
        }

        // Require at least one argument (positional or keyword `message=`).
        if call_meta.positional_args.is_empty()
            && call_meta.starred_args.is_empty()
            && call_meta.keyword_args.is_empty()
        {
            return Vec::new();
        }

        let Some(edit) = synthesize_stacklevel_insertion(node.range, &view.tree.source) else {
            return Vec::new();
        };

        vec![Diagnostic {
            rule_id: "RFT-118".to_string(),
            rule_name: "warnings-missing-stacklevel".to_string(),
            severity: Severity::Warning,
            file_id: view.file_id,
            range: node.range,
            message: "`warnings.warn(...)` called without `stacklevel=`; warning points to `warn()` site instead of caller.".to_string(),
            type_proof: Some(serde_json::json!({
                "resolved_fqname": fqn,
                "injected_kwarg": "stacklevel=2",
            })),
            fix: Some(Fix {
                title: "Added stacklevel=2 to warnings.warn()".to_string(),
                edits: vec![edit],
                required_imports: Vec::new(),
                applicability: Applicability::Safe,
            }),
        }]
    }
}

fn synthesize_stacklevel_insertion(call_range: TextRange, source: &str) -> Option<Edit> {
    let raw = call_range.slice(source)?;
    if !raw.ends_with(')') {
        return None;
    }
    let inner_end = raw.len() - 1;
    let trimmed_inner = raw[..inner_end].trim_end_matches([' ', '\t', '\r', '\n']);
    if trimmed_inner.ends_with(',') {
        let insert_at = call_range.start.0 + (trimmed_inner.len() as u32);
        Some(Edit {
            range: TextRange::new(insert_at, insert_at),
            replacement: " stacklevel=2".to_string(),
        })
    } else {
        let insert_at = call_range.start.0 + (trimmed_inner.len() as u32);
        Some(Edit {
            range: TextRange::new(insert_at, insert_at),
            replacement: ", stacklevel=2".to_string(),
        })
    }
}

/// `RFT-119`: Flags `warnings.warn(..., skip_file_prefixes=...)` inside internal helper subpackages
/// (`_lib/`, `_internal/`, `_shared/`, `_compat/`) when `skip_file_prefixes` only skips a single
/// `dirname(__file__)` / `Path(__file__).parent` directory instead of the enclosing package root (`R20`).
#[derive(Default, Debug, Clone, Copy)]
pub struct WarningsNarrowSkipFilePrefixesRule;

impl RulePlugin for WarningsNarrowSkipFilePrefixesRule {
    fn id(&self) -> &'static str {
        "RFT-119"
    }

    fn target_node_kinds(&self) -> &[NodeKind] {
        const TARGETS: &[NodeKind] = &[NodeKind::Call];
        TARGETS
    }

    fn evaluate(&self, node: &CanonicalNode, view: &AnalysisView) -> Vec<Diagnostic> {
        if !is_internal_helper_subpackage(view.file_path) {
            return Vec::new();
        }
        let Some(NodeMeta::Call(call_meta)) = view.tree.node_meta(node.id) else {
            return Vec::new();
        };
        let Some(fqn) = view
            .oracle
            .resolve_fqname(view.file_id, call_meta.callee_range)
        else {
            return Vec::new();
        };
        if fqn != "warnings.warn" && fqn != "_warnings.warn" {
            return Vec::new();
        }

        let Some(skip_kw) = call_meta
            .keyword_args
            .iter()
            .find(|kw| kw.name.as_deref() == Some("skip_file_prefixes"))
        else {
            return Vec::new();
        };
        let Some(val_text) = skip_kw.value_range.slice(&view.tree.source) else {
            return Vec::new();
        };
        if !is_single_level_dunder_file_dir(val_text) {
            return Vec::new();
        }

        vec![Diagnostic {
            rule_id: "RFT-119".to_string(),
            rule_name: "warnings-narrow-skip-file-prefixes".to_string(),
            severity: Severity::Warning,
            file_id: view.file_id,
            range: node.range,
            message: format!(
                "`warnings.warn(..., skip_file_prefixes={val_text})` inside internal helper `{}` only skips the immediate subpackage directory; callers through sibling subpackages will see internal frames instead of user call sites.",
                view.file_path
            ),
            type_proof: Some(serde_json::json!({
                "resolved_fqname": fqn,
                "file_path": view.file_path,
                "skip_file_prefixes_expr": val_text,
            })),
            fix: None,
        }]
    }
}

fn is_internal_helper_subpackage(file_path: &str) -> bool {
    let norm = file_path.replace('\\', "/");
    ["/_lib/", "/_internal/", "/_shared/", "/_compat/"]
        .iter()
        .any(|seg| norm.contains(seg))
        || ["_lib/", "_internal/", "_shared/", "_compat/"]
            .iter()
            .any(|prefix| norm.starts_with(prefix))
}

fn is_single_level_dunder_file_dir(raw: &str) -> bool {
    if !raw.contains("__file__") || raw.contains("..") || raw.contains(".parents[") {
        return false;
    }
    let dirname_count = raw.matches("dirname(").count();
    if dirname_count == 1 {
        return true;
    }
    let parent_count = raw.matches(".parent").count();
    dirname_count == 0 && parent_count == 1
}

/// `RFT-202` + `RFT-301` (`AssertWarnsMigrationRule`, SciPy #20068):
/// Migrates `np.testing.assert_warns(W, fn, *args, match=...)` -> `with pytest.warns(W, match=...): fn(*args)`.
/// Queries `TypeOraclePort` on `fn` (`R10`):
/// - When `match=` is passed: classifies `Fix` as `Applicability::Safe` ONLY if `fn` resolves to a
///   `CanonicalType::Callable` that does NOT accept `match` and has `has_var_kwargs == false`;
///   otherwise classifies as `Applicability::Unsafe`.
#[derive(Default, Debug, Clone, Copy)]
pub struct AssertWarnsMigrationRule;

impl RulePlugin for AssertWarnsMigrationRule {
    fn id(&self) -> &'static str {
        "RFT-301"
    }

    fn target_node_kinds(&self) -> &[NodeKind] {
        const TARGETS: &[NodeKind] = &[NodeKind::Call];
        TARGETS
    }

    fn evaluate(&self, node: &CanonicalNode, view: &AnalysisView) -> Vec<Diagnostic> {
        let Some(NodeMeta::Call(call_meta)) = view.tree.node_meta(node.id) else {
            return Vec::new();
        };

        let Some(fqn) = view
            .oracle
            .resolve_fqname(view.file_id, call_meta.callee_range)
        else {
            return Vec::new();
        };
        if fqn != "numpy.testing.assert_warns" && fqn != "numpy.testing._private.utils.assert_warns"
        {
            return Vec::new();
        }

        // Order positional & starred args by exact source byte offset
        let mut ordered_pos: Vec<(TextRange, bool)> = call_meta
            .positional_args
            .iter()
            .map(|&r| (r, false))
            .chain(call_meta.starred_args.iter().map(|&r| (r, true)))
            .collect();
        ordered_pos.sort_by_key(|(r, _)| (r.start, r.end));

        // Context-manager form (`with assert_warns(W, match=...):` / `with assert_warns(W):`, SciPy #20068):
        if ordered_pos.len() == 1 && !call_meta.is_expr_stmt {
            let (warn_cls_range, warn_starred) = ordered_pos[0];
            if warn_starred {
                return Vec::new();
            }
            // Only rewrite when the only optional keyword arg is `match=` (no unknown kwargs).
            if call_meta
                .keyword_args
                .iter()
                .any(|kw| kw.name.as_deref() != Some("match"))
            {
                return Vec::new();
            }
            let Some(warn_cls_text) = warn_cls_range.slice(&view.tree.source) else {
                return Vec::new();
            };
            let has_match = call_meta
                .keyword_args
                .iter()
                .any(|kw| kw.name.as_deref() == Some("match"));
            let fix = Some(Fix {
                title: if has_match {
                    format!(
                        "Migrate context manager to `pytest.warns({warn_cls_text}, match=...)`."
                    )
                } else {
                    format!("Migrate context manager to `pytest.warns({warn_cls_text})`.")
                },
                edits: vec![Edit {
                    range: call_meta.callee_range,
                    replacement: "pytest.warns".to_string(),
                }],
                required_imports: vec![ImportSpec {
                    module: "pytest".to_string(),
                    symbol: None,
                }],
                applicability: Applicability::Safe,
            });

            if has_match {
                return vec![Diagnostic {
                    rule_id: "RFT-301".to_string(),
                    rule_name: "assert-warns-unexpected-match-kwarg".to_string(),
                    severity: Severity::Error,
                    file_id: view.file_id,
                    range: node.range,
                    message: format!(
                        "`np.testing.assert_warns({warn_cls_text}, match=...)` context manager ignores `match=` regex filtering; migrate to `pytest.warns`."
                    ),
                    type_proof: Some(serde_json::json!({
                        "resolved_fqname": fqn,
                        "context_manager": true,
                        "ignored_kwarg": "match",
                    })),
                    fix,
                }];
            } else {
                return vec![Diagnostic {
                    rule_id: "RFT-202".to_string(),
                    rule_name: "deprecated-numpy-assert-warns".to_string(),
                    severity: Severity::Warning,
                    file_id: view.file_id,
                    range: node.range,
                    message: format!(
                        "`np.testing.assert_warns` context manager is deprecated; migrate to `pytest.warns({warn_cls_text})`."
                    ),
                    type_proof: Some(serde_json::json!({
                        "resolved_fqname": fqn,
                        "context_manager": true,
                    })),
                    fix,
                }];
            }
        }

        if ordered_pos.len() < 2 {
            return Vec::new();
        }
        let (warn_cls_range, warn_starred) = ordered_pos[0];
        let (target_fn_range, fn_starred) = ordered_pos[1];
        if warn_starred || fn_starred {
            return Vec::new();
        }

        let source = &view.tree.source;
        let Some(warn_cls_text) = warn_cls_range.slice(source) else {
            return Vec::new();
        };
        let Some(target_fn_text) = target_fn_range.slice(source) else {
            return Vec::new();
        };

        let match_kw = call_meta
            .keyword_args
            .iter()
            .find(|kw| kw.name.as_deref() == Some("match"));

        // Collect all arguments forwarded to `fn(...)` (positional[2..] + non-`match` keyword args)
        let mut forwarded_ranges: Vec<TextRange> =
            ordered_pos[2..].iter().map(|(r, _)| *r).collect();
        for kw in &call_meta.keyword_args {
            if kw.name.as_deref() != Some("match") {
                forwarded_ranges.push(kw.full_range);
            }
        }
        forwarded_ranges.sort_by_key(|r| (r.start, r.end));

        let mut forwarded_pieces = Vec::with_capacity(forwarded_ranges.len());
        for r in &forwarded_ranges {
            let Some(piece) = r.slice(source) else {
                return Vec::new();
            };
            forwarded_pieces.push(piece);
        }
        let forwarded_args_str = forwarded_pieces.join(", ");

        // Preserve any `# ...` comments that existed inside a multi-line `assert_warns(...)` call
        let mut kept_spans = vec![call_meta.callee_range, warn_cls_range, target_fn_range];
        kept_spans.extend(forwarded_ranges.iter().copied());
        if let Some(mkw) = match_kw {
            kept_spans.push(mkw.full_range);
        }
        let interior_comments = extract_interior_comments(source, node.range, &kept_spans);

        let indent = view.tree.line_indent_at(node.range.start);
        let warns_header = if let Some(mkw) = match_kw {
            let match_val = mkw.value_range.slice(source).unwrap_or("...");
            format!("with pytest.warns({warn_cls_text}, match={match_val}):")
        } else {
            format!("with pytest.warns({warn_cls_text}):")
        };

        let mut replacement = String::new();
        for c in &interior_comments {
            replacement.push_str(c);
            replacement.push('\n');
            replacement.push_str(indent);
        }
        replacement.push_str(&warns_header);
        replacement.push('\n');
        replacement.push_str(indent);
        replacement.push_str("    ");
        replacement.push_str(target_fn_text);
        replacement.push('(');
        replacement.push_str(&forwarded_args_str);
        replacement.push(')');

        if let Some(_mkw) = match_kw {
            let fn_ty = view.oracle.type_of(view.file_id, target_fn_range);
            build_rft_301_diagnostic(
                node,
                call_meta,
                view.file_id,
                warn_cls_text,
                target_fn_text,
                &fn_ty,
                replacement,
            )
        } else {
            let fix = if call_meta.is_expr_stmt {
                Some(Fix {
                    title: format!("Migrate to `with pytest.warns({warn_cls_text}):`."),
                    edits: vec![Edit {
                        range: node.range,
                        replacement,
                    }],
                    required_imports: vec![ImportSpec {
                        module: "pytest".to_string(),
                        symbol: None,
                    }],
                    applicability: Applicability::Safe,
                })
            } else {
                None
            };

            vec![Diagnostic {
                rule_id: "RFT-202".to_string(),
                rule_name: "deprecated-numpy-assert-warns".to_string(),
                severity: Severity::Warning,
                file_id: view.file_id,
                range: node.range,
                message: format!(
                    "`np.testing.assert_warns` is deprecated; migrate `{target_fn_text}` assertion to `with pytest.warns({warn_cls_text}):`."
                ),
                type_proof: Some(serde_json::json!({
                    "resolved_fqname": fqn,
                    "target_callable": target_fn_text,
                })),
                fix,
            }]
        }
    }
}

fn build_rft_301_diagnostic(
    node: &CanonicalNode,
    call_meta: &CallMeta,
    file_id: riffty_core::FileId,
    warn_cls_text: &str,
    target_fn_text: &str,
    fn_ty: &CanonicalType,
    replacement: String,
) -> Vec<Diagnostic> {
    let fix_title = format!("Migrate to `with pytest.warns({warn_cls_text}, match=...):`.");

    match fn_ty {
        CanonicalType::Callable {
            params,
            has_var_kwargs,
            ..
        } => {
            let accepted_params: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
            let accepts_match_param = params.iter().any(|p| p.name == "match");

            if !accepts_match_param && !has_var_kwargs {
                // Provably safe (`R10`): `fn` rejects `match=`, so `match=` was unambiguously meant for `pytest.warns`!
                let fix = if call_meta.is_expr_stmt {
                    Some(Fix {
                        title: fix_title,
                        edits: vec![Edit {
                            range: node.range,
                            replacement,
                        }],
                        required_imports: vec![ImportSpec {
                            module: "pytest".to_string(),
                            symbol: None,
                        }],
                        applicability: Applicability::Safe,
                    })
                } else {
                    None
                };

                vec![Diagnostic {
                    rule_id: "RFT-301".to_string(),
                    rule_name: "assert-warns-unexpected-match-kwarg".to_string(),
                    severity: Severity::Error,
                    file_id,
                    range: node.range,
                    message: format!(
                        "np.testing.assert_warns forwards `match=` to `{target_fn_text}`, which does not accept `match`."
                    ),
                    type_proof: Some(serde_json::json!({
                        "target_callable": target_fn_text,
                        "accepted_params": accepted_params,
                        "unexpected_kwarg": "match",
                    })),
                    fix,
                }]
            } else {
                // `fn` either has `**kwargs` (SciPy #20068 silent swallow!) or defines `match`: downgrade Fix to `Unsafe` (`R10`).
                let fix = if call_meta.is_expr_stmt {
                    Some(Fix {
                        title: fix_title,
                        edits: vec![Edit {
                            range: node.range,
                            replacement,
                        }],
                        required_imports: vec![ImportSpec {
                            module: "pytest".to_string(),
                            symbol: None,
                        }],
                        applicability: Applicability::Unsafe,
                    })
                } else {
                    None
                };

                let message = if *has_var_kwargs && !accepts_match_param {
                    format!(
                        "np.testing.assert_warns forwards `match=` into `{target_fn_text}` (`**kwargs` wrapper), silently swallowing `match=` without filtering the warning."
                    )
                } else {
                    format!(
                        "np.testing.assert_warns forwards `match=` to `{target_fn_text}`, which accepts `match`; verify whether `match=` belongs to `pytest.warns` or `{target_fn_text}`."
                    )
                };

                vec![Diagnostic {
                    rule_id: "RFT-301".to_string(),
                    rule_name: "assert-warns-ambiguous-match-kwarg".to_string(),
                    severity: Severity::Error,
                    file_id,
                    range: node.range,
                    message,
                    type_proof: Some(serde_json::json!({
                        "target_callable": target_fn_text,
                        "accepted_params": accepted_params,
                        "has_var_kwargs": has_var_kwargs,
                        "unexpected_kwarg": "match",
                    })),
                    fix,
                }]
            }
        }
        _ => {
            // Unresolved/Any callable (`R03` + `R10`): never assume `Safe`; downgrade Fix to `Unsafe`.
            let fix = if call_meta.is_expr_stmt {
                Some(Fix {
                    title: fix_title,
                    edits: vec![Edit {
                        range: node.range,
                        replacement,
                    }],
                    required_imports: vec![ImportSpec {
                        module: "pytest".to_string(),
                        symbol: None,
                    }],
                    applicability: Applicability::Unsafe,
                })
            } else {
                None
            };

            vec![Diagnostic {
                rule_id: "RFT-301".to_string(),
                rule_name: "assert-warns-ambiguous-match-kwarg".to_string(),
                severity: Severity::Error,
                file_id,
                range: node.range,
                message: format!(
                    "np.testing.assert_warns forwards `match=` to `{target_fn_text}` (signature unknown); migration requires verification."
                ),
                type_proof: None,
                fix,
            }]
        }
    }
}

/// Scans `source[call_range]` outside string literals for `# ...` comments that are not already
/// inside any preserved argument slice, ensuring multi-line `assert_warns(...)` comments survive rewrites.
fn extract_interior_comments(
    source: &str,
    call_range: TextRange,
    preserved_spans: &[TextRange],
) -> Vec<String> {
    let mut comments = Vec::new();
    let start = call_range.start.0 as usize;
    let end = (call_range.end.0 as usize).min(source.len());
    let bytes = source.as_bytes();
    let mut i = start;

    while i < end {
        let b = bytes[i];
        if b == b'\'' || b == b'"' {
            let quote = b;
            let triple = i + 2 < end && bytes[i + 1] == quote && bytes[i + 2] == quote;
            if triple {
                i += 3;
                while i + 2 < end
                    && !(bytes[i] == quote && bytes[i + 1] == quote && bytes[i + 2] == quote)
                {
                    if bytes[i] == b'\\' {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                i = (i + 3).min(end);
            } else {
                i += 1;
                while i < end && bytes[i] != quote && bytes[i] != b'\n' {
                    if bytes[i] == b'\\' {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                i = (i + 1).min(end);
            }
            continue;
        }
        if b == b'#' {
            let comment_start = i;
            while i < end && bytes[i] != b'\n' {
                i += 1;
            }
            let comment_end = i;
            let c_range = TextRange::new(comment_start as u32, comment_end as u32);
            let inside_arg = preserved_spans.iter().any(|sp| sp.contains_range(c_range));
            if !inside_arg && let Some(text) = c_range.slice(source) {
                comments.push(text.trim_end_matches('\r').to_string());
            }
            continue;
        }
        i += 1;
    }
    comments
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use riffty_adapter_ruff_ast::{RuffAstAdapter, RuffImportAdapter};
    use riffty_adapter_ty::{NullTypeOracle, TyOracleAdapter};
    use riffty_core::{FileId, WorkspaceFile};
    use riffty_pipeline::{PipelineConfig, RifftyEngine};

    use super::*;

    fn run_builtin_check(source: &str, apply_fixes: bool) -> (String, Vec<Diagnostic>) {
        let parser = RuffAstAdapter;
        let oracle = TyOracleAdapter::new();
        let import_mgr = RuffImportAdapter;
        let engine = RifftyEngine {
            parser: &parser,
            oracle: &oracle,
            import_manager: &import_mgr,
            pattern_matcher: None,
            rules: builtin_rules(),
        };
        let mut res = engine
            .run_workspace(
                &[WorkspaceFile {
                    file_id: FileId(1),
                    path: PathBuf::from("test_target.py"),
                    relative_path: "test_target.py".to_string(),
                    source: source.to_string(),
                }],
                PipelineConfig {
                    apply_fixes,
                    allow_unsafe_fixes: false,
                    max_passes: 5,
                },
                None,
            )
            .expect("pipeline succeeds");
        let r = res.remove(0);
        (r.final_source, r.remaining_diagnostics)
    }

    #[test]
    fn rft_004_flags_unawaited_coroutine_and_ignores_awaited_returned_or_assigned() {
        let src = concat!(
            "async def send_audit(event: str) -> None:\n",
            "    pass\n",
            "\n",
            "async def handler() -> None:\n",
            "    await send_audit('ok')\n",
            "    task = send_audit('assigned')\n",
            "    send_audit('forgotten')\n",
            "\n",
            "def passthrough():\n",
            "    return send_audit('returned')\n",
        );
        let (_out, diags) = run_builtin_check(src, false);
        assert_eq!(diags.len(), 1, "expected exactly 1 RFT-004, got {diags:?}");
        assert_eq!(diags[0].rule_id, "RFT-004");
        assert!(diags[0].message.contains("send_audit('forgotten')"));
    }

    #[test]
    fn rft_015_flags_disjoint_membership_and_respects_numeric_tower_and_unknown() {
        let src = concat!(
            "def check(user_id: int, tags: set[str], untyped) -> bool:\n",
            "    ok_num = 1 in {1.0, 2.0}\n",
            "    ok_unknown = user_id in untyped\n",
            "    return user_id in tags\n",
        );
        let (_out, diags) = run_builtin_check(src, false);
        assert_eq!(diags.len(), 1, "expected exactly 1 RFT-015, got {diags:?}");
        assert_eq!(diags[0].rule_id, "RFT-015");
        assert_eq!(
            diags[0].type_proof,
            Some(serde_json::json!({
                "left_type": "int",
                "container_type": "set[str]",
                "element_type": "str",
            }))
        );
    }

    #[test]
    fn rft_118_injects_stacklevel_two_and_skips_existing_or_kwargs() {
        let src = concat!(
            "import warnings\n",
            "\n",
            "def deprecate(extra: dict) -> None:\n",
            "    warnings.warn('legacy api')  # keep comment\n",
            "    warnings.warn('already set', stacklevel=3)\n",
            "    warnings.warn('unpacked', **extra)\n",
        );
        let (patched, remaining) = run_builtin_check(src, true);
        assert!(
            remaining.is_empty(),
            "expected 0 remaining, got {remaining:?}"
        );
        assert!(
            patched.contains("warnings.warn('legacy api', stacklevel=2)  # keep comment"),
            "patched:\n{patched}"
        );
    }

    #[test]
    fn r03_null_oracle_suppresses_type_dependent_rules_with_zero_false_positives() {
        let src = concat!(
            "async def job() -> int:\n",
            "    return 1\n",
            "\n",
            "def run(x: int, s: set[str]) -> bool:\n",
            "    job()\n",
            "    return x in s\n",
        );
        let parser = RuffAstAdapter;
        let null_oracle = NullTypeOracle;
        let import_mgr = RuffImportAdapter;
        let engine = RifftyEngine {
            parser: &parser,
            oracle: &null_oracle,
            import_manager: &import_mgr,
            pattern_matcher: None,
            rules: builtin_rules(),
        };
        let res = engine
            .run_workspace(
                &[WorkspaceFile {
                    file_id: FileId(1),
                    path: PathBuf::from("untyped.py"),
                    relative_path: "untyped.py".to_string(),
                    source: src.to_string(),
                }],
                PipelineConfig::default(),
                None,
            )
            .unwrap();
        assert!(res[0].remaining_diagnostics.is_empty());
    }

    #[test]
    fn rft_301_context_manager_assert_warns_migrates_to_pytest_warns_preserving_comments() {
        let src = concat!(
            "from numpy.testing import assert_warns\n",
            "\n",
            "def test_constant_input() -> None:\n",
            "    msg = 'An input array is constant'\n",
            "    with assert_warns(RuntimeWarning, match=msg):  # keep inline comment\n",
            "        _ = 1 + 1\n",
        );
        let (patched, remaining) = run_builtin_check(src, true);
        assert!(
            remaining.is_empty(),
            "expected 0 remaining, got {remaining:?}"
        );
        assert!(patched.contains("import pytest\n"), "patched:\n{patched}");
        assert!(
            patched
                .contains("with pytest.warns(RuntimeWarning, match=msg):  # keep inline comment"),
            "patched:\n{patched}"
        );
    }

    #[test]
    fn r20_rft_118_respects_skip_file_prefixes_and_rft_119_flags_narrow_internal_helper() {
        let internal_src = concat!(
            "import os\n",
            "import warnings\n",
            "\n",
            "def warn_narrow() -> None:\n",
            "    warnings.warn('scalar deprecated', DeprecationWarning, skip_file_prefixes=(os.path.dirname(__file__),))\n",
            "\n",
            "def warn_wide() -> None:\n",
            "    warnings.warn('ok wide', DeprecationWarning, skip_file_prefixes=(os.path.dirname(os.path.dirname(__file__)),))\n",
        );
        let root_src = concat!(
            "import os\n",
            "import warnings\n",
            "\n",
            "def warn_pkg_root() -> None:\n",
            "    warnings.warn('ok root', DeprecationWarning, skip_file_prefixes=(os.path.dirname(__file__),))\n",
        );

        let parser = RuffAstAdapter;
        let oracle = TyOracleAdapter::new();
        let import_mgr = RuffImportAdapter;
        let engine = RifftyEngine {
            parser: &parser,
            oracle: &oracle,
            import_manager: &import_mgr,
            pattern_matcher: None,
            rules: builtin_rules(),
        };

        let results = engine
            .run_workspace(
                &[
                    WorkspaceFile {
                        file_id: FileId(1),
                        path: PathBuf::from("scipy/_lib/_util.py"),
                        relative_path: "scipy/_lib/_util.py".to_string(),
                        source: internal_src.to_string(),
                    },
                    WorkspaceFile {
                        file_id: FileId(2),
                        path: PathBuf::from("scipy/optimize/api.py"),
                        relative_path: "scipy/optimize/api.py".to_string(),
                        source: root_src.to_string(),
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

        // RFT-118 MUST NOT inject dead `stacklevel=2` into either file when `skip_file_prefixes=` is present!
        assert_eq!(results[0].final_source, internal_src);
        assert_eq!(results[1].final_source, root_src);

        // RFT-119 MUST flag `warn_narrow` in `scipy/_lib/_util.py` and ignore `warn_wide` & `scipy/optimize/api.py`!
        assert_eq!(results[0].remaining_diagnostics.len(), 1);
        assert_eq!(results[0].remaining_diagnostics[0].rule_id, "RFT-119");
        assert!(results[1].remaining_diagnostics.is_empty());
    }

    #[test]
    fn r18_rft_015_eliminates_open_world_false_positives_and_catches_real_ast_keyword_bug() {
        let src = concat!(
            "import ast\n",
            "from enum import Enum\n",
            "from typing import Literal\n",
            "\n",
            "class Rule(str, Enum):\n",
            "    BP001 = 'BP001'\n",
            "\n",
            "class CustomComparable:\n",
            "    def __eq__(self, other: object) -> bool:\n",
            "        return isinstance(other, str)\n",
            "\n",
            "def check_false_positives(\n",
            "    r: Literal[Rule.BP001],\n",
            "    disabled: set[str],\n",
            "    custom_items: list[CustomComparable],\n",
            ") -> tuple[bool, bool, bool]:\n",
            "    a = r in disabled\n",
            "    b = 'hello' in custom_items\n",
            "    c = tuple in (list, tuple)\n",
            "    return a, b, c\n",
            "\n",
            "def check_airflow_db_manager_bug(call_node: ast.Call) -> bool:\n",
            "    kws: list[ast.keyword] = call_node.keywords\n",
            "    return 'version_table' not in kws\n",
        );
        let (_out, diags) = run_builtin_check(src, false);
        assert_eq!(
            diags.len(),
            1,
            "Expected 0 FPs on StrEnum/custom __eq__/type tuple and 1 real RFT-015 on ast.keyword; got {diags:?}"
        );
        assert_eq!(diags[0].rule_id, "RFT-015");
        assert!(diags[0].message.contains("'version_table' not in kws"));
    }

    #[test]
    fn rft_302_sqlalchemy_legacy_session_query_migrations_and_negative_guard() {
        let src = concat!(
            "from sqlalchemy.orm import Session\n\n",
            "class User:\n",
            "    id: int\n",
            "    name: str\n",
            "    active: bool\n\n",
            "class CustomClient:\n",
            "    def query(self, model):\n",
            "        return self\n",
            "    def all(self):\n",
            "        return []\n\n",
            "def run(session: Session, client: CustomClient):\n",
            "    users = session.query(User).all()\n",
            "    cols = session.query(User.id, User.name).all()\n",
            "    uid = session.query(User.id).filter(User.active).scalar()\n",
            "    one = session.query(User).filter(User.id == 1).one_or_none()\n",
            "    other = client.query(User).all()\n",
            "    return users, cols, uid, one, other\n",
        );
        let (_out_check, diags) = run_builtin_check(src, false);
        let rft302_diags: Vec<_> = diags.iter().filter(|d| d.rule_id == "RFT-302").collect();
        assert_eq!(
            rft302_diags.len(),
            4,
            "Expected 4 RFT-302 diagnostics, got: {rft302_diags:?}"
        );
        let (out, _remaining) = run_builtin_check(src, true);
        assert!(out.contains("from sqlalchemy import select"), "got:\n{out}");
        assert!(
            out.contains("users = session.scalars(select(User)).all()"),
            "got:\n{out}"
        );
        assert!(
            out.contains("cols = session.execute(select(User.id, User.name)).all()"),
            "got:\n{out}"
        );
        assert!(
            out.contains("uid = session.scalar(select(User.id).where(User.active))"),
            "got:\n{out}"
        );
        assert!(
            out.contains("one = session.scalars(select(User).where(User.id == 1)).one_or_none()"),
            "got:\n{out}"
        );
        assert!(
            out.contains("other = client.query(User).all()"),
            "got:\n{out}"
        );
    }

    #[test]
    fn issue_6_rft_015_distinguishes_variadic_tuples_from_fixed_1_tuples_and_reproduces_ols() {
        let src = concat!(
            "import subprocess\n",
            "\n",
            "def test_e2e_cluster_scripts(\n",
            "    result: subprocess.CompletedProcess[str],\n",
            "    expected_targets: list[tuple[str, str, str, str, str]],\n",
            "    lines: list[str],\n",
            "    explicit_suites: list[tuple[str, ...]],\n",
            "    list_of_1_tuples: list[tuple[int]],\n",
            ") -> None:\n",
            "    # 1. Exact openshift/lightspeed-service reproduction (0 diagnostics expected)\n",
            "    suites = [tuple(line.split(':')) for line in result.stdout.splitlines()]\n",
            "    inspection_suites = [\n",
            "        suite for suite in suites if suite[3].startswith('mcp_inspection')\n",
            "    ]\n",
            "    expected_runs = {\n",
            "        (name, provider, model, suffix)\n",
            "        for name, provider, _credentials, model, suffix in expected_targets\n",
            "    }\n",
            "    assert len(inspection_suites) == len(expected_runs)\n",
            "    assert set(inspection_suites) == expected_runs\n",
            "    assert ('openai', 'openai', 'gpt-6-luna', 'default') in suites\n",
            "\n",
            "    # 2. Inline comprehension & explicit variadic annotation (0 diagnostics expected)\n",
            "    assert ('a', 'b', 'c', 'd') in [tuple(line.split(':')) for line in lines]\n",
            "    assert ('x', 'y') in explicit_suites\n",
            "\n",
            "    # 3. True incompatible element type against variadic tuple (1 RFT-015 diagnostic)\n",
            "    bad_elem = (1, 2, 3) in [tuple(line.split(':')) for line in lines]\n",
            "\n",
            "    # 4. True fixed-arity 1-tuple mismatch preserved (1 RFT-015 diagnostic)\n",
            "    bad_arity = (1, 2) in list_of_1_tuples\n",
            "    _ = (bad_elem, bad_arity)\n",
        );

        let (_out, diags) = run_builtin_check(src, false);
        assert_eq!(
            diags.len(),
            2,
            "Expected 0 FPs on variadic str tuples and 2 real RFT-015 diagnostics, got: {diags:#?}"
        );

        assert_eq!(diags[0].rule_id, "RFT-015");
        assert_eq!(
            diags[0].type_proof,
            Some(serde_json::json!({
                "left_type": "tuple[int, int, int]",
                "container_type": "list[tuple[str, ...]]",
                "element_type": "tuple[str, ...]",
            }))
        );

        assert_eq!(diags[1].rule_id, "RFT-015");
        assert_eq!(
            diags[1].type_proof,
            Some(serde_json::json!({
                "left_type": "tuple[int, int]",
                "container_type": "list[tuple[int]]",
                "element_type": "tuple[int]",
            }))
        );
    }
}

/// `RFT-302` (`sqlalchemy-legacy-session-query`):
/// Migrates legacy `session.query(...)` chains on `sqlalchemy.orm.Session` to SQLAlchemy 2.0
/// `session.scalars(select(...))`, `session.execute(select(...))`, or `session.scalar(select(...))`.
#[derive(Default, Debug, Clone, Copy)]
pub struct SqlAlchemyLegacySessionQueryRule;

impl RulePlugin for SqlAlchemyLegacySessionQueryRule {
    fn id(&self) -> &'static str {
        "RFT-302"
    }

    fn target_node_kinds(&self) -> &[NodeKind] {
        const TARGETS: &[NodeKind] = &[NodeKind::Call];
        TARGETS
    }

    fn evaluate(&self, node: &CanonicalNode<'_>, view: &AnalysisView<'_>) -> Vec<Diagnostic> {
        let Some(NodeMeta::Call(outer_call)) = view.tree.node_meta(node.id) else {
            return Vec::new();
        };

        // Only fire on the outermost Call in a chained expression.
        if let Some(parent_id) = node.parent
            && let Some(parent) = view.tree.node(parent_id)
            && parent.kind == NodeKind::Attribute
            && let Some(grand_id) = parent.parent
            && let Some(NodeMeta::Call(grand_call)) = view.tree.node_meta(grand_id)
            && grand_call.callee_range.start == node.range.start
            && grand_call.callee_range.end > node.range.end
        {
            return Vec::new();
        }

        // Walk down the call chain from outermost Call to `$SESSION.query(...)`.
        struct ChainStep {
            method: String,
            args_text: String,
        }
        let mut steps: Vec<ChainStep> = Vec::new();
        let mut cur_range = node.range;
        let mut cur_call = outer_call.clone();

        let (session_range, query_args_text) = loop {
            let Some(callee_src) = cur_call.callee_range.slice(&view.tree.source) else {
                return Vec::new();
            };
            let Some(dot_rel) = callee_src.rfind('.') else {
                return Vec::new();
            };
            let attr_name = callee_src[dot_rel + 1..].trim();
            let recv_range = TextRange::new(
                cur_call.callee_range.start.0,
                cur_call.callee_range.start.0 + dot_rel as u32,
            );

            let args_text = if cur_range.end.0 > cur_call.callee_range.end.0 + 2 {
                TextRange::new(cur_call.callee_range.end.0 + 1, cur_range.end.0 - 1)
                    .slice(&view.tree.source)
                    .unwrap_or("")
                    .trim()
            } else {
                ""
            };

            if attr_name == "query" {
                break (recv_range, args_text.to_string());
            }

            if !matches!(
                attr_name,
                "all" | "first" | "scalar" | "one" | "one_or_none" | "filter"
            ) {
                return Vec::new();
            }

            steps.push(ChainStep {
                method: attr_name.to_string(),
                args_text: args_text.to_string(),
            });

            let Some(next_node) = view
                .tree
                .iter_nodes()
                .find(|n| n.kind == NodeKind::Call && n.range == recv_range)
            else {
                return Vec::new();
            };
            let Some(NodeMeta::Call(next_call)) = view.tree.node_meta(next_node.id) else {
                return Vec::new();
            };
            cur_range = next_node.range;
            cur_call = next_call.clone();
        };

        let session_ty = view.oracle.type_of(view.file_id, session_range);
        let session_nominal = CanonicalType::Nominal {
            fqname: "sqlalchemy.orm.Session".to_string(),
            args: vec![],
        };
        if view.oracle.is_subtype(&session_ty, &session_nominal) != riffty_core::TriState::Yes {
            return Vec::new();
        }

        let Some(session_src) = session_range.slice(&view.tree.source) else {
            return Vec::new();
        };

        // `steps` was collected outer-to-inner; reverse to inner-to-outer.
        steps.reverse();
        let is_terminal_scalar = steps.last().is_some_and(|s| s.method == "scalar");
        if is_terminal_scalar {
            steps.pop();
        }

        let is_multi_column = query_args_text.contains(',');
        let session_method = if is_terminal_scalar {
            "scalar"
        } else if is_multi_column {
            "execute"
        } else {
            "scalars"
        };

        let mut select_chain = format!("select({query_args_text})");
        let mut outer_suffix = String::new();
        for step in &steps {
            if step.method == "filter" {
                select_chain.push_str(&format!(".where({})", step.args_text));
            } else {
                outer_suffix.push_str(&format!(".{}({})", step.method, step.args_text));
            }
        }

        let replacement = format!("{session_src}.{session_method}({select_chain}){outer_suffix}");
        let fix = Fix {
            title: format!(
                "Migrate legacy `session.query(...)` to `{session_method}(select(...))`"
            ),
            edits: vec![Edit {
                range: node.range,
                replacement,
            }],
            required_imports: vec![ImportSpec {
                module: "sqlalchemy".to_string(),
                symbol: Some("select".to_string()),
            }],
            applicability: Applicability::Safe,
        };

        vec![Diagnostic {
            rule_id: "RFT-302".to_string(),
            rule_name: "sqlalchemy-legacy-session-query".to_string(),
            severity: Severity::Warning,
            file_id: view.file_id,
            range: node.range,
            message:
                "Legacy SQLAlchemy `session.query(...)` is deprecated in 2.0; use `select(...)`."
                    .to_string(),
            type_proof: Some(format!("session: {session_ty:?}").into()),
            fix: Some(fix),
        }]
    }
}

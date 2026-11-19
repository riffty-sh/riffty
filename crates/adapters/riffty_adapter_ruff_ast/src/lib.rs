use std::collections::{BTreeSet, HashSet};

use riffty_core::{
    CallMeta, CanonicalSyntaxTree, ClassMeta, CompareMeta, CompareOp, Edit, FileId, FunctionMeta,
    IfMeta, ImportManagerPort, ImportSpec, KeywordArgMeta, NodeId, NodeKind, NodeMeta,
    ScopedImportRequest, StoredNode, SyntaxParserPort, TextRange, WithItemMeta, WithMeta,
    extract_inline_suppressions,
};
use ruff_python_ast::{CmpOp, Expr, Stmt};
use ruff_python_codegen::Stylist;
use ruff_text_size::Ranged;

fn to_core_range(range: ruff_text_size::TextRange) -> TextRange {
    TextRange::new(range.start().to_u32(), range.end().to_u32())
}

/// Primary `SyntaxParserPort` adapter backed by Astral's `ruff_python_parser`.
#[derive(Default, Debug, Clone, Copy)]
pub struct RuffAstAdapter;

impl SyntaxParserPort for RuffAstAdapter {
    fn parse(&self, file_id: FileId, source: &str) -> Result<CanonicalSyntaxTree, String> {
        let parsed = ruff_python_parser::parse_module(source).map_err(|err| {
            format!(
                "Python syntax error at byte {}: {}",
                err.location.start().to_u32(),
                err.error
            )
        })?;
        let module = parsed.into_syntax();
        let mut builder = TreeBuilder {
            nodes: Vec::with_capacity(64),
        };
        let root_id = builder.push(
            NodeKind::Module,
            to_core_range(module.range),
            None,
            NodeMeta::None,
        );
        for stmt in &module.body {
            builder.visit_stmt(stmt, root_id);
        }
        let suppressions = extract_inline_suppressions(source);
        Ok(CanonicalSyntaxTree::new(
            file_id,
            source.to_string(),
            builder.nodes,
            suppressions,
        ))
    }
}

struct TreeBuilder {
    nodes: Vec<StoredNode>,
}

impl TreeBuilder {
    fn push(
        &mut self,
        kind: NodeKind,
        range: TextRange,
        parent: Option<NodeId>,
        meta: NodeMeta,
    ) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(StoredNode {
            id,
            kind,
            range,
            parent,
            children: Vec::new(),
            meta,
        });
        if let Some(pid) = parent {
            self.nodes[pid.0 as usize].children.push(id);
        }
        id
    }

    fn visit_stmt(&mut self, stmt: &Stmt, parent: NodeId) {
        match stmt {
            Stmt::FunctionDef(func) => {
                let kind = if func.is_async {
                    NodeKind::AsyncFunctionDef
                } else {
                    NodeKind::FunctionDef
                };
                let fn_id = self.push(
                    kind,
                    to_core_range(func.range),
                    Some(parent),
                    NodeMeta::Function(FunctionMeta {
                        name: func.name.id.to_string(),
                        name_range: to_core_range(func.name.range),
                        is_async: func.is_async,
                    }),
                );
                for dec in &func.decorator_list {
                    self.visit_expr(&dec.expression, fn_id, false);
                }
                for param in func
                    .parameters
                    .posonlyargs
                    .iter()
                    .chain(func.parameters.args.iter())
                    .chain(func.parameters.kwonlyargs.iter())
                {
                    if let Some(ann) = &param.parameter.annotation {
                        self.visit_expr(ann, fn_id, false);
                    }
                    if let Some(def) = &param.default {
                        self.visit_expr(def, fn_id, false);
                    }
                }
                if let Some(ret) = &func.returns {
                    self.visit_expr(ret, fn_id, false);
                }
                for s in &func.body {
                    self.visit_stmt(s, fn_id);
                }
            }
            Stmt::ClassDef(cls) => {
                let bases = cls
                    .arguments
                    .as_ref()
                    .map(|args| args.args.iter().map(|a| to_core_range(a.range())).collect())
                    .unwrap_or_default();
                let decorators = cls
                    .decorator_list
                    .iter()
                    .map(|dec| to_core_range(dec.expression.range()))
                    .collect();
                let cls_id = self.push(
                    NodeKind::ClassDef,
                    to_core_range(cls.range),
                    Some(parent),
                    NodeMeta::Class(ClassMeta {
                        name: cls.name.id.to_string(),
                        name_range: to_core_range(cls.name.range),
                        bases,
                        decorators,
                    }),
                );
                for dec in &cls.decorator_list {
                    self.visit_expr(&dec.expression, cls_id, false);
                }
                if let Some(args) = &cls.arguments {
                    for arg in &args.args {
                        self.visit_expr(arg, cls_id, false);
                    }
                    for kw in &args.keywords {
                        self.visit_expr(&kw.value, cls_id, false);
                    }
                }
                for s in &cls.body {
                    self.visit_stmt(s, cls_id);
                }
            }
            Stmt::Import(imp) => {
                self.push(
                    NodeKind::Import,
                    to_core_range(imp.range),
                    Some(parent),
                    NodeMeta::None,
                );
            }
            Stmt::ImportFrom(imp) => {
                self.push(
                    NodeKind::ImportFrom,
                    to_core_range(imp.range),
                    Some(parent),
                    NodeMeta::None,
                );
            }
            Stmt::Return(ret) => {
                let id = self.push(
                    NodeKind::Return,
                    to_core_range(ret.range),
                    Some(parent),
                    NodeMeta::None,
                );
                if let Some(val) = &ret.value {
                    self.visit_expr(val, id, false);
                }
            }
            Stmt::Expr(expr_stmt) => {
                // Direct standalone expression statement: mark `is_expr_stmt = true` for `RFT-004`.
                self.visit_expr(&expr_stmt.value, parent, true);
            }
            Stmt::Assign(assign) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(assign.range),
                    Some(parent),
                    NodeMeta::None,
                );
                for t in &assign.targets {
                    self.visit_expr(t, id, false);
                }
                self.visit_expr(&assign.value, id, false);
            }
            Stmt::AnnAssign(ann) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(ann.range),
                    Some(parent),
                    NodeMeta::None,
                );
                self.visit_expr(&ann.target, id, false);
                self.visit_expr(&ann.annotation, id, false);
                if let Some(val) = &ann.value {
                    self.visit_expr(val, id, false);
                }
            }
            Stmt::AugAssign(aug) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(aug.range),
                    Some(parent),
                    NodeMeta::None,
                );
                self.visit_expr(&aug.target, id, false);
                self.visit_expr(&aug.value, id, false);
            }
            Stmt::If(if_stmt) => {
                let id = self.push(
                    NodeKind::If,
                    to_core_range(if_stmt.range),
                    Some(parent),
                    NodeMeta::If(IfMeta {
                        test_range: to_core_range(if_stmt.test.range()),
                    }),
                );
                self.visit_expr(&if_stmt.test, id, false);
                for s in &if_stmt.body {
                    self.visit_stmt(s, id);
                }
                for clause in &if_stmt.elif_else_clauses {
                    if let Some(t) = &clause.test {
                        self.visit_expr(t, id, false);
                    }
                    for s in &clause.body {
                        self.visit_stmt(s, id);
                    }
                }
            }
            Stmt::For(for_stmt) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(for_stmt.range),
                    Some(parent),
                    NodeMeta::None,
                );
                self.visit_expr(&for_stmt.target, id, false);
                self.visit_expr(&for_stmt.iter, id, false);
                for s in &for_stmt.body {
                    self.visit_stmt(s, id);
                }
                for s in &for_stmt.orelse {
                    self.visit_stmt(s, id);
                }
            }
            Stmt::While(while_stmt) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(while_stmt.range),
                    Some(parent),
                    NodeMeta::None,
                );
                self.visit_expr(&while_stmt.test, id, false);
                for s in &while_stmt.body {
                    self.visit_stmt(s, id);
                }
            }
            Stmt::With(with_stmt) => {
                let items = with_stmt
                    .items
                    .iter()
                    .map(|item| WithItemMeta {
                        context_expr_range: to_core_range(item.context_expr.range()),
                        optional_vars_range: item
                            .optional_vars
                            .as_ref()
                            .map(|v| to_core_range(v.range())),
                    })
                    .collect();
                let id = self.push(
                    NodeKind::With,
                    to_core_range(with_stmt.range),
                    Some(parent),
                    NodeMeta::With(WithMeta {
                        is_async: with_stmt.is_async,
                        items,
                    }),
                );
                for item in &with_stmt.items {
                    self.visit_expr(&item.context_expr, id, false);
                    if let Some(v) = &item.optional_vars {
                        self.visit_expr(v, id, false);
                    }
                }
                for s in &with_stmt.body {
                    self.visit_stmt(s, id);
                }
            }
            Stmt::Try(try_stmt) => {
                let id = self.push(
                    NodeKind::Try,
                    to_core_range(try_stmt.range),
                    Some(parent),
                    NodeMeta::None,
                );
                for s in &try_stmt.body {
                    self.visit_stmt(s, id);
                }
                for h in &try_stmt.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(eh) = h;
                    for s in &eh.body {
                        self.visit_stmt(s, id);
                    }
                }
                for s in &try_stmt.orelse {
                    self.visit_stmt(s, id);
                }
                for s in &try_stmt.finalbody {
                    self.visit_stmt(s, id);
                }
            }
            Stmt::Assert(assert_stmt) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(assert_stmt.range),
                    Some(parent),
                    NodeMeta::None,
                );
                self.visit_expr(&assert_stmt.test, id, false);
                if let Some(msg) = &assert_stmt.msg {
                    self.visit_expr(msg, id, false);
                }
            }
            Stmt::Raise(raise_stmt) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(raise_stmt.range),
                    Some(parent),
                    NodeMeta::None,
                );
                if let Some(exc) = &raise_stmt.exc {
                    self.visit_expr(exc, id, false);
                }
            }
            other => {
                self.push(
                    NodeKind::Other,
                    to_core_range(other.range()),
                    Some(parent),
                    NodeMeta::None,
                );
            }
        }
    }

    fn visit_expr(&mut self, expr: &Expr, parent: NodeId, is_expr_stmt: bool) {
        match expr {
            Expr::Call(call) => {
                let mut positional_args = Vec::new();
                let mut starred_args = Vec::new();
                for arg in &call.arguments.args {
                    if let Expr::Starred(st) = arg {
                        starred_args.push(to_core_range(st.range));
                    } else {
                        positional_args.push(to_core_range(arg.range()));
                    }
                }
                let keyword_args = call
                    .arguments
                    .keywords
                    .iter()
                    .map(|kw| KeywordArgMeta {
                        name: kw.arg.as_ref().map(|a| a.id.to_string()),
                        name_range: kw.arg.as_ref().map(|a| to_core_range(a.range)),
                        value_range: to_core_range(kw.value.range()),
                        full_range: to_core_range(kw.range),
                    })
                    .collect();

                let id = self.push(
                    NodeKind::Call,
                    to_core_range(call.range()),
                    Some(parent),
                    NodeMeta::Call(CallMeta {
                        callee_range: to_core_range(call.func.range()),
                        positional_args,
                        starred_args,
                        keyword_args,
                        is_expr_stmt,
                    }),
                );
                self.visit_expr(&call.func, id, false);
                for arg in &call.arguments.args {
                    self.visit_expr(arg, id, false);
                }
                for kw in &call.arguments.keywords {
                    self.visit_expr(&kw.value, id, false);
                }
            }
            Expr::Compare(cmp) => {
                let Some(left) = cmp.operands.first() else {
                    return;
                };
                let comparators = cmp
                    .ops
                    .iter()
                    .zip(cmp.operands.iter().skip(1))
                    .map(|(op, rhs)| {
                        let kind = match op {
                            CmpOp::In => CompareOp::In,
                            CmpOp::NotIn => CompareOp::NotIn,
                            CmpOp::Is => CompareOp::Is,
                            CmpOp::IsNot => CompareOp::IsNot,
                            CmpOp::Eq => CompareOp::Eq,
                            CmpOp::NotEq => CompareOp::NotEq,
                            _ => CompareOp::Other,
                        };
                        (kind, to_core_range(rhs.range()))
                    })
                    .collect();

                let id = self.push(
                    NodeKind::Compare,
                    to_core_range(cmp.range),
                    Some(parent),
                    NodeMeta::Compare(CompareMeta {
                        left_range: to_core_range(left.range()),
                        comparators,
                    }),
                );
                for opnd in &cmp.operands {
                    self.visit_expr(opnd, id, false);
                }
            }
            Expr::Attribute(attr) => {
                let id = self.push(
                    NodeKind::Attribute,
                    to_core_range(attr.range),
                    Some(parent),
                    NodeMeta::None,
                );
                self.visit_expr(&attr.value, id, false);
            }
            Expr::Name(name) => {
                self.push(
                    NodeKind::Name,
                    to_core_range(name.range),
                    Some(parent),
                    NodeMeta::None,
                );
            }
            Expr::Subscript(sub) => {
                let id = self.push(
                    NodeKind::Subscript,
                    to_core_range(sub.range),
                    Some(parent),
                    NodeMeta::None,
                );
                self.visit_expr(&sub.value, id, false);
                self.visit_expr(&sub.slice, id, false);
            }
            Expr::Await(aw) => {
                let id = self.push(
                    NodeKind::Await,
                    to_core_range(aw.range),
                    Some(parent),
                    NodeMeta::None,
                );
                self.visit_expr(&aw.value, id, false);
            }
            Expr::List(lst) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(lst.range),
                    Some(parent),
                    NodeMeta::None,
                );
                for e in &lst.elts {
                    self.visit_expr(e, id, false);
                }
            }
            Expr::Tuple(tup) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(tup.range),
                    Some(parent),
                    NodeMeta::None,
                );
                for e in &tup.elts {
                    self.visit_expr(e, id, false);
                }
            }
            Expr::Set(set) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(set.range),
                    Some(parent),
                    NodeMeta::None,
                );
                for e in &set.elts {
                    self.visit_expr(e, id, false);
                }
            }
            Expr::Dict(dict) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(dict.range),
                    Some(parent),
                    NodeMeta::None,
                );
                for item in &dict.items {
                    if let Some(k) = &item.key {
                        self.visit_expr(k, id, false);
                    }
                    self.visit_expr(&item.value, id, false);
                }
            }
            Expr::BinOp(bin) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(bin.range),
                    Some(parent),
                    NodeMeta::None,
                );
                self.visit_expr(&bin.left, id, false);
                self.visit_expr(&bin.right, id, false);
            }
            Expr::BoolOp(bop) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(bop.range),
                    Some(parent),
                    NodeMeta::None,
                );
                for v in &bop.values {
                    self.visit_expr(v, id, false);
                }
            }
            Expr::UnaryOp(uop) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(uop.range),
                    Some(parent),
                    NodeMeta::None,
                );
                self.visit_expr(&uop.operand, id, false);
            }
            Expr::If(if_expr) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(if_expr.range),
                    Some(parent),
                    NodeMeta::None,
                );
                self.visit_expr(&if_expr.test, id, false);
                self.visit_expr(&if_expr.body, id, false);
                self.visit_expr(&if_expr.orelse, id, false);
            }
            Expr::Named(named) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(named.range),
                    Some(parent),
                    NodeMeta::None,
                );
                self.visit_expr(&named.target, id, false);
                self.visit_expr(&named.value, id, false);
            }
            Expr::Starred(st) => {
                let id = self.push(
                    NodeKind::Other,
                    to_core_range(st.range),
                    Some(parent),
                    NodeMeta::None,
                );
                self.visit_expr(&st.value, id, false);
            }
            other => {
                self.push(
                    NodeKind::Other,
                    to_core_range(other.range()),
                    Some(parent),
                    NodeMeta::None,
                );
            }
        }
    }
}

/// Primary `ImportManagerPort` adapter backed by `ruff_python_parser` + `ruff_python_codegen::Stylist`.
/// Resolves `ImportSpec`s into exact zero-length UTF-8 byte `Edit`s (`R02`, `R06`) without pretty-printing the AST.
#[derive(Default, Debug, Clone, Copy)]
pub struct RuffImportAdapter;

fn merge_from_import_edit(
    source: &str,
    nl: &str,
    last_alias_start: u32,
    last_alias_end: u32,
    stmt_end: u32,
    symbols: &[&str],
) -> Edit {
    let after_last = &source[last_alias_end as usize..stmt_end as usize];
    if after_last.contains('\n') {
        // Multiline parenthesized `from pkg import (\n    a,\n)`
        let line_start = source[..last_alias_start as usize]
            .rfind('\n')
            .map_or(0, |p| p + 1);
        let indent: String = source[line_start..last_alias_start as usize]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let trimmed_after = after_last.trim_start_matches([' ', '\t']);
        if let Some(comma_rel) = after_last.find(',')
            && trimmed_after.starts_with(',')
        {
            let insert_at = last_alias_end + comma_rel as u32 + 1;
            let mut replacement = String::new();
            for sym in symbols {
                replacement.push_str(nl);
                replacement.push_str(&indent);
                replacement.push_str(sym);
                replacement.push(',');
            }
            return Edit {
                range: TextRange::new(insert_at, insert_at),
                replacement,
            };
        }
        let mut replacement = String::from(",");
        for sym in symbols {
            replacement.push_str(nl);
            replacement.push_str(&indent);
            replacement.push_str(sym);
            replacement.push(',');
        }
        return Edit {
            range: TextRange::new(last_alias_end, last_alias_end),
            replacement,
        };
    }

    Edit {
        range: TextRange::new(last_alias_end, last_alias_end),
        replacement: format!(", {}", symbols.join(", ")),
    }
}

impl ImportManagerPort for RuffImportAdapter {
    fn resolve_import_edits(&self, source: &str, imports: &[ImportSpec]) -> Vec<Edit> {
        if imports.is_empty() {
            return Vec::new();
        }
        let Ok(parsed) = ruff_python_parser::parse_module(source) else {
            return Vec::new();
        };
        let stylist = Stylist::from_tokens(parsed.tokens(), source);
        let nl: &str = stylist.line_ending().as_str();
        let module = parsed.into_syntax();

        let mut existing_modules: BTreeSet<String> = BTreeSet::new();
        let mut existing_from_symbols: BTreeSet<(String, String)> = BTreeSet::new();
        let mut existing_from_targets: std::collections::BTreeMap<String, (u32, u32, u32)> =
            std::collections::BTreeMap::new();
        let mut insert_offset = 0u32;
        let mut saw_docstring = false;
        let mut saw_any_header_import = false;

        for (idx, stmt) in module.body.iter().enumerate() {
            match stmt {
                Stmt::Expr(expr_stmt)
                    if idx == 0 && matches!(expr_stmt.value.as_ref(), Expr::StringLiteral(_)) =>
                {
                    saw_docstring = true;
                    insert_offset = advance_past_line_end(source, expr_stmt.range.end().to_u32());
                }
                Stmt::ImportFrom(imp_from) => {
                    let mod_name = imp_from
                        .module
                        .as_ref()
                        .map(|m| m.id.to_string())
                        .unwrap_or_default();
                    for alias in &imp_from.names {
                        if alias.asname.is_none() {
                            existing_from_symbols
                                .insert((mod_name.clone(), alias.name.id.to_string()));
                        }
                    }
                    if imp_from.level == 0
                        && !mod_name.is_empty()
                        && imp_from.names.iter().all(|a| a.name.id.as_str() != "*")
                        && let Some(last_alias) = imp_from.names.last()
                    {
                        existing_from_targets.entry(mod_name.clone()).or_insert((
                            last_alias.range.start().to_u32(),
                            last_alias.range.end().to_u32(),
                            imp_from.range.end().to_u32(),
                        ));
                    }
                    if mod_name == "__future__" || !saw_any_header_import {
                        insert_offset =
                            advance_past_line_end(source, imp_from.range.end().to_u32());
                    }
                    saw_any_header_import = true;
                }
                Stmt::Import(imp) => {
                    for alias in &imp.names {
                        if alias.asname.is_none() {
                            existing_modules.insert(alias.name.id.to_string());
                        }
                    }
                    if !saw_any_header_import {
                        insert_offset = advance_past_line_end(source, imp.range.end().to_u32());
                    }
                    saw_any_header_import = true;
                }
                _ => {
                    if idx == 0 {
                        insert_offset = first_code_offset_after_leading_comments(source);
                    }
                    break;
                }
            }
        }

        if module.body.is_empty() {
            insert_offset = first_code_offset_after_leading_comments(source);
        }

        let mut new_lines: BTreeSet<String> = BTreeSet::new();
        let mut merge_by_module: std::collections::BTreeMap<String, BTreeSet<String>> =
            std::collections::BTreeMap::new();
        for spec in imports {
            match &spec.symbol {
                None => {
                    if !existing_modules.contains(&spec.module) {
                        new_lines.insert(format!("import {}", spec.module));
                    }
                }
                Some(sym) => {
                    if !existing_from_symbols.contains(&(spec.module.clone(), sym.clone())) {
                        if existing_from_targets.contains_key(&spec.module) {
                            merge_by_module
                                .entry(spec.module.clone())
                                .or_default()
                                .insert(sym.clone());
                        } else {
                            new_lines.insert(format!("from {} import {sym}", spec.module));
                        }
                    }
                }
            }
        }

        let mut edits: Vec<Edit> = Vec::new();
        for (module_name, syms) in merge_by_module {
            if let Some(&(last_start, last_end, stmt_end)) = existing_from_targets.get(&module_name)
            {
                let sym_list: Vec<&str> = syms.iter().map(String::as_str).collect();
                edits.push(merge_from_import_edit(
                    source, nl, last_start, last_end, stmt_end, &sym_list,
                ));
            }
        }

        if new_lines.is_empty() {
            return edits;
        }

        let mut payload = String::new();
        if saw_docstring
            && !saw_any_header_import
            && insert_offset as usize <= source.len()
            && insert_offset > 0
        {
            payload.push_str(nl);
        }
        for line in new_lines {
            payload.push_str(&line);
            payload.push_str(nl);
        }
        if !saw_any_header_import
            && !source[insert_offset as usize..].is_empty()
            && !source[insert_offset as usize..].starts_with(['\r', '\n'])
        {
            payload.push_str(nl);
        }

        edits.push(Edit {
            range: TextRange::new(insert_offset, insert_offset),
            replacement: payload,
        });
        edits
    }

    fn resolve_scoped_import_edits(
        &self,
        tree: &CanonicalSyntaxTree,
        requests: &[ScopedImportRequest],
    ) -> Vec<Edit> {
        if requests.is_empty() {
            return Vec::new();
        }
        let source = tree.source.as_str();
        let Ok(parsed) = ruff_python_parser::parse_module(source) else {
            return Vec::new();
        };
        let module = parsed.into_syntax();

        let mut name_refs: Vec<(String, TextRange)> = Vec::new();
        collect_name_refs_in_stmts(&module.body, &mut name_refs);

        let mut functions: Vec<ScopedFnImports> = Vec::new();
        collect_scoped_functions(&module.body, &mut functions);

        let has_dunder_all = module_defines_dunder_all(&module.body);
        let mut top_single_from_imports: Vec<SingleFromImport> = Vec::new();
        let mut top_existing_from_symbols: HashSet<(String, String)> = HashSet::new();
        for stmt in &module.body {
            if let Stmt::ImportFrom(imp_from) = stmt
                && imp_from.level == 0
                && let Some(mod_id) = &imp_from.module
            {
                let mod_name = mod_id.id.to_string();
                for alias in &imp_from.names {
                    if alias.asname.is_none() {
                        top_existing_from_symbols
                            .insert((mod_name.clone(), alias.name.id.to_string()));
                    }
                }
                if imp_from.names.len() == 1 && imp_from.names[0].asname.is_none() {
                    top_single_from_imports.push(SingleFromImport {
                        stmt_range: to_core_range(imp_from.range),
                        module: mod_name,
                        symbol: imp_from.names[0].name.id.to_string(),
                    });
                }
            }
        }

        let mut in_place_edits: std::collections::BTreeMap<u32, Edit> =
            std::collections::BTreeMap::new();
        let mut unhandled_specs: Vec<ImportSpec> = Vec::new();
        let mut seen_unhandled: HashSet<ImportSpec> = HashSet::new();

        for req in requests {
            let mut satisfied = false;
            if let Some(new_sym) = &req.spec.symbol {
                // 1. Check innermost enclosing function scope for shadowing or matching local `ImportFrom` (`R19`).
                if let Some(fn_scope) = innermost_enclosing_fn(&functions, req.use_range) {
                    for local_imp in &fn_scope.local_from_imports {
                        if local_imp.stmt_range.end.0 > req.use_range.start.0 {
                            continue;
                        }
                        if local_imp.module == req.spec.module && local_imp.symbol == *new_sym {
                            satisfied = true;
                            break;
                        }
                        let same_bound_name = local_imp.symbol == *new_sym;
                        let matches_replaced = fqn_matches_import(
                            req.replaces_fqname.as_deref(),
                            &local_imp.module,
                            &local_imp.symbol,
                            &req.spec.module,
                        );
                        if (same_bound_name || matches_replaced)
                            && all_name_uses_migrated(
                                &name_refs,
                                &local_imp.symbol,
                                fn_scope.fn_range,
                                local_imp.stmt_range,
                                &fn_scope.nested_scope_ranges,
                                requests,
                                &req.spec,
                            )
                        {
                            in_place_edits.insert(
                                local_imp.stmt_range.start.0,
                                Edit {
                                    range: local_imp.stmt_range,
                                    replacement: format!(
                                        "from {} import {new_sym}",
                                        req.spec.module
                                    ),
                                },
                            );
                            satisfied = true;
                            break;
                        }
                    }
                }

                // 2. Check top-level single-symbol deprecated `ImportFrom` when all module references are migrated (`R19`).
                if !satisfied {
                    if top_existing_from_symbols
                        .contains(&(req.spec.module.clone(), new_sym.clone()))
                    {
                        satisfied = true;
                    } else if !has_dunder_all && req.replaces_fqname.is_some() {
                        let module_range = to_core_range(module.range);
                        for top_imp in &top_single_from_imports {
                            if !fqn_matches_import(
                                req.replaces_fqname.as_deref(),
                                &top_imp.module,
                                &top_imp.symbol,
                                &req.spec.module,
                            ) {
                                continue;
                            }
                            let shadowed_fn_ranges: Vec<TextRange> = functions
                                .iter()
                                .filter(|f| {
                                    f.local_from_imports
                                        .iter()
                                        .any(|li| li.symbol == top_imp.symbol)
                                })
                                .map(|f| f.fn_range)
                                .collect();
                            if has_at_least_one_name_use(
                                &name_refs,
                                &top_imp.symbol,
                                module_range,
                                top_imp.stmt_range,
                                &shadowed_fn_ranges,
                            ) && all_name_uses_migrated(
                                &name_refs,
                                &top_imp.symbol,
                                module_range,
                                top_imp.stmt_range,
                                &shadowed_fn_ranges,
                                requests,
                                &req.spec,
                            ) {
                                in_place_edits.insert(
                                    top_imp.stmt_range.start.0,
                                    Edit {
                                        range: top_imp.stmt_range,
                                        replacement: format!(
                                            "from {} import {new_sym}",
                                            req.spec.module
                                        ),
                                    },
                                );
                                satisfied = true;
                                break;
                            }
                        }
                    }
                }
            }

            if !satisfied && seen_unhandled.insert(req.spec.clone()) {
                unhandled_specs.push(req.spec.clone());
            }
        }

        let mut edits: Vec<Edit> = in_place_edits.into_values().collect();
        if !unhandled_specs.is_empty() {
            edits.extend(self.resolve_import_edits(source, &unhandled_specs));
        }
        edits.sort_by(|a, b| {
            a.range
                .start
                .cmp(&b.range.start)
                .then(a.range.end.cmp(&b.range.end))
        });
        edits
    }
}

#[derive(Debug, Clone)]
struct SingleFromImport {
    stmt_range: TextRange,
    module: String,
    symbol: String,
}

#[derive(Debug, Clone)]
struct ScopedFnImports {
    fn_range: TextRange,
    local_from_imports: Vec<SingleFromImport>,
    nested_scope_ranges: Vec<TextRange>,
}

fn innermost_enclosing_fn(
    functions: &[ScopedFnImports],
    use_range: TextRange,
) -> Option<&ScopedFnImports> {
    functions
        .iter()
        .filter(|f| f.fn_range.contains_range(use_range))
        .min_by_key(|f| f.fn_range.len())
}

fn fqn_matches_import(
    replaces_fqn: Option<&str>,
    imp_mod: &str,
    imp_sym: &str,
    target_mod: &str,
) -> bool {
    let Some(rep) = replaces_fqn else {
        return false;
    };
    let bound_fqn = format!("{imp_mod}.{imp_sym}");
    if imp_mod == target_mod {
        rep == bound_fqn
    } else {
        rep == bound_fqn || rep.starts_with(&format!("{bound_fqn}."))
    }
}

fn has_at_least_one_name_use(
    name_refs: &[(String, TextRange)],
    sym: &str,
    scope_range: TextRange,
    import_range: TextRange,
    excluded_subscopes: &[TextRange],
) -> bool {
    name_refs.iter().any(|(name, r)| {
        name == sym
            && scope_range.contains_range(*r)
            && !import_range.contains_range(*r)
            && !excluded_subscopes.iter().any(|sub| sub.contains_range(*r))
    })
}

fn all_name_uses_migrated(
    name_refs: &[(String, TextRange)],
    sym: &str,
    scope_range: TextRange,
    import_range: TextRange,
    excluded_subscopes: &[TextRange],
    requests: &[ScopedImportRequest],
    target_spec: &ImportSpec,
) -> bool {
    for (name, r) in name_refs {
        if name != sym
            || !scope_range.contains_range(*r)
            || import_range.contains_range(*r)
            || excluded_subscopes.iter().any(|sub| sub.contains_range(*r))
        {
            continue;
        }
        let covered = requests
            .iter()
            .any(|req| &req.spec == target_spec && req.use_range.contains_range(*r));
        if !covered {
            return false;
        }
    }
    true
}

fn module_defines_dunder_all(stmts: &[Stmt]) -> bool {
    for stmt in stmts {
        match stmt {
            Stmt::Assign(assign) => {
                if assign
                    .targets
                    .iter()
                    .any(|t| matches!(t, Expr::Name(n) if n.id.as_str() == "__all__"))
                {
                    return true;
                }
            }
            Stmt::AnnAssign(ann) => {
                if matches!(ann.target.as_ref(), Expr::Name(n) if n.id.as_str() == "__all__") {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn collect_scoped_functions(stmts: &[Stmt], out: &mut Vec<ScopedFnImports>) {
    for stmt in stmts {
        match stmt {
            Stmt::FunctionDef(func) => {
                let mut local_from_imports = Vec::new();
                let mut nested_scope_ranges = Vec::new();
                collect_fn_direct_imports(
                    &func.body,
                    &mut local_from_imports,
                    &mut nested_scope_ranges,
                );
                out.push(ScopedFnImports {
                    fn_range: to_core_range(func.range),
                    local_from_imports,
                    nested_scope_ranges,
                });
                collect_scoped_functions(&func.body, out);
            }
            Stmt::ClassDef(cls) => collect_scoped_functions(&cls.body, out),
            Stmt::If(if_stmt) => {
                collect_scoped_functions(&if_stmt.body, out);
                for cl in &if_stmt.elif_else_clauses {
                    collect_scoped_functions(&cl.body, out);
                }
            }
            Stmt::With(w) => collect_scoped_functions(&w.body, out),
            Stmt::For(f) => {
                collect_scoped_functions(&f.body, out);
                collect_scoped_functions(&f.orelse, out);
            }
            Stmt::While(w) => collect_scoped_functions(&w.body, out),
            Stmt::Try(t) => {
                collect_scoped_functions(&t.body, out);
                for h in &t.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(eh) = h;
                    collect_scoped_functions(&eh.body, out);
                }
                collect_scoped_functions(&t.orelse, out);
                collect_scoped_functions(&t.finalbody, out);
            }
            _ => {}
        }
    }
}

fn collect_fn_direct_imports(
    stmts: &[Stmt],
    local_imports: &mut Vec<SingleFromImport>,
    nested_scopes: &mut Vec<TextRange>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::ImportFrom(imp_from) => {
                if imp_from.level == 0
                    && imp_from.names.len() == 1
                    && imp_from.names[0].asname.is_none()
                    && let Some(mod_id) = &imp_from.module
                {
                    local_imports.push(SingleFromImport {
                        stmt_range: to_core_range(imp_from.range),
                        module: mod_id.id.to_string(),
                        symbol: imp_from.names[0].name.id.to_string(),
                    });
                }
            }
            Stmt::FunctionDef(f) => nested_scopes.push(to_core_range(f.range)),
            Stmt::ClassDef(c) => nested_scopes.push(to_core_range(c.range)),
            Stmt::If(if_stmt) => {
                collect_fn_direct_imports(&if_stmt.body, local_imports, nested_scopes);
                for cl in &if_stmt.elif_else_clauses {
                    collect_fn_direct_imports(&cl.body, local_imports, nested_scopes);
                }
            }
            Stmt::With(w) => collect_fn_direct_imports(&w.body, local_imports, nested_scopes),
            Stmt::For(f) => {
                collect_fn_direct_imports(&f.body, local_imports, nested_scopes);
                collect_fn_direct_imports(&f.orelse, local_imports, nested_scopes);
            }
            Stmt::While(w) => collect_fn_direct_imports(&w.body, local_imports, nested_scopes),
            Stmt::Try(t) => {
                collect_fn_direct_imports(&t.body, local_imports, nested_scopes);
                for h in &t.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(eh) = h;
                    collect_fn_direct_imports(&eh.body, local_imports, nested_scopes);
                }
                collect_fn_direct_imports(&t.orelse, local_imports, nested_scopes);
                collect_fn_direct_imports(&t.finalbody, local_imports, nested_scopes);
            }
            _ => {}
        }
    }
}

fn collect_name_refs_in_stmts(stmts: &[Stmt], out: &mut Vec<(String, TextRange)>) {
    for stmt in stmts {
        match stmt {
            Stmt::FunctionDef(func) => {
                for dec in &func.decorator_list {
                    collect_name_refs_in_expr(&dec.expression, out);
                }
                for param in func
                    .parameters
                    .posonlyargs
                    .iter()
                    .chain(func.parameters.args.iter())
                    .chain(func.parameters.kwonlyargs.iter())
                {
                    if let Some(ann) = &param.parameter.annotation {
                        collect_name_refs_in_expr(ann, out);
                    }
                    if let Some(def) = &param.default {
                        collect_name_refs_in_expr(def, out);
                    }
                }
                if let Some(ret) = &func.returns {
                    collect_name_refs_in_expr(ret, out);
                }
                collect_name_refs_in_stmts(&func.body, out);
            }
            Stmt::ClassDef(cls) => {
                for dec in &cls.decorator_list {
                    collect_name_refs_in_expr(&dec.expression, out);
                }
                if let Some(args) = &cls.arguments {
                    for a in &args.args {
                        collect_name_refs_in_expr(a, out);
                    }
                    for kw in &args.keywords {
                        collect_name_refs_in_expr(&kw.value, out);
                    }
                }
                collect_name_refs_in_stmts(&cls.body, out);
            }
            Stmt::Return(r) => {
                if let Some(v) = &r.value {
                    collect_name_refs_in_expr(v, out);
                }
            }
            Stmt::Expr(e) => collect_name_refs_in_expr(&e.value, out),
            Stmt::Assign(a) => {
                for t in &a.targets {
                    collect_name_refs_in_expr(t, out);
                }
                collect_name_refs_in_expr(&a.value, out);
            }
            Stmt::AnnAssign(a) => {
                collect_name_refs_in_expr(&a.target, out);
                collect_name_refs_in_expr(&a.annotation, out);
                if let Some(v) = &a.value {
                    collect_name_refs_in_expr(v, out);
                }
            }
            Stmt::AugAssign(a) => {
                collect_name_refs_in_expr(&a.target, out);
                collect_name_refs_in_expr(&a.value, out);
            }
            Stmt::If(if_stmt) => {
                collect_name_refs_in_expr(&if_stmt.test, out);
                collect_name_refs_in_stmts(&if_stmt.body, out);
                for cl in &if_stmt.elif_else_clauses {
                    if let Some(t) = &cl.test {
                        collect_name_refs_in_expr(t, out);
                    }
                    collect_name_refs_in_stmts(&cl.body, out);
                }
            }
            Stmt::For(f) => {
                collect_name_refs_in_expr(&f.target, out);
                collect_name_refs_in_expr(&f.iter, out);
                collect_name_refs_in_stmts(&f.body, out);
                collect_name_refs_in_stmts(&f.orelse, out);
            }
            Stmt::While(w) => {
                collect_name_refs_in_expr(&w.test, out);
                collect_name_refs_in_stmts(&w.body, out);
            }
            Stmt::With(w) => {
                for item in &w.items {
                    collect_name_refs_in_expr(&item.context_expr, out);
                    if let Some(v) = &item.optional_vars {
                        collect_name_refs_in_expr(v, out);
                    }
                }
                collect_name_refs_in_stmts(&w.body, out);
            }
            Stmt::Try(t) => {
                collect_name_refs_in_stmts(&t.body, out);
                for h in &t.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(eh) = h;
                    if let Some(ty) = &eh.type_ {
                        collect_name_refs_in_expr(ty, out);
                    }
                    collect_name_refs_in_stmts(&eh.body, out);
                }
                collect_name_refs_in_stmts(&t.orelse, out);
                collect_name_refs_in_stmts(&t.finalbody, out);
            }
            Stmt::Assert(a) => {
                collect_name_refs_in_expr(&a.test, out);
                if let Some(m) = &a.msg {
                    collect_name_refs_in_expr(m, out);
                }
            }
            Stmt::Raise(r) => {
                if let Some(e) = &r.exc {
                    collect_name_refs_in_expr(e, out);
                }
                if let Some(c) = &r.cause {
                    collect_name_refs_in_expr(c, out);
                }
            }
            _ => {}
        }
    }
}

fn collect_name_refs_in_expr(expr: &Expr, out: &mut Vec<(String, TextRange)>) {
    match expr {
        Expr::Name(n) => out.push((n.id.to_string(), to_core_range(n.range))),
        Expr::Call(c) => {
            collect_name_refs_in_expr(&c.func, out);
            for a in &c.arguments.args {
                collect_name_refs_in_expr(a, out);
            }
            for kw in &c.arguments.keywords {
                collect_name_refs_in_expr(&kw.value, out);
            }
        }
        Expr::Attribute(a) => collect_name_refs_in_expr(&a.value, out),
        Expr::Subscript(s) => {
            collect_name_refs_in_expr(&s.value, out);
            collect_name_refs_in_expr(&s.slice, out);
        }
        Expr::Await(a) => collect_name_refs_in_expr(&a.value, out),
        Expr::List(l) => {
            for e in &l.elts {
                collect_name_refs_in_expr(e, out);
            }
        }
        Expr::Tuple(t) => {
            for e in &t.elts {
                collect_name_refs_in_expr(e, out);
            }
        }
        Expr::Set(s) => {
            for e in &s.elts {
                collect_name_refs_in_expr(e, out);
            }
        }
        Expr::Dict(d) => {
            for item in &d.items {
                if let Some(k) = &item.key {
                    collect_name_refs_in_expr(k, out);
                }
                collect_name_refs_in_expr(&item.value, out);
            }
        }
        Expr::Compare(c) => {
            for o in &c.operands {
                collect_name_refs_in_expr(o, out);
            }
        }
        Expr::BinOp(b) => {
            collect_name_refs_in_expr(&b.left, out);
            collect_name_refs_in_expr(&b.right, out);
        }
        Expr::BoolOp(b) => {
            for v in &b.values {
                collect_name_refs_in_expr(v, out);
            }
        }
        Expr::UnaryOp(u) => collect_name_refs_in_expr(&u.operand, out),
        Expr::If(i) => {
            collect_name_refs_in_expr(&i.test, out);
            collect_name_refs_in_expr(&i.body, out);
            collect_name_refs_in_expr(&i.orelse, out);
        }
        Expr::Named(n) => {
            collect_name_refs_in_expr(&n.target, out);
            collect_name_refs_in_expr(&n.value, out);
        }
        Expr::Starred(s) => collect_name_refs_in_expr(&s.value, out),
        _ => {}
    }
}

fn advance_past_line_end(source: &str, byte_pos: u32) -> u32 {
    let bytes = source.as_bytes();
    let mut p = (byte_pos as usize).min(bytes.len());
    while p < bytes.len() && bytes[p] != b'\n' {
        p += 1;
    }
    if p < bytes.len() && bytes[p] == b'\n' {
        p += 1;
    }
    p as u32
}

fn first_code_offset_after_leading_comments(source: &str) -> u32 {
    let mut offset = 0usize;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("#!")
            || trimmed.starts_with("# -*-")
            || trimmed.starts_with("# coding")
        {
            offset += line.len();
        } else {
            break;
        }
    }
    offset as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_edits(source: &str, edits: &[Edit]) -> String {
        let mut out = source.to_string();
        let mut sorted = edits.to_vec();
        sorted.sort_by(|a, b| {
            a.range
                .start
                .cmp(&b.range.start)
                .then(a.range.end.cmp(&b.range.end))
        });
        for edit in sorted.iter().rev() {
            out.replace_range(
                edit.range.start.0 as usize..edit.range.end.0 as usize,
                &edit.replacement,
            );
        }
        out
    }

    #[test]
    fn r21_emits_first_class_classdef_with_if_and_try_nodes_with_metadata() {
        let src = concat!(
            "@final\n",
            "class Prompt(BaseModel):\n",
            "    def compat(self, flag: bool) -> dict:\n",
            "        if flag:\n",
            "            with suppress_warning():\n",
            "                try:\n",
            "                    return self.dict()\n",
            "                except Exception:\n",
            "                    return {}\n",
            "        return {}\n",
        );
        let tree = RuffAstAdapter
            .parse(FileId(1), src)
            .expect("parses cleanly");
        let cls_node = tree
            .iter_nodes()
            .find(|n| n.kind == NodeKind::ClassDef)
            .expect("ClassDef emitted");
        let Some(NodeMeta::Class(cls_meta)) = tree.node_meta(cls_node.id) else {
            panic!("expected ClassMeta on ClassDef");
        };
        assert_eq!(cls_meta.name, "Prompt");
        assert_eq!(cls_meta.bases.len(), 1);
        assert_eq!(cls_meta.bases[0].slice(src), Some("BaseModel"));
        assert_eq!(cls_meta.decorators.len(), 1);
        assert_eq!(cls_meta.decorators[0].slice(src), Some("final"));

        let with_node = tree
            .iter_nodes()
            .find(|n| n.kind == NodeKind::With)
            .expect("With node emitted");
        let Some(NodeMeta::With(with_meta)) = tree.node_meta(with_node.id) else {
            panic!("expected WithMeta on With");
        };
        assert!(!with_meta.is_async);
        assert_eq!(with_meta.items.len(), 1);
        assert_eq!(
            with_meta.items[0].context_expr_range.slice(src),
            Some("suppress_warning()")
        );

        let if_node = tree
            .iter_nodes()
            .find(|n| n.kind == NodeKind::If)
            .expect("If node emitted");
        let Some(NodeMeta::If(if_meta)) = tree.node_meta(if_node.id) else {
            panic!("expected IfMeta on If");
        };
        assert_eq!(if_meta.test_range.slice(src), Some("flag"));

        assert!(tree.iter_nodes().any(|n| n.kind == NodeKind::Try));
    }

    #[test]
    fn r19_rewrites_shadowed_function_local_import_in_place_preserving_indent_and_comments() {
        let src = concat!(
            "\"\"\"SDK configuration parser.\"\"\"\n",
            "\n",
            "from __future__ import annotations\n",
            "\n",
            "class AirflowConfigParser:\n",
            "    def mask_secrets(self, val: str, key: str) -> None:\n",
            "        from airflow.sdk._shared.secrets_masker import mask_secret  # local lazy import\n",
            "        mask_secret(val)\n",
            "        mask_secret(val, key)\n",
        );
        let tree = RuffAstAdapter.parse(FileId(1), src).expect("parses");
        let c1_start = src.find("mask_secret(val)").unwrap() as u32;
        let c1_range = TextRange::new(c1_start, c1_start + "mask_secret(val)".len() as u32);
        let c2_start = src.find("mask_secret(val, key)").unwrap() as u32;
        let c2_range = TextRange::new(c2_start, c2_start + "mask_secret(val, key)".len() as u32);

        let spec = ImportSpec {
            module: "airflow.sdk.log".to_string(),
            symbol: Some("mask_secret".to_string()),
        };
        let edits = RuffImportAdapter.resolve_scoped_import_edits(
            &tree,
            &[
                ScopedImportRequest {
                    spec: spec.clone(),
                    use_range: c1_range,
                    replaces_fqname: Some(
                        "airflow.sdk._shared.secrets_masker.mask_secret".to_string(),
                    ),
                },
                ScopedImportRequest {
                    spec,
                    use_range: c2_range,
                    replaces_fqname: Some(
                        "airflow.sdk._shared.secrets_masker.mask_secret".to_string(),
                    ),
                },
            ],
        );
        assert_eq!(
            edits.len(),
            1,
            "expected single in-place local import edit, got {edits:?}"
        );
        let patched = apply_edits(src, &edits);
        assert!(
            patched
                .contains("        from airflow.sdk.log import mask_secret  # local lazy import\n"),
            "indentation and trailing comment must be preserved, got:\n{patched}"
        );
        assert!(
            !patched.starts_with("from airflow.sdk.log")
                && !patched.contains("_shared.secrets_masker"),
            "must not insert shadowed module-top import or keep _shared import:\n{patched}"
        );
    }

    #[test]
    fn r19_rewrites_single_symbol_deprecated_module_import_in_place() {
        let src = concat!(
            "from airflow.sdk.execution_time.secrets_masker import mask_secret\n",
            "\n",
            "def run(tok: str) -> None:\n",
            "    mask_secret(tok)\n",
        );
        let tree = RuffAstAdapter.parse(FileId(1), src).expect("parses");
        let call_start = src.find("mask_secret(tok)").unwrap() as u32;
        let call_range = TextRange::new(call_start, call_start + "mask_secret(tok)".len() as u32);
        let edits = RuffImportAdapter.resolve_scoped_import_edits(
            &tree,
            &[ScopedImportRequest {
                spec: ImportSpec {
                    module: "airflow.sdk.log".to_string(),
                    symbol: Some("mask_secret".to_string()),
                },
                use_range: call_range,
                replaces_fqname: Some(
                    "airflow.sdk.execution_time.secrets_masker.mask_secret".to_string(),
                ),
            }],
        );
        let patched = apply_edits(src, &edits);
        assert_eq!(
            patched,
            "from airflow.sdk.log import mask_secret\n\ndef run(tok: str) -> None:\n    mask_secret(tok)\n"
        );
    }

    #[test]
    fn merges_new_symbol_into_existing_single_line_from_import() {
        let src = "from pkg import a  # keep comment\n\nx = a()\n";
        let edits = RuffImportAdapter.resolve_import_edits(
            src,
            &[ImportSpec {
                module: "pkg".to_string(),
                symbol: Some("b".to_string()),
            }],
        );
        let patched = apply_edits(src, &edits);
        assert_eq!(patched, "from pkg import a, b  # keep comment\n\nx = a()\n");
    }

    #[test]
    fn merges_new_symbol_into_existing_multiline_from_import() {
        let src = "from pkg import (\n    a,\n)\n\nx = a()\n";
        let edits = RuffImportAdapter.resolve_import_edits(
            src,
            &[ImportSpec {
                module: "pkg".to_string(),
                symbol: Some("b".to_string()),
            }],
        );
        let patched = apply_edits(src, &edits);
        assert_eq!(patched, "from pkg import (\n    a,\n    b,\n)\n\nx = a()\n");
    }

    #[test]
    fn does_not_emit_duplicate_when_symbol_already_imported() {
        let src = "from pkg import a, b\n\nx = a()\n";
        let edits = RuffImportAdapter.resolve_import_edits(
            src,
            &[ImportSpec {
                module: "pkg".to_string(),
                symbol: Some("b".to_string()),
            }],
        );
        assert!(edits.is_empty(), "expected no edits, got {edits:?}");
    }

    #[test]
    fn r19_merges_same_module_sibling_symbol_without_clobbering_receiver_class_import() {
        let src = "from datetime import datetime\n\ndef echo():\n    return datetime.utcnow()\n";
        let tree = RuffAstAdapter.parse(FileId(1), src).expect("parses");
        let call_start = src.find("datetime.utcnow()").unwrap() as u32;
        let call_range = TextRange::new(call_start, call_start + "datetime.utcnow()".len() as u32);
        let edits = RuffImportAdapter.resolve_scoped_import_edits(
            &tree,
            &[ScopedImportRequest {
                spec: ImportSpec {
                    module: "datetime".to_string(),
                    symbol: Some("timezone".to_string()),
                },
                use_range: call_range,
                replaces_fqname: Some("datetime.datetime.utcnow".to_string()),
            }],
        );
        let patched = apply_edits(src, &edits);
        assert_eq!(
            patched,
            "from datetime import datetime, timezone\n\ndef echo():\n    return datetime.utcnow()\n"
        );
    }
}

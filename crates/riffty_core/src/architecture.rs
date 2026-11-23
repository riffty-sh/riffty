use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{Applicability, Diagnostic, FileId, Fix, Language, Severity, TextRange};

fn default_arch_rule_id() -> String {
    "ARCH-FORBID-001".to_string()
}

/// Declarative dependency boundary rule (`[[architecture.forbid]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchForbidRule {
    #[serde(default = "default_arch_rule_id")]
    pub id: String,
    /// Glob matching source module/package/path (e.g. `"com.acme.domain.**"`, `"src/domain/**"`).
    pub from: String,
    /// Globs matching forbidden dependency targets.
    pub target: Vec<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Declarative architectural layer (`[[architecture.layers]]`).
///
/// Intra-layer (same-layer) dependencies are always allowed, and layers matching 0 modules
/// produce zero diagnostics, keeping simple/flat applications noise-free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchLayer {
    pub name: String,
    /// Globs identifying modules/packages/paths belonging to this layer.
    pub matches: Vec<String>,
    /// Names of other layers this layer is permitted to depend on.
    #[serde(default)]
    pub may_access: Vec<String>,
}

/// Language-agnostic architecture configuration (`[architecture]` in `riffty.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArchitectureConfig {
    pub forbid: Vec<ArchForbidRule>,
    pub layers: Vec<ArchLayer>,
    pub forbid_cycles: bool,
    /// Optional module/package depth for slice grouping during cycle detection.
    pub cycle_slice_depth: Option<usize>,
}

impl ArchitectureConfig {
    pub fn is_empty(&self) -> bool {
        self.forbid.is_empty() && self.layers.is_empty() && !self.forbid_cycles
    }
}

/// Uniform cross-language dependency edge emitted by Java, Python, and JS/TS extractors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyEdge {
    pub file_id: FileId,
    pub language: Language,
    /// Source module/package/path (e.g. `"com.acme.domain"`, `"app.domain.order"`, `"src/domain/order"`).
    pub from_module: String,
    /// Target symbol/module/package/path (e.g. `"com.acme.infra.DbClient"`, `"app.infra.db"`, `"src/infra/db"`).
    pub to_target: String,
    /// Edge kind (`"import"`, `"type-ref"`, `"new"`, `"extends"`, `"implements"`, `"annotation"`, `"require"`, `"export-from"`).
    pub edge_kind: String,
    pub range: TextRange,
}

/// Evaluates declarative `forbid`, `layers`, and `forbid_cycles` rules over uniform `DependencyEdge`s.
///
/// - Completely opt-in: returns empty when `config.is_empty()`.
/// - Same-layer dependencies are always allowed (zero noise on flat/merged apps).
/// - Empty layers produce zero diagnostics.
pub fn evaluate_architecture(
    config: &ArchitectureConfig,
    edges: &[DependencyEdge],
) -> Vec<Diagnostic> {
    if config.is_empty() || edges.is_empty() {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();
    let mut seen_keys: BTreeSet<(u32, u32, u32, String)> = BTreeSet::new();

    // 1. Evaluate `forbid` rules
    for edge in edges {
        for rule in &config.forbid {
            if !glob_matches(&rule.from, &edge.from_module) {
                continue;
            }
            if let Some(matched_pat) = rule
                .target
                .iter()
                .find(|pat| glob_matches(pat, &edge.to_target))
            {
                let key = (
                    edge.file_id.0,
                    edge.range.start.0,
                    edge.range.end.0,
                    rule.id.clone(),
                );
                if !seen_keys.insert(key) {
                    continue;
                }
                let base_msg = rule.message.clone().unwrap_or_else(|| {
                    format!(
                        "Forbidden architectural dependency from `{}` to `{}`",
                        edge.from_module, edge.to_target
                    )
                });
                let msg = format!(
                    "{base_msg} ({} `{}` in `{}`)",
                    edge.edge_kind, edge.to_target, edge.from_module
                );
                diagnostics.push(Diagnostic {
                    rule_id: rule.id.clone(),
                    rule_name: "architecture-forbid".to_string(),
                    severity: Severity::Error,
                    file_id: edge.file_id,
                    range: edge.range,
                    message: msg,
                    type_proof: Some(serde_json::json!({
                        "engine": "riffty-architecture",
                        "language": edge.language.as_str(),
                        "from_module": edge.from_module,
                        "to_target": edge.to_target,
                        "edge_kind": edge.edge_kind,
                        "rule_pattern": matched_pat,
                    })),
                    fix: Some(Fix {
                        title: format!(
                            "Remove forbidden {} dependency on `{}`",
                            edge.edge_kind, edge.to_target
                        ),
                        edits: Vec::new(),
                        required_imports: Vec::new(),
                        applicability: Applicability::ManualOnly,
                    }),
                });
            }
        }
    }

    // 2. Evaluate `layers` rules
    if !config.layers.is_empty() {
        for edge in edges {
            let from_layers: Vec<&ArchLayer> = config
                .layers
                .iter()
                .filter(|layer| {
                    layer
                        .matches
                        .iter()
                        .any(|pat| glob_matches(pat, &edge.from_module))
                })
                .collect();
            if from_layers.is_empty() {
                continue;
            }
            let to_layers: Vec<&ArchLayer> = config
                .layers
                .iter()
                .filter(|layer| {
                    layer
                        .matches
                        .iter()
                        .any(|pat| glob_matches(pat, &edge.to_target))
                })
                .collect();
            if to_layers.is_empty() {
                continue;
            }

            // Intra-layer / same-layer edge is always allowed.
            let shares_layer = from_layers
                .iter()
                .any(|fl| to_layers.iter().any(|tl| tl.name == fl.name));
            if shares_layer {
                continue;
            }

            for fl in &from_layers {
                for tl in &to_layers {
                    if fl.may_access.iter().any(|allowed| allowed == &tl.name) {
                        continue;
                    }
                    let rule_id = format!("ARCH-LAYER-{}", fl.name.to_ascii_uppercase());
                    let key = (
                        edge.file_id.0,
                        edge.range.start.0,
                        edge.range.end.0,
                        rule_id.clone(),
                    );
                    if !seen_keys.insert(key) {
                        continue;
                    }
                    diagnostics.push(Diagnostic {
                        rule_id,
                        rule_name: "architecture-layer-violation".to_string(),
                        severity: Severity::Error,
                        file_id: edge.file_id,
                        range: edge.range,
                        message: format!(
                            "Layer `{}` may not depend on layer `{}` ({} `{}` in `{}`)",
                            fl.name, tl.name, edge.edge_kind, edge.to_target, edge.from_module
                        ),
                        type_proof: Some(serde_json::json!({
                            "engine": "riffty-architecture",
                            "language": edge.language.as_str(),
                            "from_layer": fl.name,
                            "to_layer": tl.name,
                            "from_module": edge.from_module,
                            "to_target": edge.to_target,
                            "edge_kind": edge.edge_kind,
                        })),
                        fix: Some(Fix {
                            title: format!(
                                "Invert or decouple dependency from layer `{}` to `{}`",
                                fl.name, tl.name
                            ),
                            edits: Vec::new(),
                            required_imports: Vec::new(),
                            applicability: Applicability::ManualOnly,
                        }),
                    });
                }
            }
        }
    }

    // 3. Evaluate `forbid_cycles` via Tarjan's SCC over internal modules/slices
    if config.forbid_cycles {
        let mut known_modules: BTreeSet<String> = BTreeSet::new();
        for edge in edges {
            let slice = slice_module(&edge.from_module, config.cycle_slice_depth);
            if !slice.is_empty() {
                known_modules.insert(slice);
            }
        }

        // Build adjacency list & retain the first representative edge for each directed pair
        let mut adj: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut rep_edge: BTreeMap<(String, String), &DependencyEdge> = BTreeMap::new();

        for edge in edges {
            let from_slice = slice_module(&edge.from_module, config.cycle_slice_depth);
            if from_slice.is_empty() {
                continue;
            }
            let Some(to_slice) =
                resolve_internal_slice(&edge.to_target, &known_modules, config.cycle_slice_depth)
            else {
                continue;
            };
            if from_slice == to_slice {
                continue;
            }
            adj.entry(from_slice.clone())
                .or_default()
                .insert(to_slice.clone());
            rep_edge.entry((from_slice, to_slice)).or_insert(edge);
        }

        let sccs = tarjan_scc(&adj);
        for scc in sccs {
            if scc.len() <= 1 {
                continue;
            }
            let scc_set: BTreeSet<&str> = scc.iter().map(String::as_str).collect();
            let cycle_path = reconstruct_cycle_chain(&scc, &adj);
            let cycle_display = cycle_path.join(" -> ");

            // Emit diagnostic on the first edge inside the cycle (deterministic order)
            if let Some(first_from) = cycle_path.first()
                && let Some(first_to) = cycle_path.get(1)
                && let Some(edge) = rep_edge.get(&(first_from.clone(), first_to.clone()))
            {
                let rule_id = "ARCH-CYCLE-001".to_string();
                let key = (
                    edge.file_id.0,
                    edge.range.start.0,
                    edge.range.end.0,
                    rule_id.clone(),
                );
                if seen_keys.insert(key) {
                    diagnostics.push(Diagnostic {
                        rule_id,
                        rule_name: "architecture-cycle".to_string(),
                        severity: Severity::Error,
                        file_id: edge.file_id,
                        range: edge.range,
                        message: format!("Cyclic module dependency detected: {cycle_display}"),
                        type_proof: Some(serde_json::json!({
                            "engine": "riffty-architecture",
                            "language": edge.language.as_str(),
                            "cycle": cycle_path,
                            "scc_size": scc_set.len(),
                        })),
                        fix: Some(Fix {
                            title: "Break cyclic dependency by extracting a shared interface or inverting control".to_string(),
                            edits: Vec::new(),
                            required_imports: Vec::new(),
                            applicability: Applicability::ManualOnly,
                        }),
                    });
                }
            }
        }
    }

    diagnostics.sort_by(|a, b| {
        a.file_id
            .cmp(&b.file_id)
            .then_with(|| a.range.start.cmp(&b.range.start))
            .then_with(|| a.range.end.cmp(&b.range.end))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });
    diagnostics
}

fn slice_module(module: &str, depth: Option<usize>) -> String {
    let trimmed = module.trim().trim_matches(['/', '.']);
    let Some(max_depth) = depth else {
        return trimmed.to_string();
    };
    if max_depth == 0 || trimmed.is_empty() {
        return trimmed.to_string();
    }
    let sep = if trimmed.contains('/') { '/' } else { '.' };
    trimmed
        .split(sep)
        .take(max_depth)
        .collect::<Vec<_>>()
        .join(&sep.to_string())
}

fn resolve_internal_slice(
    to_target: &str,
    known_modules: &BTreeSet<String>,
    depth: Option<usize>,
) -> Option<String> {
    let direct = slice_module(to_target, depth);
    if known_modules.contains(&direct) {
        return Some(direct);
    }
    // Strip trailing symbol (e.g., `com.acme.infra.DbClient` -> `com.acme.infra` or `src/infra/db` -> `src/infra`)
    let trimmed = to_target.trim().trim_matches(['/', '.']);
    let sep = if trimmed.contains('/') { '/' } else { '.' };
    let mut cur = trimmed;
    while let Some((prefix, _)) = cur.rsplit_once(sep) {
        let candidate = slice_module(prefix, depth);
        if known_modules.contains(&candidate) {
            return Some(candidate);
        }
        cur = prefix;
    }
    None
}

fn tarjan_scc(adj: &BTreeMap<String, BTreeSet<String>>) -> Vec<Vec<String>> {
    struct State<'a> {
        adj: &'a BTreeMap<String, BTreeSet<String>>,
        index: usize,
        indices: BTreeMap<String, usize>,
        lowlink: BTreeMap<String, usize>,
        on_stack: BTreeSet<String>,
        stack: Vec<String>,
        sccs: Vec<Vec<String>>,
    }

    fn strongconnect(v: &str, st: &mut State<'_>) {
        let v_idx = st.index;
        st.index += 1;
        st.indices.insert(v.to_string(), v_idx);
        st.lowlink.insert(v.to_string(), v_idx);
        st.stack.push(v.to_string());
        st.on_stack.insert(v.to_string());

        if let Some(neighbors) = st.adj.get(v) {
            for w in neighbors {
                if !st.indices.contains_key(w.as_str()) {
                    strongconnect(w, st);
                    let w_low = st.lowlink[w.as_str()];
                    let v_low = st.lowlink[v];
                    if w_low < v_low {
                        st.lowlink.insert(v.to_string(), w_low);
                    }
                } else if st.on_stack.contains(w.as_str()) {
                    let w_idx = st.indices[w.as_str()];
                    let v_low = st.lowlink[v];
                    if w_idx < v_low {
                        st.lowlink.insert(v.to_string(), w_idx);
                    }
                }
            }
        }

        if st.lowlink[v] == st.indices[v] {
            let mut scc = Vec::new();
            while let Some(w) = st.stack.pop() {
                st.on_stack.remove(&w);
                let done = w == v;
                scc.push(w);
                if done {
                    break;
                }
            }
            scc.sort();
            st.sccs.push(scc);
        }
    }

    let mut nodes: BTreeSet<String> = BTreeSet::new();
    for (u, vs) in adj {
        nodes.insert(u.clone());
        for v in vs {
            nodes.insert(v.clone());
        }
    }

    let mut st = State {
        adj,
        index: 0,
        indices: BTreeMap::new(),
        lowlink: BTreeMap::new(),
        on_stack: BTreeSet::new(),
        stack: Vec::new(),
        sccs: Vec::new(),
    };

    for node in &nodes {
        if !st.indices.contains_key(node.as_str()) {
            strongconnect(node, &mut st);
        }
    }

    st.sccs.sort();
    st.sccs
}

fn reconstruct_cycle_chain(
    scc: &[String],
    adj: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<String> {
    let scc_set: BTreeSet<&str> = scc.iter().map(String::as_str).collect();
    let Some(start) = scc.first() else {
        return Vec::new();
    };
    // BFS/DFS to find shortest cycle starting and ending at `start` within `scc`
    let mut stack: Vec<(String, Vec<String>)> = vec![(start.clone(), vec![start.clone()])];
    let mut visited: BTreeSet<String> = BTreeSet::new();

    while let Some((curr, path)) = stack.pop() {
        if let Some(neighbors) = adj.get(&curr) {
            for next in neighbors {
                if !scc_set.contains(next.as_str()) {
                    continue;
                }
                if next == start && path.len() >= 2 {
                    let mut out = path;
                    out.push(start.clone());
                    return out;
                }
                if visited.insert(next.clone()) {
                    let mut next_path = path.clone();
                    next_path.push(next.clone());
                    stack.push((next.clone(), next_path));
                }
            }
        }
    }

    let mut fallback = scc.to_vec();
    fallback.push(start.clone());
    fallback
}

/// Glob matcher supporting `*` (single segment / non-separator), `**` (any depth),
/// and both dotted package names (`com.acme.domain.**`) and slash paths (`src/domain/**`).
pub fn glob_matches(pattern: &str, value: &str) -> bool {
    let pat = pattern.trim();
    let val = value.trim();
    if pat == val || pat == "**" || pat == "*" {
        return true;
    }
    if let Some(prefix) = pat.strip_suffix(".**") {
        return val == prefix || val.starts_with(&format!("{prefix}."));
    }
    if let Some(prefix) = pat.strip_suffix("/**") {
        return val == prefix || val.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = pat.strip_suffix(".*") {
        if let Some(rest) = val.strip_prefix(&format!("{prefix}.")) {
            return !rest.contains('.');
        }
        return false;
    }
    if let Some(prefix) = pat.strip_suffix("/*") {
        if let Some(rest) = val.strip_prefix(&format!("{prefix}/")) {
            return !rest.contains('/');
        }
        return false;
    }
    wildcard_match(pat.as_bytes(), val.as_bytes())
}

fn wildcard_match(pat: &[u8], val: &[u8]) -> bool {
    let (mut p, mut v) = (0usize, 0usize);
    let mut star_p: Option<usize> = None;
    let mut star_v: usize = 0;

    while v < val.len() {
        if p + 1 < pat.len() && pat[p] == b'*' && pat[p + 1] == b'*' {
            star_p = Some(p);
            star_v = v;
            p += 2;
        } else if p < pat.len() && pat[p] == b'*' {
            star_p = Some(p);
            star_v = v;
            p += 1;
        } else if p < pat.len() && (pat[p] == b'?' || pat[p] == val[v]) {
            p += 1;
            v += 1;
        } else if let Some(sp) = star_p {
            let is_double = sp + 1 < pat.len() && pat[sp + 1] == b'*';
            if !is_double && (val[star_v] == b'/' || val[star_v] == b'.') {
                return false;
            }
            star_v += 1;
            v = star_v;
            p = if is_double { sp + 2 } else { sp + 1 };
        } else {
            return false;
        }
    }

    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(file_id: u32, from: &str, to: &str, lang: Language) -> DependencyEdge {
        DependencyEdge {
            file_id: FileId(file_id),
            language: lang,
            from_module: from.to_string(),
            to_target: to.to_string(),
            edge_kind: "import".to_string(),
            range: TextRange::new(0, 10),
        }
    }

    #[test]
    fn empty_config_produces_zero_noise_on_simple_apps() {
        let cfg = ArchitectureConfig::default();
        let edges = vec![edge(1, "com.acme.app", "com.acme.app.Repo", Language::Java)];
        assert!(evaluate_architecture(&cfg, &edges).is_empty());
    }

    #[test]
    fn flat_same_layer_app_and_empty_layers_produce_zero_noise() {
        let cfg = ArchitectureConfig {
            forbid: Vec::new(),
            layers: vec![
                ArchLayer {
                    name: "app".to_string(),
                    matches: vec!["com.acme.app.**".to_string()],
                    may_access: vec![],
                },
                ArchLayer {
                    name: "adapters".to_string(),
                    matches: vec!["com.acme.adapters.**".to_string()],
                    may_access: vec!["app".to_string()],
                },
            ],
            forbid_cycles: true,
            cycle_slice_depth: None,
        };
        let edges = vec![
            edge(1, "com.acme.app", "com.acme.app.Service", Language::Java),
            edge(2, "com.acme.app", "com.acme.app.Repo", Language::Java),
        ];
        assert!(evaluate_architecture(&cfg, &edges).is_empty());
    }

    #[test]
    fn enforces_forbid_and_layers_across_java_python_and_jsts() {
        let cfg = ArchitectureConfig {
            forbid: vec![ArchForbidRule {
                id: "NO-JUL".to_string(),
                from: "com.acme.**".to_string(),
                target: vec!["java.util.logging.**".to_string()],
                message: Some("Use SLF4J instead of java.util.logging".to_string()),
            }],
            layers: vec![
                ArchLayer {
                    name: "domain".to_string(),
                    matches: vec![
                        "com.acme.domain.**".to_string(),
                        "app.domain.**".to_string(),
                        "src/domain/**".to_string(),
                    ],
                    may_access: vec![],
                },
                ArchLayer {
                    name: "infra".to_string(),
                    matches: vec![
                        "com.acme.infra.**".to_string(),
                        "app.infra.**".to_string(),
                        "src/infra/**".to_string(),
                    ],
                    may_access: vec!["domain".to_string()],
                },
            ],
            forbid_cycles: false,
            cycle_slice_depth: None,
        };

        let edges = vec![
            edge(
                1,
                "com.acme.domain",
                "java.util.logging.Logger",
                Language::Java,
            ),
            edge(
                2,
                "com.acme.domain",
                "com.acme.infra.DbClient",
                Language::Java,
            ),
            edge(3, "app.domain.order", "app.infra.sql", Language::Python),
            edge(4, "src/domain/order", "src/infra/prisma", Language::JsTs),
            // Allowed direction: infra -> domain
            edge(5, "src/infra/prisma", "src/domain/order", Language::JsTs),
        ];

        let diags = evaluate_architecture(&cfg, &edges);
        assert_eq!(diags.len(), 4, "got: {diags:#?}");
        assert_eq!(diags[0].rule_id, "NO-JUL");
        assert_eq!(diags[1].rule_id, "ARCH-LAYER-DOMAIN");
        assert_eq!(diags[2].rule_id, "ARCH-LAYER-DOMAIN");
        assert_eq!(diags[3].rule_id, "ARCH-LAYER-DOMAIN");
    }

    #[test]
    fn detects_package_cycles_with_tarjan_scc() {
        let cfg = ArchitectureConfig {
            forbid: Vec::new(),
            layers: Vec::new(),
            forbid_cycles: true,
            cycle_slice_depth: None,
        };
        let edges = vec![
            edge(
                1,
                "com.acme.orders",
                "com.acme.billing.Invoice",
                Language::Java,
            ),
            edge(
                2,
                "com.acme.billing",
                "com.acme.users.Account",
                Language::Java,
            ),
            edge(3, "com.acme.users", "com.acme.orders.Order", Language::Java),
        ];
        let diags = evaluate_architecture(&cfg, &edges);
        assert_eq!(diags.len(), 1, "got: {diags:#?}");
        assert_eq!(diags[0].rule_id, "ARCH-CYCLE-001");
        assert!(diags[0].message.contains("com.acme.billing"));
        assert!(diags[0].message.contains("com.acme.orders"));
        assert!(diags[0].message.contains("com.acme.users"));
    }
}

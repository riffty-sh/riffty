use std::path::{Path, PathBuf};
use std::sync::Arc;

use riffty_adapter_ast_grep::AstGrepAdapter;
use riffty_adapter_fs::FsWorkspaceAdapter;
use riffty_adapter_java_fast::JavaFastAnalyzer;
use riffty_adapter_jsts_fast::JsTsFastAnalyzer;
use riffty_adapter_plugin::{JavaPluginAdapter, discover_java_sidecar_bundle};
use riffty_adapter_refaster::RefasterYamlRule;
use riffty_adapter_ruff_ast::{RuffAstAdapter, RuffImportAdapter};
use riffty_adapter_ty::TyOracleAdapter;
use riffty_core::{AnalysisTier, Language, RulePlugin};
use riffty_pipeline::{
    AnalyzerRegistry, MultiLanguageReviewFacade, PythonLanguageAnalyzer, ReviewChangesRequest,
    ReviewVerdict, RifftyEngine,
};
use riffty_rules_builtin::builtin_rules;
use rmcp::model::{
    CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
    Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, serve_server};
use serde::Deserialize;

pub const REVIEW_TOOL_NAME: &str = "riffty_review_changes";

#[derive(Debug, Clone, Deserialize)]
struct McpReviewToolInput {
    target: String,
    #[serde(default = "default_base_ref")]
    base_ref: String,
    #[serde(default = "default_auto_heal")]
    auto_heal: bool,
    #[serde(default)]
    workspace_root: Option<String>,
}

fn default_base_ref() -> String {
    "main".to_string()
}

const fn default_auto_heal() -> bool {
    true
}

#[derive(Clone)]
pub struct RifftyMcpServer {
    pub default_workspace_root: PathBuf,
    pub rules_path: Option<PathBuf>,
    pub python_venv: Option<PathBuf>,
}

impl RifftyMcpServer {
    pub fn new(default_workspace_root: PathBuf, rules_path: Option<PathBuf>) -> Self {
        Self {
            default_workspace_root,
            rules_path,
            python_venv: None,
        }
    }

    pub fn with_venv(
        default_workspace_root: PathBuf,
        rules_path: Option<PathBuf>,
        python_venv: Option<PathBuf>,
    ) -> Self {
        Self {
            default_workspace_root,
            rules_path,
            python_venv,
        }
    }

    pub fn review_tool_definition() -> Tool {
        let schema_val = serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "enum": ["unstaged", "staged", "branch_diff"],
                    "description": "Git diff target to review (`unstaged`, `staged`, or `branch_diff`)."
                },
                "base_ref": {
                    "type": "string",
                    "default": "main",
                    "description": "Base git ref when `target` is `branch_diff`."
                },
                "auto_heal": {
                    "type": "boolean",
                    "default": true,
                    "description": "When true, automatically apply `Applicability::Safe` byte-offset fixes on disk."
                },
                "workspace_root": {
                    "type": "string",
                    "description": "Optional path to git repository root (defaults to server working directory)."
                }
            },
            "required": ["target"]
        });

        let schema_obj = match schema_val {
            serde_json::Value::Object(map) => Arc::new(map),
            _ => Arc::new(serde_json::Map::new()),
        };

        Tool::new(
            REVIEW_TOOL_NAME,
            "Deterministic type-aware Python & Java diff reviewer and surgical byte-offset auto-healer. Extracts touched git diff spans, expands blast radius via reverse_dependents, suppresses baseline noise, optionally heals Safe fixes on disk, and returns a compact APPROVED/REQUEST_CHANGES JSON verdict with provenance.",
            schema_obj,
        )
        .with_annotations(
            ToolAnnotations::with_title("Review & Auto-Heal Python & Java Diff")
                .read_only(false)
                .destructive(false)
                .idempotent(true)
                .open_world(false),
        )
    }
}

/// Canonicalizes `requested` (if provided) and enforces that it remains contained within
/// `default_root` to prevent MCP clients from scanning or patching arbitrary host directories.
pub fn resolve_contained_workspace_root(
    default_root: &Path,
    requested: Option<&str>,
) -> Result<PathBuf, String> {
    let canonical_default = default_root.canonicalize().map_err(|e| {
        format!(
            "failed to canonicalize default workspace root `{}`: {e}",
            default_root.display()
        )
    })?;

    let Some(req_str) = requested else {
        return Ok(canonical_default);
    };

    let candidate = PathBuf::from(req_str);
    let resolved = if candidate.is_absolute() {
        candidate
    } else {
        canonical_default.join(candidate)
    };
    let canonical_requested = resolved.canonicalize().map_err(|e| {
        format!(
            "invalid `workspace_root` `{req_str}`: failed to canonicalize ({e})"
        )
    })?;

    if !canonical_requested.starts_with(&canonical_default) {
        return Err(format!(
            "security boundary violation: requested `workspace_root` `{}` escapes configured root `{}`",
            canonical_requested.display(),
            canonical_default.display()
        ));
    }

    Ok(canonical_requested)
}

/// Executes `ReviewFacade::review_changes` synchronously with the production hexagonal adapters.
pub fn execute_review_request(
    workspace_root: &Path,
    rules_path: Option<&Path>,
    request: &ReviewChangesRequest,
) -> Result<ReviewVerdict, String> {
    execute_review_request_with_venv(workspace_root, rules_path, None, request)
}

/// Executes multi-language (`Python` + `Java`) diff review with Safe-only auto-heal
/// and zero build-tool execution.
pub fn execute_review_request_with_venv(
    workspace_root: &Path,
    rules_path: Option<&Path>,
    python_venv: Option<PathBuf>,
    request: &ReviewChangesRequest,
) -> Result<ReviewVerdict, String> {
    let parser = RuffAstAdapter;
    let oracle = TyOracleAdapter::with_venv(python_venv);
    let import_mgr = RuffImportAdapter;
    let matcher = AstGrepAdapter;
    let fs_adapter = FsWorkspaceAdapter;

    let mut rules: Vec<Box<dyn RulePlugin>> = builtin_rules();
    let mut jsts_rules: Vec<RefasterYamlRule> = Vec::new();
    let mut rules_paths: Vec<PathBuf> = Vec::new();
    if let Some(yaml_path) = rules_path {
        let extra = RefasterYamlRule::from_yaml_file_for_language(yaml_path, Language::Python)?;
        rules.extend(extra);
        jsts_rules.extend(RefasterYamlRule::jsts_rules_from_file(yaml_path)?);
        rules_paths.push(yaml_path.to_path_buf());
    }

    let mut registry = AnalyzerRegistry::new();
    registry.register(PythonLanguageAnalyzer {
        engine: RifftyEngine {
            parser: &parser,
            oracle: &oracle,
            import_manager: &import_mgr,
            pattern_matcher: Some(&matcher),
            rules,
        },
        tier: AnalysisTier::Tier2Semantic,
        runtime_detail: Some("ruff-ast + ty".to_string()),
    });

    if let Some(bundle) = discover_java_sidecar_bundle() {
        registry.register(JavaPluginAdapter {
            bundle,
            extra_classpath: Vec::new(),
            use_daemon: false,
            daemon_socket: None,
            architecture_forbid: Vec::new(),
            allow_build_tool: false,
        });
    } else {
        registry.register(JavaFastAnalyzer);
    }

    registry.register(JsTsFastAnalyzer {
        refaster_rules: jsts_rules,
    });

    let facade = MultiLanguageReviewFacade {
        registry: &registry,
        vfs: &fs_adapter,
        patch_applier: &fs_adapter,
        git_diff: &fs_adapter,
    };

    facade.review_changes(workspace_root, &rules_paths, request)
}

impl ServerHandler for RifftyMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("riffty", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Call `riffty_review_changes` after editing Python files to auto-heal deterministic bugs and obtain a compact review verdict.",
            )
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        if name == REVIEW_TOOL_NAME {
            Some(Self::review_tool_definition())
        } else {
            None
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![
            Self::review_tool_definition(),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if request.name != REVIEW_TOOL_NAME {
            return Err(ErrorData::method_not_found::<CallToolRequestMethod>());
        }

        let args_val = serde_json::Value::Object(request.arguments.unwrap_or_default());
        let parsed_input: McpReviewToolInput = serde_json::from_value(args_val).map_err(|e| {
            ErrorData::invalid_params(
                format!("invalid `riffty_review_changes` parameters: {e}"),
                None,
            )
        })?;

        let review_req = ReviewChangesRequest {
            target: parsed_input.target,
            base_ref: parsed_input.base_ref,
            auto_heal: parsed_input.auto_heal,
        };
        // Validate `target` upfront so invalid enum values return JSON-RPC `-32602` (`invalid_params`).
        if let Err(msg) = review_req.to_diff_target() {
            return Err(ErrorData::invalid_params(msg, None));
        }

        let root = resolve_contained_workspace_root(
            &self.default_workspace_root,
            parsed_input.workspace_root.as_deref(),
        )
        .map_err(|msg| ErrorData::invalid_params(msg, None))?;
        let rules_path = self.rules_path.clone();
        let python_venv = self.python_venv.clone();

        let verdict_res = tokio::task::spawn_blocking(move || {
            execute_review_request_with_venv(&root, rules_path.as_deref(), python_venv, &review_req)
        })
        .await
        .map_err(|join_err| {
            ErrorData::internal_error(format!("review worker panicked: {join_err}"), None)
        })?;

        match verdict_res {
            Ok(verdict) => {
                let compact_json = serde_json::to_string(&verdict).map_err(|e| {
                    ErrorData::internal_error(format!("failed to serialize verdict: {e}"), None)
                })?;
                let structured = serde_json::to_value(&verdict).ok();
                let mut res = CallToolResult::success(vec![ContentBlock::text(compact_json)]);
                res.structured_content = structured;
                Ok(CallToolResponse::Complete(res))
            }
            Err(err_msg) => Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text(err_msg),
            ]))),
        }
    }
}

pub async fn run_stdio_server(
    workspace_root: PathBuf,
    rules_path: Option<PathBuf>,
) -> Result<(), String> {
    run_stdio_server_with_venv(workspace_root, rules_path, None).await
}

pub async fn run_stdio_server_with_venv(
    workspace_root: PathBuf,
    rules_path: Option<PathBuf>,
    python_venv: Option<PathBuf>,
) -> Result<(), String> {
    let server = RifftyMcpServer::with_venv(workspace_root, rules_path, python_venv);
    let running = serve_server(server, rmcp::transport::stdio())
        .await
        .map_err(|e| format!("MCP initialization error: {e}"))?;
    let _ = running
        .waiting()
        .await
        .map_err(|e| format!("MCP transport error: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("riffty_mcp_test_{name}_{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn mcp_workspace_root_containment_allows_subpath_and_blocks_escape() {
        let root = make_temp_dir("contained_root");
        let sub = root.join("pkg");
        let outside = make_temp_dir("outside_root");
        std::fs::create_dir_all(&sub).unwrap();

        let default_ok = resolve_contained_workspace_root(&root, None).unwrap();
        assert_eq!(default_ok, root.canonicalize().unwrap());

        let sub_ok =
            resolve_contained_workspace_root(&root, Some(sub.to_str().unwrap())).unwrap();
        assert_eq!(sub_ok, sub.canonicalize().unwrap());

        let abs_escape =
            resolve_contained_workspace_root(&root, Some(outside.to_str().unwrap())).unwrap_err();
        assert!(abs_escape.contains("security boundary violation"));

        let rel_escape = resolve_contained_workspace_root(&root, Some("..")).unwrap_err();
        assert!(rel_escape.contains("security boundary violation"));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn req_m0_06_mcp_jsts_blocks_traversal_and_option_injection_and_heals_staged_diff() {
        let root = make_temp_dir("jsts_mcp_gate");
        let outside = make_temp_dir("jsts_mcp_outside");

        // 1. Path traversal blocked on JS/TS workspace
        let escape_err = resolve_contained_workspace_root(&root, Some("../"))
            .unwrap_err();
        assert!(escape_err.contains("security boundary violation"));
        let abs_escape =
            resolve_contained_workspace_root(&root, Some(outside.to_str().unwrap())).unwrap_err();
        assert!(abs_escape.contains("security boundary violation"));

        // 2. Initialize git repo with a TS file and malicious package.json (never executed!)
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .expect("git command");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_git(&["init", "-q"]);
        run_git(&["config", "user.email", "test@riffty.sh"]);
        run_git(&["config", "user.name", "Riffty Test"]);

        std::fs::write(
            root.join("package.json"),
            r#"{"name":"evil","scripts":{"postinstall":"exit 99"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/check.ts"),
            "export function isBad(x: number): boolean {\n  return false;\n}\n",
        )
        .unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-q", "-m", "initial"]);

        // 3. Option injection via `base_ref` must be rejected
        let inj_req = ReviewChangesRequest {
            target: "branch_diff".to_string(),
            base_ref: "--output=/tmp/pwned".to_string(),
            auto_heal: true,
        };
        let inj_err = execute_review_request(&root, None, &inj_req).unwrap_err();
        assert!(inj_err.contains("invalid git ref"));

        // 4. Stage a buggy JS/TS edit and verify MCP heals it via JS/TS Refaster YAML
        let rules_file = root.join("jsts_rules.yml");
        std::fs::write(
            &rules_file,
            r#"rules:
  - id: JSTS-REF-NAN
    name: nan-equality
    language: jsts
    severity: error
    applicability: safe
    message: "Use Number.isNaN($X)"
    pattern: "$X === NaN"
    replace: "Number.isNaN($X)"
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("src/check.ts"),
            "export function isBad(x: number): boolean {\n  return x === NaN;\n}\n",
        )
        .unwrap();
        run_git(&["add", "src/check.ts"]);

        let staged_req = ReviewChangesRequest {
            target: "staged".to_string(),
            base_ref: "main".to_string(),
            auto_heal: true,
        };
        let verdict =
            execute_review_request(&root, Some(&rules_file), &staged_req).expect("mcp review ok");
        assert_eq!(verdict.verdict, "APPROVED");
        assert_eq!(verdict.summary.auto_healed_count, 1);
        let healed = std::fs::read_to_string(root.join("src/check.ts")).unwrap();
        assert!(healed.contains("return Number.isNaN(x);"), "got:\n{healed}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}

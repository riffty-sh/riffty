use std::path::{Path, PathBuf};
use std::process::Command;

use riffty_adapter_fs::WorkspaceResolver;
use riffty_adapter_java_fast::{RuleDoc, java_fast_rule_catalog};
use riffty_adapter_jsts_fast::jsts_fast_rule_catalog;
use riffty_adapter_plugin::{discover_java_sidecar_bundle, verify_jar_sha256};
use riffty_adapter_tsrs::jsts_tier2_rule_catalog;
use riffty_core::{Language, Severity};
use serde::Serialize;

use crate::config::RifftyTomlConfig;

fn python_builtin_rule_catalog() -> &'static [RuleDoc] {
    &[
        RuleDoc {
            id: "RFT-004",
            name: "unawaited-coroutine",
            language: Language::Python,
            severity: Severity::Error,
            default_applicability: "ManualOnly",
            summary: "Coroutine call inside an async function is neither awaited, returned, nor assigned.",
            rationale: "Calling an `async def` function without `await` creates a coroutine object that is immediately discarded, silently skipping the async work and emitting `RuntimeWarning`.",
            bad_example: "async def handler():\n    send_audit_event(user_id)",
            good_example: "async def handler():\n    await send_audit_event(user_id)",
        },
        RuleDoc {
            id: "RFT-015",
            name: "disjoint-membership-check",
            language: Language::Python,
            severity: Severity::Error,
            default_applicability: "ManualOnly",
            summary: "Membership test `x in container` compares provably disjoint types (always False).",
            rationale: "Type-proven disjoint membership checks (e.g. `int in set[str]`) are dead logic bugs that silently evaluate to `False` at runtime.",
            bad_example: "if user_id in allowed_usernames:  # int in set[str]",
            good_example: "if str(user_id) in allowed_usernames:",
        },
        RuleDoc {
            id: "RFT-118",
            name: "warnings-warn-missing-stacklevel",
            language: Language::Python,
            severity: Severity::Warning,
            default_applicability: "Safe",
            summary: "`warnings.warn(...)` omits explicit `stacklevel=2`.",
            rationale: "Without `stacklevel=2`, Python attributes the warning to the library/helper line instead of the caller's code, making deprecation and runtime warnings hard to trace.",
            bad_example: "warnings.warn(\"deprecated API\")",
            good_example: "warnings.warn(\"deprecated API\", stacklevel=2)",
        },
        RuleDoc {
            id: "RFT-301",
            name: "numpy-assert-warns-to-pytest-warns",
            language: Language::Python,
            severity: Severity::Error,
            default_applicability: "Safe / Unsafe",
            summary: "Migrate `np.testing.assert_warns(...)` to `with pytest.warns(...):`.",
            rationale: "`np.testing.assert_warns` does not forward `match=` kwargs cleanly and is deprecated in modern NumPy/SciPy test suites.",
            bad_example: "np.testing.assert_warns(RuntimeWarning, fn, arg)",
            good_example: "with pytest.warns(RuntimeWarning):\n    fn(arg)",
        },
        RuleDoc {
            id: "RFT-302",
            name: "sqlalchemy-legacy-session-query",
            language: Language::Python,
            severity: Severity::Warning,
            default_applicability: "Safe",
            summary: "Migrate legacy SQLAlchemy 1.x `session.query(Model)` patterns to 2.0 `select(Model)`.",
            rationale: "SQLAlchemy 2.0 deprecates the legacy `Query` API in favor of typed `select()` statements.",
            bad_example: "rows = session.query(User).all()",
            good_example: "rows = session.scalars(select(User)).all()",
        },
    ]
}

fn all_rule_docs() -> Vec<RuleDoc> {
    let mut docs = Vec::new();
    docs.extend_from_slice(python_builtin_rule_catalog());
    docs.extend_from_slice(java_fast_rule_catalog());
    for d in jsts_fast_rule_catalog()
        .iter()
        .chain(jsts_tier2_rule_catalog().iter())
    {
        docs.push(RuleDoc {
            id: d.id,
            name: d.name,
            language: d.language,
            severity: d.severity,
            default_applicability: d.default_applicability,
            summary: d.summary,
            rationale: d.rationale,
            bad_example: d.bad_example,
            good_example: d.good_example,
        });
    }
    docs
}

pub fn run_explain(rule_id: Option<&str>, list: bool) -> Result<(), String> {
    let docs = all_rule_docs();
    if list || rule_id.is_none() {
        println!("Available built-in riffty rules ({}):\n", docs.len());
        println!(
            "{:<18} {:<8} {:<10} {:<28} SUMMARY",
            "RULE ID", "LANG", "SEVERITY", "DEFAULT --fix"
        );
        println!("{}", "-".repeat(100));
        for d in &docs {
            println!(
                "{:<18} {:<8} {:<10} {:<28} {}",
                d.id,
                d.language.as_str(),
                d.severity.as_upper_str(),
                d.default_applicability,
                d.summary
            );
        }
        return Ok(());
    }

    let query = rule_id.unwrap_or_default().trim();
    let Some(doc) = docs
        .iter()
        .find(|d| d.id.eq_ignore_ascii_case(query) || d.name.eq_ignore_ascii_case(query))
    else {
        return Err(format!(
            "unknown rule `{query}` — run `riffty explain --list` to see all built-in rules"
        ));
    };

    println!("{} ({})", doc.id, doc.name);
    println!("  Language:      {}", doc.language.as_str());
    println!("  Severity:      {}", doc.severity.as_upper_str());
    println!("  Default --fix: {}", doc.default_applicability);
    println!("\nSummary:\n  {}", doc.summary);
    println!("\nWhy it matters:\n  {}", doc.rationale);
    println!("\nBefore:\n  {}", doc.bad_example.replace('\n', "\n  "));
    println!("\nAfter:\n  {}", doc.good_example.replace('\n', "\n  "));
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct JsTsDoctorInfo {
    pub workspace_root: String,
    pub tsconfig_path: Option<String>,
    pub package_json_path: Option<String>,
    pub node_modules_present: bool,
    pub zero_node_execution: bool,
    pub tier1: String,
    pub tier2: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub version: String,
    pub workspace_root: String,
    pub workspace_marker: String,
    pub config: String,
    pub python_analyzer: String,
    pub java_tier1: String,
    pub java_tier2: String,
    pub system_java: String,
    pub system_javac: String,
    pub jsts: JsTsDoctorInfo,
    pub mcp_server: String,
}

fn find_nearest_file_within(start: &Path, root: &Path, filenames: &[&str]) -> Option<PathBuf> {
    let start_dir = if start.is_file() {
        start.parent().unwrap_or(start)
    } else {
        start
    };
    for ancestor in start_dir.ancestors() {
        for name in filenames {
            let candidate = ancestor.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if ancestor == root {
            break;
        }
    }
    for name in filenames {
        let candidate = root.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn detect_node_modules_present(start: &Path, root: &Path) -> bool {
    let start_dir = if start.is_file() {
        start.parent().unwrap_or(start)
    } else {
        start
    };
    for ancestor in start_dir.ancestors() {
        if ancestor.join("node_modules").is_dir() {
            return true;
        }
        if ancestor == root {
            break;
        }
    }
    root.join("node_modules").is_dir()
}

pub fn build_doctor_report(target_path: &Path) -> DoctorReport {
    let resolved = WorkspaceResolver::resolve(target_path);
    let toml_status = match RifftyTomlConfig::discover(&resolved.root) {
        Ok(cfg) => {
            if resolved.root.join("riffty.toml").is_file() {
                format!(
                    "loaded riffty.toml (rules={}, exclude={})",
                    cfg.rules
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "none".to_string()),
                    cfg.exclude.len()
                )
            } else if resolved.root.join("pyproject.toml").is_file() {
                "checked pyproject.toml ([tool.riffty])".to_string()
            } else {
                "none (using zero-config defaults)".to_string()
            }
        }
        Err(e) => format!("ERROR: {e}"),
    };

    let java_version = detect_command_first_line("java", &["-version"]);
    let javac_version = detect_command_first_line("javac", &["-version"]);

    let tier2_status = match discover_java_sidecar_bundle() {
        Some(bundle) => match verify_jar_sha256(&bundle.runner_jar, &bundle.expected_sha256) {
            Ok(()) => format!(
                "Active — JDK 25 ({}) + {} [SHA-256 OK]",
                bundle.java_bin.display(),
                bundle.runner_jar.display()
            ),
            Err(e) => format!("BLOCKED — {e}"),
        },
        None => "Not installed (falling back to Tier 1 zero-JVM tree-sitter-java)".to_string(),
    };

    // Pure read-only filesystem checks for JS/TS (zero Node.js / npm / pnpm / yarn / bun execution).
    let tsconfig_path = find_nearest_file_within(
        &resolved.target,
        &resolved.root,
        &["tsconfig.json", "jsconfig.json"],
    )
    .map(|p| p.display().to_string());
    let package_json_path =
        find_nearest_file_within(&resolved.target, &resolved.root, &["package.json"])
            .map(|p| p.display().to_string());
    let node_modules_present = detect_node_modules_present(&resolved.target, &resolved.root);

    DoctorReport {
        version: env!("CARGO_PKG_VERSION").to_string(),
        workspace_root: resolved.root.display().to_string(),
        workspace_marker: format!("{:?}", resolved.marker),
        config: toml_status,
        python_analyzer: "Tier 2 Semantic (ruff-ast + ty, in-process)".to_string(),
        java_tier1: format!(
            "Active — tree-sitter-java (in-process, zero-JVM, {} rules)",
            java_fast_rule_catalog().len()
        ),
        java_tier2: tier2_status,
        system_java: java_version
            .unwrap_or_else(|| "not found on PATH (not required for Tier 1)".to_string()),
        system_javac: javac_version
            .unwrap_or_else(|| "not found on PATH (not required for Tier 1)".to_string()),
        jsts: JsTsDoctorInfo {
            workspace_root: resolved.root.display().to_string(),
            tsconfig_path,
            package_json_path,
            node_modules_present,
            zero_node_execution: true,
            tier1: format!(
                "Active — oxc_parser + oxc_semantic (in-process, zero-Node.js, {} rules)",
                jsts_fast_rule_catalog().len()
            ),
            tier2: format!(
                "Active — tsrs (TS7 Rust, in-process, zero-Node.js, {} rules)",
                jsts_tier2_rule_catalog().len()
            ),
        },
        mcp_server:
            "Hardened (workspace containment + git ref validation + atomic patch + zero-Node.js)"
                .to_string(),
    }
}

pub fn run_doctor(target_path: &Path, json_output: bool) -> Result<(), String> {
    let report = build_doctor_report(target_path);
    if json_output {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| format!("failed to serialize doctor report: {e}"))?;
        println!("{json}");
        return Ok(());
    }

    println!("riffty doctor (v{})", report.version);
    println!("------------------------------------------------------------");
    println!(
        "Workspace root:   {} ({})",
        report.workspace_root, report.workspace_marker
    );
    println!("Config:           {}", report.config);
    println!("Python analyzer:  {}", report.python_analyzer);
    println!("Java Tier 1:      {}", report.java_tier1);
    println!("Java Tier 2:      {}", report.java_tier2);
    println!("System java:      {}", report.system_java);
    println!("System javac:     {}", report.system_javac);
    println!("JS/TS Tier 1:     {}", report.jsts.tier1);
    println!("JS/TS Tier 2:     {}", report.jsts.tier2);
    println!(
        "JS/TS tsconfig:   {}",
        report.jsts.tsconfig_path.as_deref().unwrap_or("not found")
    );
    println!(
        "JS/TS node_mods:  {} (read-only check; zero Node.js/npm execution)",
        if report.jsts.node_modules_present {
            "present"
        } else {
            "absent"
        }
    );
    println!("MCP server:       {}", report.mcp_server);
    Ok(())
}

fn detect_command_first_line(bin: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(bin).args(args).output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn req_m0_05_doctor_reports_jsts_root_tsconfig_node_modules_zero_node_exec() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("riffty_doctor_jsts_{nanos}"));
        let nested = root.join("packages/web/src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join("node_modules/typescript")).unwrap();
        std::fs::write(
            root.join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\n",
        )
        .unwrap();
        std::fs::write(root.join("packages/web/tsconfig.json"), "{}\n").unwrap();
        let target = nested.join("app.ts");
        std::fs::write(&target, "export const n = 1;\n").unwrap();

        let report = build_doctor_report(&target);
        assert_eq!(report.workspace_root, root.display().to_string());
        assert_eq!(report.jsts.workspace_root, root.display().to_string());
        assert_eq!(
            report.jsts.tsconfig_path.as_deref(),
            Some(
                root.join("packages/web/tsconfig.json")
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert!(report.jsts.node_modules_present);
        assert!(report.jsts.zero_node_execution);

        // JSON serialization check
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["jsts"]["node_modules_present"], true);
        assert_eq!(json["jsts"]["zero_node_execution"], true);
        assert!(
            json["jsts"]["tsconfig_path"]
                .as_str()
                .unwrap()
                .ends_with("tsconfig.json")
        );

        // Text + JSON run_doctor both succeed
        run_doctor(&target, false).expect("text doctor ok");
        run_doctor(&target, true).expect("json doctor ok");

        let _ = std::fs::remove_dir_all(&root);
    }
}

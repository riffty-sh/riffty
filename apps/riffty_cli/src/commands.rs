use std::path::Path;
use std::process::Command;

use riffty_adapter_fs::WorkspaceResolver;
use riffty_adapter_java_fast::{RuleDoc, java_fast_rule_catalog};
use riffty_adapter_plugin::{discover_java_sidecar_bundle, verify_jar_sha256};
use riffty_core::{Language, Severity};

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
    docs
}

pub fn run_explain(rule_id: Option<&str>, list: bool) -> Result<(), String> {
    let docs = all_rule_docs();
    if list || rule_id.is_none() {
        println!("Available built-in riffty rules ({}):\n", docs.len());
        println!(
            "{:<18} {:<8} {:<10} {:<28} {}",
            "RULE ID", "LANG", "SEVERITY", "DEFAULT --fix", "SUMMARY"
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

pub fn run_doctor(target_path: &Path) -> Result<(), String> {
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

    println!("riffty doctor (v{})", env!("CARGO_PKG_VERSION"));
    println!("------------------------------------------------------------");
    println!("Workspace root:   {} ({:?})", resolved.root.display(), resolved.marker);
    println!("Config:           {toml_status}");
    println!("Python analyzer:  Tier 2 Semantic (ruff-ast + ty, in-process)");
    println!(
        "Java Tier 1:      Active — tree-sitter-java (in-process, zero-JVM, {} rules)",
        java_fast_rule_catalog().len()
    );
    println!("Java Tier 2:      {tier2_status}");
    println!(
        "System java:      {}",
        java_version.unwrap_or_else(|| "not found on PATH (not required for Tier 1)".to_string())
    );
    println!(
        "System javac:     {}",
        javac_version.unwrap_or_else(|| "not found on PATH (not required for Tier 1)".to_string())
    );
    println!("MCP server:       Hardened (workspace containment + git ref validation + atomic patch)");
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


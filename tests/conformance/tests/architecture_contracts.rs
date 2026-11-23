use std::fs;
use std::path::{Path, PathBuf};

use riffty_adapter_infra::default_infra_registry;
use riffty_adapter_java_fast::JavaFastAnalyzer;
use riffty_adapter_jsts_fast::JsTsFastAnalyzer;
use riffty_adapter_plugin::verify_jar_sha256;
use riffty_adapter_tsrs::TsrsSemanticAnalyzer;
use riffty_core::{
    AnalysisTier, AnalyzePassRequest, FileId, Language, LanguageAnalyzerPort, WorkspaceFile,
};
use riffty_infra_core::InfraCapability;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tests/conformance is 2 levels below workspace root")
        .to_path_buf()
}

fn dependencies_section(manifest: &str) -> &str {
    manifest
        .split_once("[dependencies]")
        .map(|(_, rest)| rest)
        .unwrap_or("")
}

#[test]
fn hexagonal_boundary_riffty_infra_core_is_pure() {
    let manifest = fs::read_to_string(workspace_root().join("crates/riffty_infra_core/Cargo.toml"))
        .expect("read riffty_infra_core Cargo.toml");
    let deps = dependencies_section(&manifest);
    for forbidden in [
        "ureq",
        "reqwest",
        "tokio",
        "clap",
        "ruff_",
        "ty_",
        "oxc_",
        "tsrs_",
        "ast-grep",
        "riffty_adapter_",
    ] {
        assert!(
            !deps.contains(forbidden),
            "riffty_infra_core MUST remain pure (found forbidden dependency `{forbidden}`)"
        );
    }
}

#[test]
fn hexagonal_boundary_riffty_ab_core_is_pure() {
    let manifest = fs::read_to_string(workspace_root().join("crates/riffty_ab_core/Cargo.toml"))
        .expect("read riffty_ab_core Cargo.toml");
    let deps = dependencies_section(&manifest);
    for forbidden in [
        "ureq",
        "reqwest",
        "tokio",
        "clap",
        "ruff_",
        "ty_",
        "oxc_",
        "tsrs_",
        "ast-grep",
        "riffty_adapter_",
    ] {
        assert!(
            !deps.contains(forbidden),
            "riffty_ab_core MUST remain pure (found forbidden dependency `{forbidden}`)"
        );
    }
}

#[test]
fn infra_adapters_have_zero_curl_shell_outs() {
    let infra_src = workspace_root().join("crates/adapters/riffty_adapter_infra/src");
    for file in ["prometheus.rs", "opencost.rs"] {
        let src = fs::read_to_string(infra_src.join(file)).expect("read infra adapter source");
        assert!(
            !src.contains("Command::new(\"curl\")"),
            "{file} must use native Rust HTTP client instead of shelling out to `curl`"
        );
    }
}

#[test]
fn infra_registry_registers_core_only_and_connected_providers() {
    let registry = default_infra_registry();
    let providers = registry.list();
    let ids: Vec<&str> = providers.iter().map(|d| d.id.as_str()).collect();
    for expected in [
        "local-jfr-metrics",
        "kubernetes-hpa",
        "public-pricing",
        "prometheus-metrics",
        "opencost-billing",
    ] {
        assert!(
            ids.contains(&expected),
            "expected infra provider `{expected}` in default registry"
        );
    }

    // CoreOnly capabilities (Metrics & Pricing) must always resolve offline.
    assert_eq!(
        registry
            .resolve_for_capability(InfraCapability::Metrics)
            .map(|d| d.id.as_str()),
        Some("local-jfr-metrics")
    );
    assert_eq!(
        registry
            .resolve_for_capability(InfraCapability::Pricing)
            .map(|d| d.id.as_str()),
        Some("public-pricing")
    );
}

#[test]
fn java_and_jsts_analyzers_implement_language_port_and_report_tier() {
    let java_analyzer = JavaFastAnalyzer;
    assert_eq!(java_analyzer.language(), Language::Java);

    let files = vec![WorkspaceFile {
        file_id: FileId(1),
        path: PathBuf::from("Demo.java"),
        relative_path: "Demo.java".to_string(),
        source:
            "import java.math.BigDecimal;\nclass Demo { BigDecimal x = new BigDecimal(0.1); }\n"
                .to_string(),
    }];
    let java_req = AnalyzePassRequest {
        workspace_root: Path::new("."),
        files: &files,
        scoped_ranges: None,
        rules_paths: &[],
        allow_unsafe: false,
        pass_index: 0,
    };
    let java_resp = java_analyzer
        .analyze_pass(&java_req)
        .expect("JavaFastAnalyzer succeeds");
    assert_eq!(java_resp.provenance.tier, AnalysisTier::Tier1Syntactic);
    assert!(
        !java_resp.diagnostics.is_empty(),
        "JavaFastAnalyzer should flag new BigDecimal(double)"
    );

    let jsts_fast = JsTsFastAnalyzer {
        refaster_rules: Vec::new(),
    };
    assert_eq!(jsts_fast.language(), Language::JsTs);

    let tsrs = TsrsSemanticAnalyzer {
        refaster_rules: Vec::new(),
        tier1_only: false,
        architecture_forbid: Vec::new(),
    };
    assert_eq!(tsrs.language(), Language::JsTs);
}

#[test]
fn java_sidecar_sha256_verifier_fails_closed_on_mismatch() {
    let manifest_path = workspace_root().join("Cargo.toml");
    let err = verify_jar_sha256(
        &manifest_path,
        "0000000000000000000000000000000000000000000000000000000000000000",
    )
    .expect_err("SHA-256 mismatch must fail closed");
    assert!(err.contains("SHA-256 mismatch"));
}

use std::collections::BTreeMap;

use riffty_core::AnalysisProvenance;
use serde_json::{Value, json};

use crate::JsonDiagnosticEntry;

/// Renders diagnostics and provenance as a SARIF 2.1.0 JSON document
/// compatible with GitHub Code Scanning and enterprise SAST dashboards.
pub fn render_sarif_report(
    diagnostics: &[JsonDiagnosticEntry],
    provenance: &[AnalysisProvenance],
) -> Value {
    let mut rules_map: BTreeMap<&str, Value> = BTreeMap::new();
    let mut results: Vec<Value> = Vec::with_capacity(diagnostics.len());

    for d in diagnostics {
        rules_map.entry(d.rule_id.as_str()).or_insert_with(|| {
            json!({
                "id": d.rule_id,
                "name": d.rule_name,
                "shortDescription": { "text": d.rule_name },
                "defaultConfiguration": {
                    "level": sarif_level(&d.severity)
                }
            })
        });

        let mut props = serde_json::Map::new();
        if let Some(ref app) = d.applicability {
            props.insert("applicability".to_string(), json!(app));
        }
        if let Some(ref action) = d.suggested_action {
            props.insert("suggested_action".to_string(), json!(action));
        }
        if let Some(ref proof) = d.type_proof {
            props.insert("type_proof".to_string(), proof.clone());
        }

        results.push(json!({
            "ruleId": d.rule_id,
            "level": sarif_level(&d.severity),
            "message": { "text": d.message },
            "locations": [{
                "physicalLocation": {
                    "artifactLocation": { "uri": d.file },
                    "region": {
                        "startLine": d.line,
                        "startColumn": d.column
                    }
                }
            }],
            "properties": props
        }));
    }

    let rules: Vec<Value> = rules_map.into_values().collect();
    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "riffty",
                    "version": env!("CARGO_PKG_VERSION"),
                    "informationUri": "https://github.com/riffty-sh/riffty",
                    "rules": rules
                }
            },
            "results": results,
            "properties": {
                "provenance": provenance
            }
        }]
    })
}

fn sarif_level(severity: &str) -> &'static str {
    match severity {
        "ERROR" => "error",
        "WARNING" => "warning",
        _ => "note",
    }
}


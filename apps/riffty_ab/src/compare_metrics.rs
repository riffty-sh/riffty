use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use riffty_ab_core::{
    CURRENT_SCHEMA_VERSION, PerfComparison, compute_perf_comparison, compute_sha256_hex,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, clap::Args)]
pub struct CompareMetricsArgs {
    /// Path to input JSON containing multi-metric A/B samples
    #[arg(long)]
    pub input: PathBuf,

    /// Optional output path for structured `MultiMetricAbReport` JSON
    #[arg(long)]
    pub out_json: Option<PathBuf>,

    /// Optional output path for Markdown summary report
    #[arg(long)]
    pub out_md: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetricSeriesInput {
    pub name: String,
    pub unit: String,
    pub higher_is_better: bool,
    pub samples_a: Vec<f64>,
    pub samples_b: Vec<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MultiMetricInput {
    pub case_id: String,
    pub execution_order: Vec<String>,
    pub window_secs: u64,
    #[serde(default)]
    pub warmup_secs: u64,
    pub metrics: Vec<MetricSeriesInput>,
    #[serde(default)]
    pub evidence_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiMetricEntry {
    pub unit: String,
    pub comparison: PerfComparison,
    pub verdict: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiMetricAbReport {
    pub schema_version: u32,
    pub case_id: String,
    pub execution_order: Vec<String>,
    pub window_secs: u64,
    pub warmup_secs: u64,
    pub metrics: Vec<MultiMetricEntry>,
    pub overall_verdict: String,
    pub evidence_hashes: BTreeMap<String, String>,
    pub report_sha256: String,
}

pub fn build_multi_metric_report(
    input: &MultiMetricInput,
    base_dir: Option<&Path>,
) -> Result<MultiMetricAbReport, String> {
    if input.metrics.is_empty() {
        return Err("compare-metrics input must contain at least one metric".to_string());
    }

    let mut entries = Vec::with_capacity(input.metrics.len());
    let mut improved_count = 0usize;
    let mut regressed_count = 0usize;

    for m in &input.metrics {
        let comparison = compute_perf_comparison(
            m.name.clone(),
            m.higher_is_better,
            0,
            m.samples_a.clone(),
            m.samples_b.clone(),
        )?;

        let median_regressed = if m.higher_is_better {
            comparison.stats_b.median < comparison.stats_a.median
        } else {
            comparison.stats_b.median > comparison.stats_a.median
        };
        let paired_losses = comparison
            .measured_pairs
            .saturating_sub(comparison.paired_wins);

        let verdict = if comparison.improved {
            improved_count += 1;
            "IMPROVED".to_string()
        } else if median_regressed && paired_losses * 2 > comparison.measured_pairs {
            regressed_count += 1;
            "REGRESSED".to_string()
        } else {
            "INCONCLUSIVE".to_string()
        };

        entries.push(MultiMetricEntry {
            unit: m.unit.clone(),
            comparison,
            verdict,
        });
    }

    let overall_verdict = if regressed_count > 0 && improved_count == 0 {
        "REGRESSED".to_string()
    } else if improved_count == entries.len() {
        "VERIFIED_IMPROVED".to_string()
    } else if improved_count > 0 && regressed_count == 0 {
        "PARTIALLY_IMPROVED".to_string()
    } else if improved_count > 0 && regressed_count > 0 {
        "MIXED".to_string()
    } else {
        "INCONCLUSIVE".to_string()
    };

    let mut evidence_hashes = BTreeMap::new();
    for ev in &input.evidence_files {
        let path = PathBuf::from(ev);
        let resolved = if path.is_absolute() || path.exists() {
            path
        } else if let Some(dir) = base_dir {
            dir.join(ev)
        } else {
            path
        };
        let bytes = std::fs::read(&resolved).map_err(|e| {
            format!(
                "failed to read evidence file '{}' (resolved '{}'): {e}",
                ev,
                resolved.display()
            )
        })?;
        evidence_hashes.insert(ev.clone(), compute_sha256_hex(&bytes));
    }

    let mut report = MultiMetricAbReport {
        schema_version: CURRENT_SCHEMA_VERSION,
        case_id: input.case_id.clone(),
        execution_order: input.execution_order.clone(),
        window_secs: input.window_secs,
        warmup_secs: input.warmup_secs,
        metrics: entries,
        overall_verdict,
        evidence_hashes,
        report_sha256: String::new(),
    };

    let canonical_bytes = serde_json::to_vec(&report)
        .map_err(|e| format!("failed to serialize canonical report for hashing: {e}"))?;
    report.report_sha256 = compute_sha256_hex(&canonical_bytes);

    Ok(report)
}

pub fn render_multi_metric_md(report: &MultiMetricAbReport) -> String {
    let mut md = String::new();
    md.push_str(&format!(
        "# `riffty-ab` Multi-Metric A/B Report — `{}`\n\n",
        report.case_id
    ));
    md.push_str(&format!("- **Overall Verdict:** `{}`\n", report.overall_verdict));
    md.push_str(&format!(
        "- **Execution Order:** `{}`\n",
        report.execution_order.join(", ")
    ));
    md.push_str(&format!(
        "- **Window / Warmup:** `{}s` / `{}s`\n",
        report.window_secs, report.warmup_secs
    ));
    md.push_str(&format!("- **Report SHA-256:** `{}`\n\n", report.report_sha256));

    md.push_str("## Per-Metric Comparison (Baseline A vs. Fixed B)\n\n");
    md.push_str("| Metric | Unit | Baseline Median | Fixed Median | Baseline Mean ± 95% CI | Fixed Mean ± 95% CI | Baseline p95 / p99 / max | Fixed p95 / p99 / max | Delta % | Paired Wins | Verdict |\n");
    md.push_str("| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |\n");

    for entry in &report.metrics {
        let c = &entry.comparison;
        let a = &c.stats_a;
        let b = &c.stats_b;
        md.push_str(&format!(
            "| `{}` | `{}` | {:.3} | {:.3} | {:.3} ± {:.3} | {:.3} ± {:.3} | {:.3} / {:.3} / {:.3} | {:.3} / {:.3} / {:.3} | {:+.2}% | {}/{} | `{}` |\n",
            c.metric_name,
            entry.unit,
            a.median,
            b.median,
            a.mean,
            a.ci95_half_width,
            b.mean,
            b.ci95_half_width,
            a.p95,
            a.p99,
            a.max,
            b.p95,
            b.p99,
            b.max,
            c.delta_pct,
            c.paired_wins,
            c.measured_pairs,
            entry.verdict
        ));
    }

    if !report.evidence_hashes.is_empty() {
        md.push_str("\n## Evidence File SHA-256 Attestation\n\n");
        md.push_str("| File | SHA-256 |\n");
        md.push_str("| :--- | :--- |\n");
        for (file, sha) in &report.evidence_hashes {
            md.push_str(&format!("| `{file}` | `{sha}` |\n"));
        }
    }

    md
}

pub fn run_compare_metrics(args: CompareMetricsArgs) -> Result<MultiMetricAbReport, String> {
    let raw = std::fs::read_to_string(&args.input)
        .map_err(|e| format!("failed to read {}: {e}", args.input.display()))?;
    let input: MultiMetricInput = serde_json::from_str(&raw)
        .map_err(|e| format!("invalid JSON in {}: {e}", args.input.display()))?;

    let report = build_multi_metric_report(&input, args.input.parent())?;
    let json_pretty = serde_json::to_string_pretty(&report)
        .map_err(|e| format!("failed to serialize MultiMetricAbReport: {e}"))?;
    let md = render_multi_metric_md(&report);

    if let Some(ref out_json) = args.out_json {
        if let Some(parent) = out_json.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!("failed to create directory {}: {e}", parent.display())
            })?;
        }
        std::fs::write(out_json, format!("{json_pretty}\n"))
            .map_err(|e| format!("failed to write {}: {e}", out_json.display()))?;
    }

    if let Some(ref out_md) = args.out_md {
        if let Some(parent) = out_md.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!("failed to create directory {}: {e}", parent.display())
            })?;
        }
        std::fs::write(out_md, &md)
            .map_err(|e| format!("failed to write {}: {e}", out_md.display()))?;
    }

    if args.out_json.is_none() && args.out_md.is_none() {
        println!("{json_pretty}");
    } else {
        println!(
            "COMPARE-METRICS: {} | verdict={} | metrics={} | sha256={}",
            report.case_id,
            report.overall_verdict,
            report.metrics.len(),
            report.report_sha256
        );
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_metrics_produces_valid_json_markdown_and_deterministic_sha256() {
        let tmp = std::env::temp_dir().join(format!(
            "riffty-ab-compare-metrics-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let evidence_path = tmp.join("prom_snapshot.json");
        std::fs::write(&evidence_path, r#"{"status":"success"}"#).unwrap();

        let input_path = tmp.join("metrics_input.json");
        let input_json = serde_json::json!({
            "case_id": "k8s-spring-petclinic-cpu",
            "execution_order": ["AB", "BA", "AB", "BA", "AB"],
            "window_secs": 120,
            "warmup_secs": 30,
            "metrics": [
                {
                    "name": "cpu_core_seconds",
                    "unit": "core-s",
                    "higher_is_better": false,
                    "samples_a": [120.0, 122.0, 118.0, 121.0, 119.0],
                    "samples_b": [72.0, 74.0, 70.0, 73.0, 71.0]
                },
                {
                    "name": "throughput",
                    "unit": "rps",
                    "higher_is_better": true,
                    "samples_a": [1000.0, 1020.0, 990.0, 1010.0, 1005.0],
                    "samples_b": [1500.0, 1520.0, 1480.0, 1510.0, 1495.0]
                }
            ],
            "evidence_files": ["prom_snapshot.json"]
        });
        std::fs::write(
            &input_path,
            serde_json::to_string_pretty(&input_json).unwrap(),
        )
        .unwrap();

        let out_json_1 = tmp.join("out1/report.json");
        let out_md_1 = tmp.join("out1/REPORT.md");
        let report_1 = run_compare_metrics(CompareMetricsArgs {
            input: input_path.clone(),
            out_json: Some(out_json_1.clone()),
            out_md: Some(out_md_1.clone()),
        })
        .unwrap();

        let out_json_2 = tmp.join("out2/report.json");
        let out_md_2 = tmp.join("out2/REPORT.md");
        let report_2 = run_compare_metrics(CompareMetricsArgs {
            input: input_path,
            out_json: Some(out_json_2),
            out_md: Some(out_md_2),
        })
        .unwrap();

        // Deterministic SHA-256 across repeated runs
        assert_eq!(report_1.report_sha256.len(), 64);
        assert_eq!(report_1.report_sha256, report_2.report_sha256);
        assert_eq!(report_1.overall_verdict, "VERIFIED_IMPROVED");
        assert_eq!(report_1.evidence_hashes.len(), 1);
        assert_eq!(
            report_1.evidence_hashes["prom_snapshot.json"],
            compute_sha256_hex(br#"{"status":"success"}"#)
        );

        // JSON file parses cleanly back into MultiMetricAbReport
        let parsed_json: MultiMetricAbReport =
            serde_json::from_str(&std::fs::read_to_string(&out_json_1).unwrap()).unwrap();
        assert_eq!(parsed_json.case_id, report_1.case_id);
        assert_eq!(parsed_json.report_sha256, report_1.report_sha256);
        assert_eq!(parsed_json.overall_verdict, report_1.overall_verdict);
        assert_eq!(parsed_json.evidence_hashes, report_1.evidence_hashes);
        assert_eq!(parsed_json.metrics.len(), 2);
        assert!(
            (parsed_json.metrics[0].comparison.stats_a.ci95_half_width
                - report_1.metrics[0].comparison.stats_a.ci95_half_width)
                .abs()
                < 1e-12
        );
        assert!(parsed_json.metrics[0].comparison.stats_a.ci95_half_width > 0.0);
        assert_eq!(parsed_json.metrics[0].comparison.stats_a.p99, 122.0);
        assert_eq!(parsed_json.metrics[0].comparison.stats_a.max, 122.0);

        // Markdown contains key sections and columns
        let md = std::fs::read_to_string(&out_md_1).unwrap();
        assert!(md.contains("k8s-spring-petclinic-cpu"));
        assert!(md.contains("AB, BA, AB, BA, AB"));
        assert!(md.contains("VERIFIED_IMPROVED"));
        assert!(md.contains(&report_1.report_sha256));
        assert!(md.contains("cpu_core_seconds"));
        assert!(md.contains("throughput"));
        assert!(md.contains("±"));
        assert!(md.contains("5/5"));
        assert!(md.contains("prom_snapshot.json"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

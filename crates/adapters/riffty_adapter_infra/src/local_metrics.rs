use std::fs;
use std::path::{Path, PathBuf};

use riffty_infra_core::{
    MeasurementWindow, MetricsProviderPort, ObservedMetrics, WorkloadReference,
};
use serde_json::Value;

/// Adapter reading observed metrics from local benchmark artifacts (JSON / JSONL / JFR summaries).
#[derive(Debug, Clone)]
pub struct LocalBenchmarkMetricsAdapter {
    pub path: PathBuf,
    pub side: Option<String>,
}

impl LocalBenchmarkMetricsAdapter {
    pub fn new(path: impl Into<PathBuf>, side: Option<String>) -> Self {
        Self {
            path: path.into(),
            side,
        }
    }
}

impl MetricsProviderPort for LocalBenchmarkMetricsAdapter {
    fn collect(
        &self,
        _workload: &WorkloadReference,
        _window: &MeasurementWindow,
    ) -> Result<ObservedMetrics, String> {
        parse_metrics_file(&self.path, self.side.as_deref())
    }
}

/// Parses a local benchmark metrics artifact into `ObservedMetrics`.
///
/// Supported formats:
/// 1. Direct `ObservedMetrics` JSON object.
/// 2. Track B1/B2/B3 `01_before_after_http.json` (selects `"baseline"` or `"fixed"` via `side`).
/// 3. Quarkus `05_comparison_summary.json` (`payloads[]` array with `"baseline"`/`"fixed"` entries).
/// 4. JSONL benchmark streams (`02_workload_results.jsonl`, `03_https_tls_results.jsonl`).
pub fn parse_metrics_file(path: &Path, side: Option<&str>) -> Result<ObservedMetrics, String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("failed to read metrics file '{}': {e}", path.display()))?;
    parse_metrics_str(&raw, side)
        .map_err(|e| format!("failed to parse metrics file '{}': {e}", path.display()))
}

pub fn parse_metrics_str(raw: &str, side: Option<&str>) -> Result<ObservedMetrics, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("metrics input is empty".to_string());
    }

    // Try single JSON document first
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return parse_json_value(&value, side);
    }

    // Fall back to JSONL (e.g. 02_workload_results.jsonl / 03_https_tls_results.jsonl)
    parse_jsonl_records(trimmed, side)
}

fn canonical_side(side: Option<&str>) -> &str {
    match side.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("optimized") | Some("fixed") | Some("after") => "fixed",
        Some("baseline") | Some("before") => "baseline",
        _ => "baseline",
    }
}

fn parse_json_value(value: &Value, side: Option<&str>) -> Result<ObservedMetrics, String> {
    let Some(obj) = value.as_object() else {
        return Err("expected top-level JSON object".to_string());
    };

    // Case 1: Quarkus `05_comparison_summary.json` (`{ "payloads": [ ... ] }`)
    if let Some(payloads) = obj.get("payloads").and_then(Value::as_array) {
        return parse_quarkus_comparison_summary(payloads, side);
    }

    // Case 2: Track B1/B2/B3 `01_before_after_http.json` (`{ "baseline": { "http": ... }, "fixed": { "http": ... } }`)
    let target_key = canonical_side(side);
    if obj.contains_key("baseline") && (obj.contains_key("fixed") || obj.contains_key("optimized")) {
        let side_val = obj
            .get(target_key)
            .or_else(|| {
                if target_key == "fixed" {
                    obj.get("optimized")
                } else {
                    None
                }
            })
            .ok_or_else(|| format!("missing '{target_key}' section in comparison JSON"))?;
        return parse_track_side_object(side_val);
    }

    // Case 3: Direct `ObservedMetrics` JSON (or single workload row)
    if obj.contains_key("rps")
        || obj.contains_key("p99_ms")
        || obj.contains_key("cpu_us_per_req")
        || obj.contains_key("avg_replicas")
        || obj.contains_key("http")
        || obj.contains_key("latency_ms")
    {
        if obj.contains_key("http") || obj.contains_key("latency_ms") {
            return parse_track_side_object(value);
        }
        let metrics: ObservedMetrics = serde_json::from_value(value.clone())
            .map_err(|e| format!("invalid ObservedMetrics JSON: {e}"))?;
        return Ok(metrics);
    }

    Err("unrecognized JSON metrics schema".to_string())
}

fn parse_track_side_object(side_val: &Value) -> Result<ObservedMetrics, String> {
    let Some(obj) = side_val.as_object() else {
        return Err("expected JSON object for benchmark side".to_string());
    };

    let http = obj.get("http").and_then(Value::as_object);
    let process = obj.get("process").and_then(Value::as_object);
    let latency_ms = obj.get("latency_ms").and_then(Value::as_object);

    let rps = http
        .and_then(|h| h.get("rps").and_then(Value::as_f64))
        .or_else(|| obj.get("rps").and_then(Value::as_f64));
    let p50_ms = http
        .and_then(|h| h.get("p50_ms").and_then(Value::as_f64))
        .or_else(|| latency_ms.and_then(|l| l.get("p50").and_then(Value::as_f64)))
        .or_else(|| obj.get("p50_ms").and_then(Value::as_f64));
    let p95_ms = http
        .and_then(|h| h.get("p95_ms").and_then(Value::as_f64))
        .or_else(|| latency_ms.and_then(|l| l.get("p95").and_then(Value::as_f64)))
        .or_else(|| obj.get("p95_ms").and_then(Value::as_f64));
    let p99_ms = http
        .and_then(|h| h.get("p99_ms").and_then(Value::as_f64))
        .or_else(|| latency_ms.and_then(|l| l.get("p99").and_then(Value::as_f64)))
        .or_else(|| obj.get("p99_ms").and_then(Value::as_f64));

    let requests_total = http
        .and_then(|h| {
            h.get("total_ops")
                .or_else(|| h.get("requests_total"))
                .and_then(Value::as_u64)
        })
        .or_else(|| {
            obj.get("requests_total")
                .or_else(|| obj.get("total_ops"))
                .and_then(Value::as_u64)
        });

    let errors_total = http
        .and_then(|h| {
            h.get("errors")
                .or_else(|| h.get("failed_ops"))
                .and_then(Value::as_u64)
        })
        .or_else(|| {
            obj.get("errors")
                .or_else(|| obj.get("errors_total"))
                .and_then(Value::as_u64)
        });

    let error_rate_pct = match (errors_total, requests_total) {
        (Some(e), Some(t)) if t > 0 => Some((e as f64 / t as f64) * 100.0),
        (Some(0), Some(0)) => Some(0.0),
        _ => obj.get("error_rate_pct").and_then(Value::as_f64),
    };

    // process.cpu_pct_avg (e.g., 447.75% of 1 core) -> 4477.5 millicores
    let cpu_millicores_avg = process
        .and_then(|p| p.get("cpu_pct_avg").and_then(Value::as_f64))
        .map(|pct| pct * 10.0)
        .or_else(|| obj.get("cpu_millicores_avg").and_then(Value::as_f64));

    let memory_mib_avg = process
        .and_then(|p| p.get("rss_mb_avg").and_then(Value::as_f64))
        .or_else(|| obj.get("memory_mib_avg").and_then(Value::as_f64));

    let cpu_us_per_req = obj.get("cpu_us_per_req").and_then(Value::as_f64).or_else(|| {
        match (cpu_millicores_avg, rps) {
            (Some(mc), Some(r)) if r > 0.0 => Some((mc * 1000.0) / r),
            _ => None,
        }
    });

    Ok(ObservedMetrics {
        rps,
        p50_ms,
        p95_ms,
        p99_ms,
        error_rate_pct,
        cpu_us_per_req,
        cpu_millicores_avg,
        memory_mib_avg,
        gc_pause_ms_per_1k_req: obj.get("gc_pause_ms_per_1k_req").and_then(Value::as_f64),
        avg_replicas: obj.get("avg_replicas").and_then(Value::as_f64),
        peak_replicas: obj
            .get("peak_replicas")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        pod_minutes: obj.get("pod_minutes").and_then(Value::as_f64),
        requests_total,
        errors_total,
    })
}

fn parse_quarkus_comparison_summary(
    payloads: &[Value],
    side: Option<&str>,
) -> Result<ObservedMetrics, String> {
    if payloads.is_empty() {
        return Err("comparison summary contains empty 'payloads' array".to_string());
    }
    let key = canonical_side(side);
    let mut rows = Vec::new();
    for entry in payloads {
        if let Some(side_obj) = entry.get(key) {
            rows.push(parse_track_side_object(side_obj)?);
        }
    }
    if rows.is_empty() {
        return Err(format!(
            "no '{key}' entries found inside comparison summary 'payloads'"
        ));
    }
    Ok(aggregate_metrics(&rows))
}

fn parse_jsonl_records(raw: &str, side: Option<&str>) -> Result<ObservedMetrics, String> {
    let target_mode = side.map(|s| canonical_side(Some(s)));
    let mut parsed_rows: Vec<(Option<String>, ObservedMetrics)> = Vec::new();

    for (idx, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: Value = serde_json::from_str(line)
            .map_err(|e| format!("invalid JSONL on line {}: {e}", idx + 1))?;
        let Some(obj) = val.as_object() else {
            return Err(format!("expected JSON object on line {}", idx + 1));
        };
        let mode = obj
            .get("mode")
            .and_then(Value::as_str)
            .map(|m| canonical_side(Some(m)).to_string());

        let total_ops = obj
            .get("total_ops")
            .or_else(|| obj.get("requests_total"))
            .and_then(Value::as_u64);
        let failed_ops = obj
            .get("failed_ops")
            .or_else(|| obj.get("errors"))
            .and_then(Value::as_u64);
        let rps = obj.get("rps").and_then(Value::as_f64);
        let p50_ms = obj.get("p50_ms").and_then(Value::as_f64);
        let p95_ms = obj.get("p95_ms").and_then(Value::as_f64);
        let p99_ms = obj.get("p99_ms").and_then(Value::as_f64);
        let cpu_us_per_req = obj.get("cpu_us_per_req").and_then(Value::as_f64);

        let cpu_millicores_avg = match (
            obj.get("process_cpu_sec").and_then(Value::as_f64),
            obj.get("wall_sec").and_then(Value::as_f64),
        ) {
            (Some(cpu_s), Some(wall_s)) if wall_s > 0.0 => Some((cpu_s / wall_s) * 1000.0),
            _ => obj.get("cpu_millicores_avg").and_then(Value::as_f64),
        };

        let gc_pause_ms_per_1k_req = match (
            obj.get("gc_time_ms").and_then(Value::as_f64),
            total_ops,
        ) {
            (Some(gc_ms), Some(ops)) if ops > 0 => Some(gc_ms / (ops as f64 / 1000.0)),
            _ => None,
        };

        let error_rate_pct = match (failed_ops, total_ops) {
            (Some(e), Some(t)) if t > 0 => Some((e as f64 / t as f64) * 100.0),
            (Some(0), Some(0)) => Some(0.0),
            _ => None,
        };

        parsed_rows.push((
            mode,
            ObservedMetrics {
                rps,
                p50_ms,
                p95_ms,
                p99_ms,
                error_rate_pct,
                cpu_us_per_req,
                cpu_millicores_avg,
                memory_mib_avg: obj.get("memory_mib_avg").and_then(Value::as_f64),
                gc_pause_ms_per_1k_req,
                avg_replicas: obj.get("avg_replicas").and_then(Value::as_f64),
                peak_replicas: obj
                    .get("peak_replicas")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32),
                pod_minutes: obj.get("pod_minutes").and_then(Value::as_f64),
                requests_total: total_ops,
                errors_total: failed_ops,
            },
        ));
    }

    let filtered: Vec<ObservedMetrics> = parsed_rows
        .into_iter()
        .filter(|(mode, _)| match (target_mode, mode.as_deref()) {
            (Some(want), Some(have)) => want == have,
            _ => true,
        })
        .map(|(_, m)| m)
        .collect();

    if filtered.is_empty() {
        return Err(format!(
            "no matching JSONL records found for side '{}'",
            target_mode.unwrap_or("any")
        ));
    }

    // Prefer non-zero-op records when a stream contains failed/zero-op runs alongside valid runs
    let non_zero: Vec<ObservedMetrics> = filtered
        .iter()
        .filter(|m| m.requests_total.unwrap_or(1) > 0)
        .cloned()
        .collect();
    let chosen = if non_zero.is_empty() {
        &filtered
    } else {
        &non_zero
    };

    Ok(aggregate_metrics(chosen))
}

fn aggregate_metrics(rows: &[ObservedMetrics]) -> ObservedMetrics {
    let avg = |extract: fn(&ObservedMetrics) -> Option<f64>| -> Option<f64> {
        let vals: Vec<f64> = rows.iter().filter_map(extract).collect();
        if vals.is_empty() {
            None
        } else {
            Some(vals.iter().sum::<f64>() / vals.len() as f64)
        }
    };

    let requests_total = {
        let vals: Vec<u64> = rows.iter().filter_map(|r| r.requests_total).collect();
        if vals.is_empty() {
            None
        } else {
            Some(vals.iter().sum())
        }
    };
    let errors_total = {
        let vals: Vec<u64> = rows.iter().filter_map(|r| r.errors_total).collect();
        if vals.is_empty() {
            None
        } else {
            Some(vals.iter().sum())
        }
    };
    let peak_replicas = rows.iter().filter_map(|r| r.peak_replicas).max();

    let error_rate_pct = match (errors_total, requests_total) {
        (Some(e), Some(t)) if t > 0 => Some((e as f64 / t as f64) * 100.0),
        _ => avg(|r| r.error_rate_pct),
    };

    ObservedMetrics {
        rps: avg(|r| r.rps),
        p50_ms: avg(|r| r.p50_ms),
        p95_ms: avg(|r| r.p95_ms),
        p99_ms: avg(|r| r.p99_ms),
        error_rate_pct,
        cpu_us_per_req: avg(|r| r.cpu_us_per_req),
        cpu_millicores_avg: avg(|r| r.cpu_millicores_avg),
        memory_mib_avg: avg(|r| r.memory_mib_avg),
        gc_pause_ms_per_1k_req: avg(|r| r.gc_pause_ms_per_1k_req),
        avg_replicas: avg(|r| r.avg_replicas),
        peak_replicas,
        pod_minutes: avg(|r| r.pod_minutes),
        requests_total,
        errors_total,
    }
}

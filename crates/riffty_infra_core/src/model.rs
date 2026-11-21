use serde::{Deserialize, Serialize};

/// Identifies a target service or workload under measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadReference {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub platform: String,
}

impl WorkloadReference {
    pub fn new(name: impl Into<String>, platform: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            namespace: None,
            platform: platform.into(),
        }
    }

    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }
}

/// Time window over which metrics or scaling telemetry were collected.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MeasurementWindow {
    pub duration_secs: f64,
    pub samples: usize,
}

/// Raw observed performance, resource, and autoscaling metrics.
/// Strictly separated from estimated or billed monetary costs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ObservedMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_rate_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_us_per_req: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_millicores_avg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib_avg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_pause_ms_per_1k_req: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_replicas: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_replicas: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_minutes: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors_total: Option<u64>,
}

impl ObservedMetrics {
    /// Returns explicit `error_rate_pct`, or derives it from `errors_total / requests_total`.
    pub fn effective_error_rate_pct(&self) -> Option<f64> {
        if let Some(rate) = self.error_rate_pct {
            return Some(rate);
        }
        match (self.errors_total, self.requests_total) {
            (Some(errors), Some(total)) if total > 0 => Some((errors as f64 / total as f64) * 100.0),
            (Some(0), Some(0)) => Some(0.0),
            _ => None,
        }
    }

    /// Returns explicit `cpu_us_per_req`, or derives it from `cpu_millicores_avg` and `rps`.
    pub fn effective_cpu_us_per_req(&self) -> Option<f64> {
        if let Some(cpu_us) = self.cpu_us_per_req {
            return Some(cpu_us);
        }
        match (self.cpu_millicores_avg, self.rps) {
            (Some(mc), Some(rps)) if rps > 0.0 => {
                // mc millicores = (mc / 1000.0) vCPUs = (mc * 1000.0) CPU-microseconds per wall second
                Some((mc * 1000.0) / rps)
            }
            _ => None,
        }
    }

    /// Merges autoscaling telemetry into this metrics record without overwriting non-empty HTTP fields.
    pub fn merge_autoscaling(&mut self, scaling: &AutoscalingSummary) {
        self.avg_replicas = Some(scaling.avg_replicas);
        self.peak_replicas = Some(scaling.peak_replicas);
        self.pod_minutes = Some(scaling.pod_minutes);
        if self.cpu_millicores_avg.is_none() {
            self.cpu_millicores_avg = scaling.avg_cpu_millicores;
        }
        if self.memory_mib_avg.is_none() {
            self.memory_mib_avg = scaling.avg_memory_mib;
        }
    }
}

/// Normalized cloud compute pricing model used to project estimated costs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricingModel {
    pub provider: String,
    pub region: String,
    pub currency: String,
    pub vcpu_per_hour: f64,
    pub memory_gib_per_hour: f64,
    pub billing_model: String,
    #[serde(default = "default_target_utilization")]
    pub target_utilization: f64,
}

fn default_target_utilization() -> f64 {
    0.70
}

/// Strict provenance classification distinguishing modeled estimates from actual cloud bills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostKind {
    Estimated,
    ActualBilled,
}

/// Monetary cost attribution with explicit provenance (`CostKind`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostAttribution {
    pub kind: CostKind,
    pub currency: String,
    pub hourly_cost: f64,
    pub monthly_cost: f64,
    pub annual_cost: f64,
    pub basis: String,
    pub pricing_source: String,
}

/// Service-Level Objective thresholds checked against observed metrics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SloPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_p99_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_error_rate_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_rps: Option<f64>,
}

impl SloPolicy {
    pub fn evaluate(&self, metrics: &ObservedMetrics) -> SloEvaluation {
        let mut violations = Vec::new();

        if let Some(max_p99) = self.max_p99_ms {
            if let Some(actual_p99) = metrics.p99_ms {
                if actual_p99 > max_p99 {
                    violations.push(format!(
                        "p99 latency {actual_p99:.3}ms exceeded max_p99_ms {max_p99:.3}ms"
                    ));
                }
            }
        }

        if let Some(max_err) = self.max_error_rate_pct {
            if let Some(actual_err) = metrics.effective_error_rate_pct() {
                if actual_err > max_err {
                    violations.push(format!(
                        "error rate {actual_err:.3}% exceeded max_error_rate_pct {max_err:.3}%"
                    ));
                }
            }
        }

        if let Some(min_rps) = self.min_rps {
            if let Some(actual_rps) = metrics.rps {
                if actual_rps < min_rps {
                    violations.push(format!(
                        "throughput {actual_rps:.2} RPS below min_rps {min_rps:.2} RPS"
                    ));
                }
            }
        }

        SloEvaluation {
            passed: violations.is_empty(),
            violations,
        }
    }
}

/// Result of checking a `SloPolicy` against `ObservedMetrics`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SloEvaluation {
    pub passed: bool,
    pub violations: Vec<String>,
}

/// Operating modality of Riffty Infrastructure Intelligence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InfraModality {
    CoreOnly,
    Connected,
    Enterprise,
}

/// Point-in-time snapshot of a workload's infrastructure configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfraSnapshot {
    pub workload: WorkloadReference,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpu_request: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_gib_request: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_replicas: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_replicas: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_replicas: Option<u32>,
}

/// Aggregated autoscaling telemetry over a measurement window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoscalingSummary {
    pub samples: usize,
    pub duration_secs: f64,
    pub avg_replicas: f64,
    pub peak_replicas: u32,
    pub pod_minutes: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_cpu_millicores: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_memory_mib: Option<f64>,
}

/// Pod resource request specification used for replica-based cost attribution.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PodResourceSpec {
    pub vcpu_request: f64,
    pub mem_gib_request: f64,
}

/// Percentage deltas between baseline and optimized observed metrics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EfficiencyDeltas {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rps_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_per_req_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_minutes_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_pct: Option<f64>,
}

/// Input specification for comparing baseline vs. optimized workload efficiency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EfficiencyComparison {
    pub modality: InfraModality,
    pub active_providers: Vec<String>,
    pub baseline_metrics: ObservedMetrics,
    pub optimized_metrics: ObservedMetrics,
    pub pricing: PricingModel,
    #[serde(default)]
    pub slo_policy: SloPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_rps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_spec: Option<PodResourceSpec>,
}

/// Complete efficiency & cost attribution report generated by `EfficiencyAnalyzer`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EfficiencyReport {
    pub modality: InfraModality,
    pub active_providers: Vec<String>,
    pub baseline_metrics: ObservedMetrics,
    pub optimized_metrics: ObservedMetrics,
    pub deltas: EfficiencyDeltas,
    pub baseline_slo: SloEvaluation,
    pub optimized_slo: SloEvaluation,
    pub baseline_cost: CostAttribution,
    pub optimized_cost: CostAttribution,
    pub estimated_monthly_savings: f64,
    pub estimated_annual_savings: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub savings_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_hourly_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_hourly_cost: Option<f64>,
    pub assumptions: Vec<String>,
    pub limitations: Vec<String>,
}

impl EfficiencyReport {
    /// Serializes the efficiency report to pretty-printed JSON.
    pub fn to_json_pretty(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| format!("failed to serialize EfficiencyReport: {e}"))
    }
}

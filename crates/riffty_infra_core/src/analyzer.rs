use crate::model::{
    CostAttribution, CostKind, EfficiencyComparison, EfficiencyDeltas, EfficiencyReport,
    ObservedMetrics, PodResourceSpec, PricingModel,
};

const HOURS_PER_MONTH: f64 = 730.0;
const HOURS_PER_YEAR: f64 = 8760.0;

pub const CORE_ONLY_CAPACITY_LIMITATION: &str = "Core-only capacity equivalence: saved vCPUs only reduce cloud bills when HPA or right-sizing reduces provisioned pods/nodes.";

/// Pure domain analyzer comparing baseline vs. optimized workload efficiency and estimating cost impact.
pub struct EfficiencyAnalyzer;

impl EfficiencyAnalyzer {
    pub fn compare(input: &EfficiencyComparison) -> EfficiencyReport {
        let baseline = &input.baseline_metrics;
        let optimized = &input.optimized_metrics;

        let deltas = EfficiencyDeltas {
            rps_pct: pct_delta(baseline.rps, optimized.rps),
            p99_pct: pct_delta(baseline.p99_ms, optimized.p99_ms),
            cpu_per_req_pct: pct_delta(
                baseline.effective_cpu_us_per_req(),
                optimized.effective_cpu_us_per_req(),
            ),
            pod_minutes_pct: pct_delta(baseline.pod_minutes, optimized.pod_minutes),
            memory_pct: pct_delta(baseline.memory_mib_avg, optimized.memory_mib_avg),
        };

        let baseline_slo = input.slo_policy.evaluate(baseline);
        let optimized_slo = input.slo_policy.evaluate(optimized);

        let mut assumptions = Vec::new();
        let mut limitations = Vec::new();

        let pricing_source = format!(
            "{} ({}, {})",
            input.pricing.provider, input.pricing.region, input.pricing.billing_model
        );

        let has_hpa_data =
            baseline.avg_replicas.is_some() && optimized.avg_replicas.is_some();

        let (baseline_cost, optimized_cost) = if has_hpa_data {
            let pod_spec = input.pod_spec.unwrap_or_else(|| {
                assumptions.push(
                    "No pod resource spec provided; defaulted to 1.0 vCPU and 1.0 GiB per replica."
                        .to_string(),
                );
                PodResourceSpec {
                    vcpu_request: 1.0,
                    mem_gib_request: 1.0,
                }
            });
            assumptions.push(format!(
                "Observed K8s HPA replica mode: {:.3} vCPU + {:.3} GiB per pod at ${:.4}/vCPU-hr and ${:.4}/GiB-hr.",
                pod_spec.vcpu_request,
                pod_spec.mem_gib_request,
                input.pricing.vcpu_per_hour,
                input.pricing.memory_gib_per_hour
            ));
            assumptions.push(
                "Monthly = hourly × 730 hrs; annual = hourly × 8760 hrs (steady-state extrapolation)."
                    .to_string(),
            );
            limitations.push(
                "Estimated cost based on normalized reference pricing and observed replica counts, not actual cloud billing invoices."
                    .to_string(),
            );

            let base_replicas = baseline.avg_replicas.unwrap_or(0.0);
            let opt_replicas = optimized.avg_replicas.unwrap_or(0.0);

            (
                hpa_cost_attribution(
                    base_replicas,
                    pod_spec,
                    &input.pricing,
                    &pricing_source,
                ),
                hpa_cost_attribution(
                    opt_replicas,
                    pod_spec,
                    &input.pricing,
                    &pricing_source,
                ),
            )
        } else {
            let target_rps = input
                .target_rps
                .or(baseline.rps)
                .or(optimized.rps)
                .unwrap_or(1000.0);
            let target_util = if input.pricing.target_utilization > 0.0 {
                input.pricing.target_utilization
            } else {
                0.70
            };

            assumptions.push(format!(
                "CPU capacity-equivalence mode at target throughput {:.2} RPS and {:.0}% target CPU utilization.",
                target_rps,
                target_util * 100.0
            ));
            assumptions.push(format!(
                "Compute priced at ${:.4}/vCPU-hr ({}); monthly = hourly × 730 hrs, annual = hourly × 8760 hrs.",
                input.pricing.vcpu_per_hour, pricing_source
            ));
            limitations.push(CORE_ONLY_CAPACITY_LIMITATION.to_string());

            let base_cost = capacity_equivalence_cost(
                baseline,
                target_rps,
                target_util,
                &input.pricing,
                &pricing_source,
                &mut limitations,
                "baseline",
            );
            let opt_cost = capacity_equivalence_cost(
                optimized,
                target_rps,
                target_util,
                &input.pricing,
                &pricing_source,
                &mut limitations,
                "optimized",
            );
            (base_cost, opt_cost)
        };

        let estimated_monthly_savings = baseline_cost.monthly_cost - optimized_cost.monthly_cost;
        let estimated_annual_savings = baseline_cost.annual_cost - optimized_cost.annual_cost;
        let savings_pct = if baseline_cost.monthly_cost > 0.0 {
            Some((estimated_monthly_savings / baseline_cost.monthly_cost) * 100.0)
        } else {
            None
        };

        EfficiencyReport {
            modality: input.modality,
            active_providers: input.active_providers.clone(),
            baseline_metrics: baseline.clone(),
            optimized_metrics: optimized.clone(),
            deltas,
            baseline_slo,
            optimized_slo,
            baseline_cost,
            optimized_cost,
            estimated_monthly_savings,
            estimated_annual_savings,
            savings_pct,
            idle_hourly_cost: None,
            shared_hourly_cost: None,
            assumptions,
            limitations,
        }
    }
}

fn hpa_cost_attribution(
    avg_replicas: f64,
    pod_spec: PodResourceSpec,
    pricing: &PricingModel,
    pricing_source: &str,
) -> CostAttribution {
    let per_pod_hourly = pod_spec.vcpu_request * pricing.vcpu_per_hour
        + pod_spec.mem_gib_request * pricing.memory_gib_per_hour;
    let hourly = (avg_replicas.max(0.0)) * per_pod_hourly;
    CostAttribution {
        kind: CostKind::Estimated,
        currency: pricing.currency.clone(),
        hourly_cost: hourly,
        monthly_cost: hourly * HOURS_PER_MONTH,
        annual_cost: hourly * HOURS_PER_YEAR,
        basis: format!(
            "Observed HPA replicas ({avg_replicas:.2} avg pods × ({:.2} vCPU + {:.2} GiB))",
            pod_spec.vcpu_request, pod_spec.mem_gib_request
        ),
        pricing_source: pricing_source.to_string(),
    }
}

fn capacity_equivalence_cost(
    metrics: &ObservedMetrics,
    target_rps: f64,
    target_utilization: f64,
    pricing: &PricingModel,
    pricing_source: &str,
    limitations: &mut Vec<String>,
    label: &str,
) -> CostAttribution {
    let cpu_us = match metrics.effective_cpu_us_per_req() {
        Some(v) if v >= 0.0 => v,
        _ => {
            limitations.push(format!(
                "Missing cpu_us_per_req and cpu_millicores_avg on {label} metrics; modeled vCPU cost set to 0.0."
            ));
            0.0
        }
    };

    let vcpus = ((target_rps * cpu_us) / 1_000_000.0) / target_utilization;
    let hourly = vcpus * pricing.vcpu_per_hour;

    CostAttribution {
        kind: CostKind::Estimated,
        currency: pricing.currency.clone(),
        hourly_cost: hourly,
        monthly_cost: hourly * HOURS_PER_MONTH,
        annual_cost: hourly * HOURS_PER_YEAR,
        basis: format!(
            "CPU capacity equivalence ({vcpus:.3} vCPUs = ({target_rps:.1} RPS × {cpu_us:.2} µs/req / 1e6) / {target_utilization:.2})"
        ),
        pricing_source: pricing_source.to_string(),
    }
}

fn pct_delta(baseline: Option<f64>, optimized: Option<f64>) -> Option<f64> {
    match (baseline, optimized) {
        (Some(b), Some(o)) if b.abs() > f64::EPSILON => Some(((o - b) / b) * 100.0),
        _ => None,
    }
}

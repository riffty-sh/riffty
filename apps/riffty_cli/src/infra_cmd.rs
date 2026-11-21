use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use riffty_adapter_infra::{
    KubernetesHpaAdapter, LocalBenchmarkMetricsAdapter, OpenCostCostAdapter,
    PrometheusMetricsAdapter, PublicPricingAdapter, default_infra_registry,
};
use riffty_infra_core::{
    AutoscalingProviderPort, CORE_ONLY_CAPACITY_LIMITATION, CostProviderPort, EfficiencyAnalyzer,
    EfficiencyComparison, EfficiencyReport, InfraModality, MeasurementWindow, MetricsProviderPort,
    ObservedMetrics, PodResourceSpec, PricingProviderPort, SloPolicy, WorkloadReference,
};

#[derive(Debug, Args)]
pub struct EfficiencyArgs {
    #[command(subcommand)]
    pub command: EfficiencySubcommand,
}

#[derive(Debug, Subcommand)]
pub enum EfficiencySubcommand {
    /// Compare baseline vs. optimized workload telemetry and project estimated infrastructure savings
    Analyze(EfficiencyAnalyzeArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EfficiencyFormat {
    Text,
    Json,
    Markdown,
}

#[derive(Debug, Args)]
pub struct EfficiencyAnalyzeArgs {
    /// Path to baseline metrics JSON/JSONL (or combined before/after comparison file)
    #[arg(long)]
    pub baseline: Option<PathBuf>,

    /// Path to optimized metrics JSON/JSONL (defaults to --baseline for combined comparison files)
    #[arg(long)]
    pub optimized: Option<PathBuf>,

    /// Optional Kubernetes HPA JSONL sample log for baseline run
    #[arg(long)]
    pub hpa_baseline: Option<PathBuf>,

    /// Optional Kubernetes HPA JSONL sample log for optimized run
    #[arg(long)]
    pub hpa_optimized: Option<PathBuf>,

    /// Optional Prometheus base URL for live CPU/memory metrics enrichment
    #[arg(long, env = "PROMETHEUS_URL")]
    pub prometheus_url: Option<String>,

    /// Optional OpenCost base URL for live namespace cost attribution
    #[arg(long, env = "OPENCOST_URL")]
    pub opencost_url: Option<String>,

    /// Kubernetes namespace for the baseline workload (used with --prometheus-url / --opencost-url)
    #[arg(long)]
    pub namespace_baseline: Option<String>,

    /// Kubernetes namespace for the optimized workload (used with --prometheus-url / --opencost-url)
    #[arg(long)]
    pub namespace_optimized: Option<String>,

    /// Measurement window in minutes for Prometheus/OpenCost queries
    #[arg(long, default_value_t = 15)]
    pub window_mins: u64,

    /// Normalized cloud pricing profile (`aws-us-east-1`, `gcp-us-central1`, `azure-eastus`)
    #[arg(long, default_value = "aws-us-east-1")]
    pub pricing: String,

    /// Optional custom PricingModel JSON file overriding `--pricing`
    #[arg(long)]
    pub pricing_file: Option<PathBuf>,

    /// Target throughput (RPS) for Core-only CPU capacity-equivalence estimation
    #[arg(long)]
    pub rps: Option<f64>,

    /// Pod vCPU request per replica (used when HPA replica data is present)
    #[arg(long)]
    pub pod_vcpu: Option<f64>,

    /// Pod memory GiB request per replica (used when HPA replica data is present)
    #[arg(long)]
    pub pod_mem_gib: Option<f64>,

    /// Optional SLO ceiling for p99 latency in milliseconds
    #[arg(long)]
    pub max_p99_ms: Option<f64>,

    /// Optional SLO ceiling for error rate percentage
    #[arg(long)]
    pub max_error_pct: Option<f64>,

    /// Output format (`text`, `json`, or `markdown`)
    #[arg(long, value_enum, default_value_t = EfficiencyFormat::Text)]
    pub format: EfficiencyFormat,
}

#[derive(Debug, Args)]
pub struct InfraArgs {
    #[command(subcommand)]
    pub command: InfraSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum InfraSubcommand {
    /// List registered infrastructure intelligence provider plugins and their runtime health
    Providers(InfraProvidersArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InfraProvidersFormat {
    Text,
    Json,
}

#[derive(Debug, Args)]
pub struct InfraProvidersArgs {
    /// Output format (`text` or `json`)
    #[arg(long, value_enum, default_value_t = InfraProvidersFormat::Text)]
    pub format: InfraProvidersFormat,
}

pub fn run_efficiency(args: EfficiencyArgs) -> Result<(), String> {
    match args.command {
        EfficiencySubcommand::Analyze(a) => run_efficiency_analyze(a),
    }
}

pub fn run_infra(args: InfraArgs) -> Result<(), String> {
    match args.command {
        InfraSubcommand::Providers(a) => run_infra_providers(a),
    }
}

fn run_efficiency_analyze(args: EfficiencyAnalyzeArgs) -> Result<(), String> {
    let prom_url = args
        .prometheus_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let opencost_url = args
        .opencost_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let namespaces = match (&args.namespace_baseline, &args.namespace_optimized) {
        (Some(base_ns), Some(opt_ns)) => Some((base_ns.as_str(), opt_ns.as_str())),
        (None, None) => None,
        _ => {
            return Err(
                "both --namespace-baseline and --namespace-optimized must be provided together"
                    .to_string(),
            );
        }
    };

    let has_live_source = namespaces.is_some() && (prom_url.is_some() || opencost_url.is_some());
    if args.baseline.is_none() && !has_live_source {
        return Err(
            "provide --baseline <path>, or --namespace-baseline + --namespace-optimized with --opencost-url and/or --prometheus-url"
                .to_string(),
        );
    }

    let is_k8s =
        args.hpa_baseline.is_some() || args.hpa_optimized.is_some() || namespaces.is_some();
    let platform = if is_k8s {
        "kubernetes"
    } else {
        "local-benchmark"
    };
    let workload_name = args
        .baseline
        .as_ref()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .or_else(|| namespaces.map(|(b, _)| b))
        .unwrap_or("workload");

    let mut base_workload = WorkloadReference::new(workload_name, platform);
    let mut opt_workload = WorkloadReference::new(workload_name, platform);
    if let Some((base_ns, opt_ns)) = namespaces {
        base_workload = base_workload.with_namespace(base_ns);
        opt_workload = opt_workload.with_namespace(opt_ns);
    }

    let window = MeasurementWindow {
        duration_secs: (args.window_mins.max(1) as f64) * 60.0,
        samples: 1,
    };

    let mut active_providers: Vec<String> = Vec::new();
    let mut modality = InfraModality::CoreOnly;

    let (mut baseline_metrics, mut optimized_metrics) = if let Some(base_path) = &args.baseline {
        let opt_path = args.optimized.as_ref().unwrap_or(base_path);
        let base_adapter = LocalBenchmarkMetricsAdapter::new(base_path, Some("baseline".into()));
        let opt_adapter = LocalBenchmarkMetricsAdapter::new(opt_path, Some("fixed".into()));

        active_providers.push("local-jfr-metrics".to_string());
        (
            base_adapter.collect(&base_workload, &window)?,
            opt_adapter.collect(&opt_workload, &window)?,
        )
    } else {
        (ObservedMetrics::default(), ObservedMetrics::default())
    };

    if let (Some(hpa_base_path), Some(hpa_opt_path)) = (&args.hpa_baseline, &args.hpa_optimized) {
        let base_hpa = KubernetesHpaAdapter::from_log_file(hpa_base_path)
            .with_pod_requests(args.pod_vcpu, args.pod_mem_gib);
        let opt_hpa = KubernetesHpaAdapter::from_log_file(hpa_opt_path)
            .with_pod_requests(args.pod_vcpu, args.pod_mem_gib);

        let base_scaling = base_hpa.collect_scaling(&base_workload, &window)?;
        let opt_scaling = opt_hpa.collect_scaling(&opt_workload, &window)?;

        baseline_metrics.merge_autoscaling(&base_scaling);
        optimized_metrics.merge_autoscaling(&opt_scaling);
        active_providers.push("kubernetes-hpa".to_string());
        modality = InfraModality::Connected;
    } else if args.hpa_baseline.is_some() || args.hpa_optimized.is_some() {
        return Err(
            "both --hpa-baseline and --hpa-optimized must be provided together".to_string(),
        );
    }

    if let (Some(url), Some(_)) = (prom_url, namespaces) {
        let prom_adapter = PrometheusMetricsAdapter::new(url);
        let prom_base = prom_adapter.collect(&base_workload, &window)?;
        let prom_opt = prom_adapter.collect(&opt_workload, &window)?;

        if let Some(cpu) = prom_base.cpu_millicores_avg {
            baseline_metrics.cpu_millicores_avg = Some(cpu);
        }
        if let Some(mem) = prom_base.memory_mib_avg {
            baseline_metrics.memory_mib_avg = Some(mem);
        }
        if let Some(cpu) = prom_opt.cpu_millicores_avg {
            optimized_metrics.cpu_millicores_avg = Some(cpu);
        }
        if let Some(mem) = prom_opt.memory_mib_avg {
            optimized_metrics.memory_mib_avg = Some(mem);
        }

        active_providers.push("prometheus-metrics".to_string());
        modality = InfraModality::Connected;
    }

    let use_opencost = opencost_url.is_some() && namespaces.is_some();
    if !use_opencost {
        active_providers.push("public-pricing".to_string());
    }

    let pricing_adapter = match &args.pricing_file {
        Some(path) => PublicPricingAdapter::with_custom_file(path),
        None => PublicPricingAdapter::new(),
    };
    let pricing = pricing_adapter.resolve_pricing(&args.pricing)?;

    let pod_spec = match (args.pod_vcpu, args.pod_mem_gib) {
        (Some(vcpu), Some(mem)) => Some(PodResourceSpec {
            vcpu_request: vcpu,
            mem_gib_request: mem,
        }),
        (Some(vcpu), None) => Some(PodResourceSpec {
            vcpu_request: vcpu,
            mem_gib_request: 1.0,
        }),
        (None, Some(mem)) => Some(PodResourceSpec {
            vcpu_request: 1.0,
            mem_gib_request: mem,
        }),
        (None, None) => None,
    };

    let slo_policy = SloPolicy {
        max_p99_ms: args.max_p99_ms,
        max_error_rate_pct: args.max_error_pct,
        min_rps: None,
    };

    let comparison = EfficiencyComparison {
        modality,
        active_providers,
        baseline_metrics,
        optimized_metrics,
        pricing,
        slo_policy,
        target_rps: args.rps,
        pod_spec,
    };

    let mut report = EfficiencyAnalyzer::compare(&comparison);

    if let (Some(url), Some((base_ns, opt_ns))) = (opencost_url, namespaces) {
        let opencost_adapter = OpenCostCostAdapter::new(url);
        let base_cost = opencost_adapter.attribute_cost(&base_workload, &window)?;
        let opt_cost = opencost_adapter.attribute_cost(&opt_workload, &window)?;

        let monthly_savings = base_cost.monthly_cost - opt_cost.monthly_cost;
        let annual_savings = base_cost.annual_cost - opt_cost.annual_cost;
        let savings_pct = if base_cost.monthly_cost > 0.0 {
            Some((monthly_savings / base_cost.monthly_cost) * 100.0)
        } else {
            None
        };

        report.baseline_cost = base_cost;
        report.optimized_cost = opt_cost;
        report.estimated_monthly_savings = monthly_savings;
        report.estimated_annual_savings = annual_savings;
        report.savings_pct = savings_pct;
        report.modality = InfraModality::Connected;
        if !report
            .active_providers
            .iter()
            .any(|p| p == "opencost-billing")
        {
            report.active_providers.push("opencost-billing".to_string());
        }
        report.assumptions.clear();
        report.assumptions.push(format!(
            "Live OpenCost /allocation/compute attribution for namespaces '{base_ns}' vs '{opt_ns}' over {}m window.",
            args.window_mins.max(5)
        ));
        report.assumptions.push(
            "Monthly = hourly × 730 hrs; annual = hourly × 8760 hrs (normalized from OpenCost window minutes)."
                .to_string(),
        );
        report
            .limitations
            .retain(|l| l != CORE_ONLY_CAPACITY_LIMITATION && !l.contains("modeled vCPU cost"));
    }

    match args.format {
        EfficiencyFormat::Json => {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| format!("failed to serialize efficiency report: {e}"))?;
            println!("{json}");
        }
        EfficiencyFormat::Markdown => {
            println!("{}", render_efficiency_markdown(&report));
        }
        EfficiencyFormat::Text => {
            println!("{}", render_efficiency_text(&report));
        }
    }

    Ok(())
}

fn run_infra_providers(args: InfraProvidersArgs) -> Result<(), String> {
    let registry = default_infra_registry();
    let summary = registry.health_summary();

    match args.format {
        InfraProvidersFormat::Json => {
            let json = serde_json::to_string_pretty(&summary)
                .map_err(|e| format!("failed to serialize infra providers: {e}"))?;
            println!("{json}");
        }
        InfraProvidersFormat::Text => {
            println!(
                "{:<22} {:<8} {:<6} {:<26} {:<12} {}",
                "PROVIDER ID", "VERSION", "API", "CAPABILITIES", "HEALTH", "DETAILS"
            );
            println!("{}", "-".repeat(104));
            for p in &summary {
                let caps = p
                    .capabilities
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                let detail = p.health.detail().unwrap_or("-");
                println!(
                    "{:<22} {:<8} {:<6} {:<26} {:<12} {}",
                    p.id,
                    p.version,
                    p.api_compat,
                    caps,
                    p.health.label(),
                    detail
                );
            }
        }
    }

    Ok(())
}

fn fmt_opt(val: Option<f64>, unit: &str) -> String {
    match val {
        Some(v) => format!("{v:.2}{unit}"),
        None => "-".to_string(),
    }
}

fn fmt_delta(val: Option<f64>) -> String {
    match val {
        Some(v) => format!("{v:+.2}%"),
        None => "-".to_string(),
    }
}

fn render_efficiency_text(r: &EfficiencyReport) -> String {
    let mut out = String::new();
    out.push_str("=== Riffty Efficiency & Infrastructure Intelligence Report ===\n");
    out.push_str(&format!(
        "Modality: {:?} | Active Providers: {}\n\n",
        r.modality,
        r.active_providers.join(", ")
    ));
    out.push_str("Observed Metrics (Baseline vs. Optimized):\n");
    out.push_str(&format!(
        "  RPS:            {:>12} -> {:>12} ({})\n",
        fmt_opt(r.baseline_metrics.rps, ""),
        fmt_opt(r.optimized_metrics.rps, ""),
        fmt_delta(r.deltas.rps_pct)
    ));
    out.push_str(&format!(
        "  p99 Latency:    {:>12} -> {:>12} ({})\n",
        fmt_opt(r.baseline_metrics.p99_ms, "ms"),
        fmt_opt(r.optimized_metrics.p99_ms, "ms"),
        fmt_delta(r.deltas.p99_pct)
    ));
    out.push_str(&format!(
        "  CPU / Request:  {:>12} -> {:>12} ({})\n",
        fmt_opt(r.baseline_metrics.effective_cpu_us_per_req(), "us"),
        fmt_opt(r.optimized_metrics.effective_cpu_us_per_req(), "us"),
        fmt_delta(r.deltas.cpu_per_req_pct)
    ));
    out.push_str(&format!(
        "  Memory (avg):   {:>12} -> {:>12} ({})\n",
        fmt_opt(r.baseline_metrics.memory_mib_avg, "MiB"),
        fmt_opt(r.optimized_metrics.memory_mib_avg, "MiB"),
        fmt_delta(r.deltas.memory_pct)
    ));
    if r.baseline_metrics.avg_replicas.is_some() || r.optimized_metrics.avg_replicas.is_some() {
        out.push_str(&format!(
            "  Avg Replicas:   {:>12} -> {:>12}\n",
            fmt_opt(r.baseline_metrics.avg_replicas, ""),
            fmt_opt(r.optimized_metrics.avg_replicas, "")
        ));
        out.push_str(&format!(
            "  Pod-Minutes:    {:>12} -> {:>12} ({})\n",
            fmt_opt(r.baseline_metrics.pod_minutes, ""),
            fmt_opt(r.optimized_metrics.pod_minutes, ""),
            fmt_delta(r.deltas.pod_minutes_pct)
        ));
    }

    out.push_str("\nSLO Evaluation:\n");
    out.push_str(&format!(
        "  Baseline:  {}\n",
        if r.baseline_slo.passed {
            "PASS".to_string()
        } else {
            format!("FAIL ({})", r.baseline_slo.violations.join("; "))
        }
    ));
    out.push_str(&format!(
        "  Optimized: {}\n",
        if r.optimized_slo.passed {
            "PASS".to_string()
        } else {
            format!("FAIL ({})", r.optimized_slo.violations.join("; "))
        }
    ));

    out.push_str(&format!(
        "\nCost Attribution (Kind: {:?} | Pricing: {}):\n",
        r.baseline_cost.kind, r.baseline_cost.pricing_source
    ));
    out.push_str(&format!(
        "  Baseline:  ${:.4}/hr | ${:.2}/mo | ${:.2}/yr\n",
        r.baseline_cost.hourly_cost, r.baseline_cost.monthly_cost, r.baseline_cost.annual_cost
    ));
    out.push_str(&format!(
        "  Optimized: ${:.4}/hr | ${:.2}/mo | ${:.2}/yr\n",
        r.optimized_cost.hourly_cost, r.optimized_cost.monthly_cost, r.optimized_cost.annual_cost
    ));
    out.push_str(&format!(
        "  Estimated Savings: ${:.2}/mo | ${:.2}/yr ({})\n",
        r.estimated_monthly_savings,
        r.estimated_annual_savings,
        fmt_delta(r.savings_pct)
    ));

    if !r.assumptions.is_empty() {
        out.push_str("\nAssumptions:\n");
        for a in &r.assumptions {
            out.push_str(&format!("  - {a}\n"));
        }
    }
    if !r.limitations.is_empty() {
        out.push_str("\nLimitations:\n");
        for l in &r.limitations {
            out.push_str(&format!("  - {l}\n"));
        }
    }
    out
}

fn render_efficiency_markdown(r: &EfficiencyReport) -> String {
    let mut out = String::new();
    out.push_str("# Riffty Efficiency & Infrastructure Intelligence Report\n\n");
    out.push_str(&format!(
        "- **Modality:** `{:?}`\n- **Active Providers:** `{}`\n- **Cost Provenance:** `{:?}` ({})\n\n",
        r.modality,
        r.active_providers.join(", "),
        r.baseline_cost.kind,
        r.baseline_cost.pricing_source
    ));
    out.push_str("## Observed Metrics\n\n");
    out.push_str("| Metric | Baseline | Optimized | Delta |\n");
    out.push_str("| :--- | ---: | ---: | ---: |\n");
    out.push_str(&format!(
        "| Throughput (RPS) | {} | {} | {} |\n",
        fmt_opt(r.baseline_metrics.rps, ""),
        fmt_opt(r.optimized_metrics.rps, ""),
        fmt_delta(r.deltas.rps_pct)
    ));
    out.push_str(&format!(
        "| p99 Latency | {} | {} | {} |\n",
        fmt_opt(r.baseline_metrics.p99_ms, " ms"),
        fmt_opt(r.optimized_metrics.p99_ms, " ms"),
        fmt_delta(r.deltas.p99_pct)
    ));
    out.push_str(&format!(
        "| CPU / Request | {} | {} | {} |\n",
        fmt_opt(r.baseline_metrics.effective_cpu_us_per_req(), " µs"),
        fmt_opt(r.optimized_metrics.effective_cpu_us_per_req(), " µs"),
        fmt_delta(r.deltas.cpu_per_req_pct)
    ));
    out.push_str(&format!(
        "| Memory (avg) | {} | {} | {} |\n",
        fmt_opt(r.baseline_metrics.memory_mib_avg, " MiB"),
        fmt_opt(r.optimized_metrics.memory_mib_avg, " MiB"),
        fmt_delta(r.deltas.memory_pct)
    ));
    if r.baseline_metrics.avg_replicas.is_some() || r.optimized_metrics.avg_replicas.is_some() {
        out.push_str(&format!(
            "| Pod-Minutes | {} | {} | {} |\n",
            fmt_opt(r.baseline_metrics.pod_minutes, ""),
            fmt_opt(r.optimized_metrics.pod_minutes, ""),
            fmt_delta(r.deltas.pod_minutes_pct)
        ));
    }

    out.push_str("\n## Estimated Cost Attribution\n\n");
    out.push_str("| Side | Hourly | Monthly (730h) | Annual (8760h) | Basis |\n");
    out.push_str("| :--- | ---: | ---: | ---: | :--- |\n");
    out.push_str(&format!(
        "| Baseline | ${:.4} | ${:.2} | ${:.2} | {} |\n",
        r.baseline_cost.hourly_cost,
        r.baseline_cost.monthly_cost,
        r.baseline_cost.annual_cost,
        r.baseline_cost.basis
    ));
    out.push_str(&format!(
        "| Optimized | ${:.4} | ${:.2} | ${:.2} | {} |\n",
        r.optimized_cost.hourly_cost,
        r.optimized_cost.monthly_cost,
        r.optimized_cost.annual_cost,
        r.optimized_cost.basis
    ));
    out.push_str(&format!(
        "\n**Estimated Savings:** `${:.2}/mo` (`${:.2}/yr`, {})\n",
        r.estimated_monthly_savings,
        r.estimated_annual_savings,
        fmt_delta(r.savings_pct)
    ));

    if !r.limitations.is_empty() {
        out.push_str("\n## Limitations\n\n");
        for l in &r.limitations {
            out.push_str(&format!("- {l}\n"));
        }
    }
    out
}

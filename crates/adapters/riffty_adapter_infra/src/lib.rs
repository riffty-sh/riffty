pub mod k8s_hpa;
pub mod local_metrics;
pub mod opencost;
pub mod pricing;
pub mod prometheus;

pub use k8s_hpa::{
    KubernetesHpaAdapter, current_kubectl_context, parse_hpa_jsonl, parse_hpa_jsonl_str,
    probe_kubectl_health,
};
pub use local_metrics::{LocalBenchmarkMetricsAdapter, parse_metrics_file, parse_metrics_str};
pub use opencost::{
    OpenCostCostAdapter, OpenCostDetailedAllocation, parse_opencost_allocation,
    parse_opencost_detailed,
};
pub use pricing::PublicPricingAdapter;
pub use prometheus::{PrometheusMetricsAdapter, parse_prometheus_scalar};

use riffty_infra_core::{
    InfraCapability, InfraPluginDescriptor, InfraPluginHealth, InfraPluginRegistry,
};

/// Builds the default infrastructure intelligence plugin registry with built-in and optional providers.
pub fn default_infra_registry() -> InfraPluginRegistry {
    let mut registry = InfraPluginRegistry::new();
    let version = env!("CARGO_PKG_VERSION").to_string();

    let _ = registry.register(
        InfraPluginDescriptor {
            id: "local-jfr-metrics".to_string(),
            version: version.clone(),
            api_compat: "1.x".to_string(),
            capabilities: vec![InfraCapability::Metrics],
            requires_external_infra: false,
            description: "Local HTTP/JFR benchmark JSON & JSONL metrics parser".to_string(),
        },
        InfraPluginHealth::Healthy,
    );

    let _ = registry.register(
        InfraPluginDescriptor {
            id: "kubernetes-hpa".to_string(),
            version: version.clone(),
            api_compat: "1.x".to_string(),
            capabilities: vec![
                InfraCapability::Infrastructure,
                InfraCapability::Autoscaling,
            ],
            requires_external_infra: true,
            description: "Kubernetes HPA JSONL telemetry & kubectl cluster adapter".to_string(),
        },
        probe_kubectl_health(),
    );

    let _ = registry.register(
        InfraPluginDescriptor {
            id: "public-pricing".to_string(),
            version: version.clone(),
            api_compat: "1.x".to_string(),
            capabilities: vec![InfraCapability::Pricing],
            requires_external_infra: false,
            description: "Normalized public cloud compute reference pricing (Estimated)".to_string(),
        },
        InfraPluginHealth::Healthy,
    );

    let prom_health = match std::env::var("PROMETHEUS_URL") {
        Ok(url) if !url.trim().is_empty() => PrometheusMetricsAdapter::new(url).health_check(),
        _ => InfraPluginHealth::Unavailable("PROMETHEUS_URL not configured".to_string()),
    };
    let _ = registry.register(
        InfraPluginDescriptor {
            id: "prometheus-metrics".to_string(),
            version: version.clone(),
            api_compat: "1.x".to_string(),
            capabilities: vec![InfraCapability::Metrics],
            requires_external_infra: true,
            description: "Optional Prometheus PromQL metrics provider".to_string(),
        },
        prom_health,
    );

    let opencost_health = match std::env::var("OPENCOST_URL") {
        Ok(url) if !url.trim().is_empty() => OpenCostCostAdapter::new(url).health_check(),
        _ => InfraPluginHealth::Unavailable("OPENCOST_URL not configured".to_string()),
    };
    let _ = registry.register(
        InfraPluginDescriptor {
            id: "opencost-billing".to_string(),
            version,
            api_compat: "1.x".to_string(),
            capabilities: vec![InfraCapability::Cost],
            requires_external_infra: true,
            description: "Optional OpenCost cluster cost allocation provider".to_string(),
        },
        opencost_health,
    );

    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use riffty_infra_core::{CostKind, PricingProviderPort};

    #[test]
    fn parses_track_b_before_after_http_json() {
        let sample = r#"{
          "benchmark": "track-b1-google-genai",
          "baseline": {
            "http": { "rps": 24907.71, "p50_ms": 0.558, "p95_ms": 1.123, "p99_ms": 1.994, "total_ops": 199336, "errors": 0 },
            "process": { "cpu_pct_avg": 447.75, "rss_mb_avg": 379.1 }
          },
          "fixed": {
            "http": { "rps": 27089.69, "p50_ms": 0.518, "p95_ms": 0.999, "p99_ms": 1.727, "total_ops": 216745, "errors": 0 },
            "process": { "cpu_pct_avg": 415.4, "rss_mb_avg": 384.45 }
          }
        }"#;

        let base = parse_metrics_str(sample, Some("baseline")).unwrap();
        let opt = parse_metrics_str(sample, Some("fixed")).unwrap();

        assert!((base.rps.unwrap() - 24907.71).abs() < 1e-6);
        assert!((base.cpu_millicores_avg.unwrap() - 4477.5).abs() < 1e-6);
        assert!((base.memory_mib_avg.unwrap() - 379.1).abs() < 1e-6);
        assert!(base.cpu_us_per_req.unwrap() > opt.cpu_us_per_req.unwrap());
        assert!((opt.p99_ms.unwrap() - 1.727).abs() < 1e-6);
    }

    #[test]
    fn parses_quarkus_comparison_summary_json() {
        let sample = r#"{
          "payloads": [
            {
              "payload_kb": 1,
              "baseline": { "requests_total": 50000, "errors": 0, "rps": 6000.0, "latency_ms": { "p50": 1.5, "p95": 5.0, "p99": 16.0 } },
              "fixed": { "requests_total": 75000, "errors": 0, "rps": 9000.0, "latency_ms": { "p50": 0.9, "p95": 3.5, "p99": 12.0 } }
            },
            {
              "payload_kb": 10,
              "baseline": { "requests_total": 30000, "errors": 10, "rps": 4000.0, "latency_ms": { "p50": 2.0, "p95": 11.0, "p99": 40.0 } },
              "fixed": { "requests_total": 85000, "errors": 0, "rps": 11000.0, "latency_ms": { "p50": 1.1, "p95": 2.5, "p99": 8.0 } }
            }
          ]
        }"#;

        let base = parse_metrics_str(sample, Some("baseline")).unwrap();
        let opt = parse_metrics_str(sample, Some("optimized")).unwrap();

        assert!((base.rps.unwrap() - 5000.0).abs() < 1e-6);
        assert_eq!(base.requests_total, Some(80000));
        assert_eq!(base.errors_total, Some(10));
        assert!((opt.rps.unwrap() - 10000.0).abs() < 1e-6);
        assert!((opt.p99_ms.unwrap() - 10.0).abs() < 1e-6);
    }

    #[test]
    fn parses_workload_jsonl_stream() {
        let sample = concat!(
            r#"{"scenario":"objmap_fanout","mode":"baseline","wall_sec":8.0,"total_ops":100000,"rps":12500.0,"process_cpu_sec":40.0,"cpu_us_per_req":400.0,"gc_time_ms":500,"p50_ms":0.5,"p95_ms":1.2,"p99_ms":3.0}"#,
            "\n",
            r#"{"scenario":"objmap_fanout","mode":"fixed","wall_sec":8.0,"total_ops":400000,"rps":50000.0,"process_cpu_sec":40.0,"cpu_us_per_req":100.0,"gc_time_ms":200,"p50_ms":0.1,"p95_ms":0.3,"p99_ms":0.8}"#,
            "\n"
        );

        let base = parse_metrics_str(sample, Some("baseline")).unwrap();
        let opt = parse_metrics_str(sample, Some("fixed")).unwrap();

        assert!((base.cpu_us_per_req.unwrap() - 400.0).abs() < 1e-6);
        assert!((opt.cpu_us_per_req.unwrap() - 100.0).abs() < 1e-6);
        assert!((base.gc_pause_ms_per_1k_req.unwrap() - 5.0).abs() < 1e-6);
        assert!((opt.gc_pause_ms_per_1k_req.unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn parses_hpa_jsonl_samples() {
        let hpa_log = concat!(
            r#"{"t_sec":0,"current_replicas":2,"pods":[{"cpu":"450m","memory":"384Mi"},{"cpu":"550m","memory":"384Mi"}]}"#,
            "\n",
            r#"{"t_sec":5,"current_replicas":4,"cpu_millicores":2000,"memory_mib":1536}"#,
            "\n",
            r#"{"t_sec":10,"current_replicas":6,"cpu_millicores":3000,"memory_mib":2304}"#,
            "\n"
        );

        let summary = parse_hpa_jsonl_str(hpa_log, 5.0).unwrap();
        assert_eq!(summary.samples, 3);
        assert!((summary.duration_secs - 15.0).abs() < 1e-6);
        assert!((summary.avg_replicas - 4.0).abs() < 1e-6);
        assert_eq!(summary.peak_replicas, 6);
        assert!((summary.pod_minutes - 1.0).abs() < 1e-6);
        assert!((summary.avg_cpu_millicores.unwrap() - 2000.0).abs() < 1e-6);
        assert!((summary.avg_memory_mib.unwrap() - 1536.0).abs() < 1e-6);
    }

    #[test]
    fn public_pricing_adapter_resolves_profiles_and_tags_estimated() {
        let adapter = PublicPricingAdapter::new();
        let aws = adapter.resolve_pricing("aws-us-east-1").unwrap();
        let gcp = adapter.resolve_pricing("gcp-us-central1").unwrap();
        let azure = adapter.resolve_pricing("azure-eastus").unwrap();

        assert_eq!(aws.provider, "aws");
        assert_eq!(gcp.provider, "gcp");
        assert_eq!(azure.provider, "azure");
        assert!(adapter.resolve_pricing("nonexistent-cloud").is_err());

        let attr = PublicPricingAdapter::estimated_attribution(&aws, 1.0, "test");
        assert_eq!(attr.kind, CostKind::Estimated);
        assert!((attr.monthly_cost - 730.0).abs() < 1e-6);
    }

    #[test]
    fn default_registry_registers_core_and_optional_providers() {
        let registry = default_infra_registry();
        let summary = registry.health_summary();
        assert_eq!(summary.len(), 5);
        assert!(summary.iter().any(|p| p.id == "local-jfr-metrics"));
        assert!(summary.iter().any(|p| p.id == "kubernetes-hpa"));
        assert!(summary.iter().any(|p| p.id == "public-pricing"));
        assert!(summary.iter().any(|p| p.id == "prometheus-metrics"));
        assert!(summary.iter().any(|p| p.id == "opencost-billing"));
    }

    #[test]
    fn parses_prometheus_vector_scalar_empty_and_error_responses() {
        let vector_payload = r#"{
          "status": "success",
          "data": {
            "resultType": "vector",
            "result": [
              {
                "metric": {},
                "value": [1732182000.123, "1842.65"]
              }
            ]
          }
        }"#;
        let val = parse_prometheus_scalar(vector_payload).unwrap().unwrap();
        assert!((val - 1842.65).abs() < 1e-6);

        let empty_payload = r#"{
          "status": "success",
          "data": {
            "resultType": "vector",
            "result": []
          }
        }"#;
        assert_eq!(parse_prometheus_scalar(empty_payload).unwrap(), None);

        let nan_payload = r#"{
          "status": "success",
          "data": {
            "resultType": "vector",
            "result": [{"metric": {}, "value": [1732182000, "NaN"]}]
          }
        }"#;
        assert_eq!(parse_prometheus_scalar(nan_payload).unwrap(), None);

        let error_payload = r#"{
          "status": "error",
          "errorType": "bad_data",
          "error": "parse error: unexpected character"
        }"#;
        assert!(parse_prometheus_scalar(error_payload).is_err());
    }

    #[test]
    fn parses_opencost_allocation_compute_response_and_normalizes_cost() {
        let payload = r#"{
          "code": 200,
          "status": "success",
          "data": [
            {
              "riffty-baseline": {
                "name": "riffty-baseline",
                "properties": { "cluster": "kind-riffty", "namespace": "riffty-baseline" },
                "window": { "start": "2026-11-21T09:00:00Z", "end": "2026-11-21T09:15:00Z" },
                "minutes": 15.0,
                "cpuCores": 2.5,
                "cpuCost": 0.0198,
                "ramBytes": 2147483648.0,
                "ramCost": 0.0022,
                "totalCost": 0.022
              },
              "riffty-optimized": {
                "name": "riffty-optimized",
                "minutes": 15.0,
                "cpuCores": 1.0,
                "cpuCost": 0.00792,
                "ramBytes": 1073741824.0,
                "ramCost": 0.0011,
                "totalCost": 0.00902
              }
            }
          ]
        }"#;

        let base = parse_opencost_allocation(payload, "riffty-baseline").unwrap();
        let opt = parse_opencost_allocation(payload, "riffty-optimized").unwrap();

        assert_eq!(base.kind, CostKind::Estimated);
        assert_eq!(base.currency, "USD");
        assert_eq!(base.pricing_source, "opencost (/allocation/compute)");
        // 0.022 over 15 minutes => 0.088/hr => 64.24/mo => 770.88/yr
        assert!((base.hourly_cost - 0.088).abs() < 1e-9);
        assert!((base.monthly_cost - (0.088 * 730.0)).abs() < 1e-6);
        assert!((base.annual_cost - (0.088 * 8760.0)).abs() < 1e-6);
        assert!(base.basis.contains("cpuCores=2.500"));
        assert!(base.basis.contains("ramGiB=2.000"));
        assert!(base.basis.contains("minutes=15.0"));

        assert!(opt.hourly_cost < base.hourly_cost);
        assert!(parse_opencost_allocation(payload, "nonexistent-ns").is_err());
    }

    #[test]
    fn parses_opencost_detailed_with_idle_and_shared_costs() {
        let payload = r#"{
          "code": 200,
          "status": "success",
          "data": [
            {
              "__idle__": {
                "name": "__idle__",
                "minutes": 15.0,
                "totalCost": 0.055
              },
              "riffty-baseline": {
                "name": "riffty-baseline",
                "minutes": 15.0,
                "cpuCores": 2.0,
                "cpuCoreUsageAverage": 1.65,
                "cpuCoreRequestAverage": 2.0,
                "ramBytes": 2147483648.0,
                "ramByteUsageAverage": 536870912.0,
                "ramByteRequestAverage": 1073741824.0,
                "sharedCost": 0.005,
                "totalCost": 0.025
              }
            }
          ]
        }"#;

        let detailed = parse_opencost_detailed(payload, "riffty-baseline").unwrap();
        // 0.025 over 15 min => 0.10/hr
        assert!((detailed.allocated.hourly_cost - 0.10).abs() < 1e-9);
        // __idle__: 0.055 over 15 min => 0.22/hr
        assert!((detailed.idle_hourly_cost - 0.22).abs() < 1e-9);
        // sharedCost: 0.005 over 15 min => 0.02/hr
        assert!((detailed.shared_hourly_cost - 0.02).abs() < 1e-9);
        assert!((detailed.cpu_core_usage_avg - 1.65).abs() < 1e-9);
        assert!((detailed.cpu_core_request_avg - 2.0).abs() < 1e-9);
        assert!((detailed.ram_mib_usage_avg - 512.0).abs() < 1e-6);
        assert!((detailed.ram_mib_request_avg - 1024.0).abs() < 1e-6);
        assert!((detailed.minutes - 15.0).abs() < 1e-9);
    }
}

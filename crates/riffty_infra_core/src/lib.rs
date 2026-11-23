pub mod analyzer;
pub mod model;
pub mod plugin;
pub mod ports;

pub use analyzer::{CORE_ONLY_CAPACITY_LIMITATION, EfficiencyAnalyzer};
pub use model::{
    AutoscalingSummary, CostAttribution, CostKind, EfficiencyComparison, EfficiencyDeltas,
    EfficiencyReport, InfraModality, InfraSnapshot, MeasurementWindow, ObservedMetrics,
    PodResourceSpec, PricingModel, SloEvaluation, SloPolicy, WorkloadReference,
};
pub use plugin::{
    InfraCapability, InfraPluginDescriptor, InfraPluginHealth, InfraPluginRegistry,
    PluginHealthSummary, SUPPORTED_PLUGIN_API_MAJOR,
};
pub use ports::{
    AutoscalingProviderPort, CostProviderPort, InfrastructureProviderPort, MetricsProviderPort,
    PricingProviderPort,
};

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeMetricsAdapter {
        metrics: ObservedMetrics,
    }

    impl MetricsProviderPort for FakeMetricsAdapter {
        fn collect(
            &self,
            _workload: &WorkloadReference,
            _window: &MeasurementWindow,
        ) -> Result<ObservedMetrics, String> {
            Ok(self.metrics.clone())
        }
    }

    struct FakeAutoscalingAdapter {
        summary: AutoscalingSummary,
    }

    impl AutoscalingProviderPort for FakeAutoscalingAdapter {
        fn collect_scaling(
            &self,
            _workload: &WorkloadReference,
            _window: &MeasurementWindow,
        ) -> Result<AutoscalingSummary, String> {
            Ok(self.summary.clone())
        }
    }

    struct FakePricingAdapter {
        model: PricingModel,
    }

    impl PricingProviderPort for FakePricingAdapter {
        fn resolve_pricing(&self, _profile: &str) -> Result<PricingModel, String> {
            Ok(self.model.clone())
        }
    }

    struct FakeActualCostAdapter {
        cost: CostAttribution,
    }

    impl CostProviderPort for FakeActualCostAdapter {
        fn attribute_cost(
            &self,
            _workload: &WorkloadReference,
            _window: &MeasurementWindow,
        ) -> Result<CostAttribution, String> {
            Ok(self.cost.clone())
        }
    }

    fn sample_aws_pricing() -> PricingModel {
        PricingModel {
            provider: "aws".to_string(),
            region: "us-east-1".to_string(),
            currency: "USD".to_string(),
            vcpu_per_hour: 0.04048,
            memory_gib_per_hour: 0.004445,
            billing_model: "on-demand-reference".to_string(),
            target_utilization: 0.70,
        }
    }

    #[test]
    fn core_only_capacity_equivalence_mode_computes_deltas_and_limitations() {
        let workload = WorkloadReference::new("genai-gateway", "local-jvm");
        let window = MeasurementWindow {
            duration_secs: 60.0,
            samples: 12,
        };

        let base_adapter = FakeMetricsAdapter {
            metrics: ObservedMetrics {
                rps: Some(1000.0),
                p99_ms: Some(40.0),
                cpu_us_per_req: Some(1000.0),
                memory_mib_avg: Some(512.0),
                error_rate_pct: Some(0.0),
                ..Default::default()
            },
        };
        let opt_adapter = FakeMetricsAdapter {
            metrics: ObservedMetrics {
                rps: Some(2000.0),
                p99_ms: Some(10.0),
                cpu_us_per_req: Some(250.0),
                memory_mib_avg: Some(384.0),
                error_rate_pct: Some(0.0),
                ..Default::default()
            },
        };
        let pricing_adapter = FakePricingAdapter {
            model: sample_aws_pricing(),
        };

        let comparison = EfficiencyComparison {
            modality: InfraModality::CoreOnly,
            active_providers: vec!["fake-metrics".into(), "fake-pricing".into()],
            baseline_metrics: base_adapter.collect(&workload, &window).unwrap(),
            optimized_metrics: opt_adapter.collect(&workload, &window).unwrap(),
            pricing: pricing_adapter.resolve_pricing("aws-us-east-1").unwrap(),
            slo_policy: SloPolicy {
                max_p99_ms: Some(50.0),
                max_error_rate_pct: Some(1.0),
                min_rps: Some(500.0),
            },
            target_rps: Some(5000.0),
            pod_spec: None,
        };

        let report = EfficiencyAnalyzer::compare(&comparison);

        assert_eq!(report.modality, InfraModality::CoreOnly);
        assert!((report.deltas.rps_pct.unwrap() - 100.0).abs() < 1e-6);
        assert!((report.deltas.p99_pct.unwrap() - (-75.0)).abs() < 1e-6);
        assert!((report.deltas.cpu_per_req_pct.unwrap() - (-75.0)).abs() < 1e-6);
        assert!((report.deltas.memory_pct.unwrap() - (-25.0)).abs() < 1e-6);
        assert!(report.baseline_slo.passed);
        assert!(report.optimized_slo.passed);
        assert_eq!(report.baseline_cost.kind, CostKind::Estimated);
        assert_eq!(report.optimized_cost.kind, CostKind::Estimated);
        assert!((report.savings_pct.unwrap() - 75.0).abs() < 1e-6);
        assert!(
            report
                .limitations
                .iter()
                .any(|l| l == CORE_ONLY_CAPACITY_LIMITATION)
        );
    }

    #[test]
    fn k8s_hpa_replica_mode_computes_provisioned_replica_savings() {
        let workload =
            WorkloadReference::new("genai-gateway", "kubernetes").with_namespace("riffty-track-b1");
        let window = MeasurementWindow {
            duration_secs: 60.0,
            samples: 12,
        };

        let mut baseline = ObservedMetrics {
            rps: Some(24907.71),
            p99_ms: Some(1.994),
            ..Default::default()
        };
        let mut optimized = ObservedMetrics {
            rps: Some(27089.69),
            p99_ms: Some(1.727),
            ..Default::default()
        };

        let base_hpa = FakeAutoscalingAdapter {
            summary: AutoscalingSummary {
                samples: 12,
                duration_secs: 60.0,
                avg_replicas: 6.0,
                peak_replicas: 8,
                pod_minutes: 6.0,
                avg_cpu_millicores: Some(2400.0),
                avg_memory_mib: Some(2200.0),
            },
        };
        let opt_hpa = FakeAutoscalingAdapter {
            summary: AutoscalingSummary {
                samples: 12,
                duration_secs: 60.0,
                avg_replicas: 3.0,
                peak_replicas: 4,
                pod_minutes: 3.0,
                avg_cpu_millicores: Some(1100.0),
                avg_memory_mib: Some(1100.0),
            },
        };

        baseline.merge_autoscaling(&base_hpa.collect_scaling(&workload, &window).unwrap());
        optimized.merge_autoscaling(&opt_hpa.collect_scaling(&workload, &window).unwrap());

        let pricing = sample_aws_pricing();
        let pod_spec = PodResourceSpec {
            vcpu_request: 1.0,
            mem_gib_request: 1.0,
        };

        let report = EfficiencyAnalyzer::compare(&EfficiencyComparison {
            modality: InfraModality::Connected,
            active_providers: vec![
                "local-jfr-metrics".into(),
                "kubernetes-hpa".into(),
                "public-pricing".into(),
            ],
            baseline_metrics: baseline,
            optimized_metrics: optimized,
            pricing: pricing.clone(),
            slo_policy: SloPolicy::default(),
            target_rps: None,
            pod_spec: Some(pod_spec),
        });

        let expected_base_hourly = 6.0 * (pricing.vcpu_per_hour + pricing.memory_gib_per_hour);
        let expected_opt_hourly = 3.0 * (pricing.vcpu_per_hour + pricing.memory_gib_per_hour);

        assert!((report.baseline_cost.hourly_cost - expected_base_hourly).abs() < 1e-9);
        assert!((report.optimized_cost.hourly_cost - expected_opt_hourly).abs() < 1e-9);
        assert!((report.baseline_cost.monthly_cost - expected_base_hourly * 730.0).abs() < 1e-6);
        assert!((report.baseline_cost.annual_cost - expected_base_hourly * 8760.0).abs() < 1e-6);
        assert!((report.savings_pct.unwrap() - 50.0).abs() < 1e-6);
        assert!((report.deltas.pod_minutes_pct.unwrap() - (-50.0)).abs() < 1e-6);
        assert_eq!(report.baseline_cost.kind, CostKind::Estimated);
        assert_eq!(report.optimized_cost.kind, CostKind::Estimated);
        assert!(
            !report
                .limitations
                .iter()
                .any(|l| l == CORE_ONLY_CAPACITY_LIMITATION)
        );
    }

    #[test]
    fn slo_policy_detects_latency_error_and_throughput_violations() {
        let policy = SloPolicy {
            max_p99_ms: Some(15.0),
            max_error_rate_pct: Some(0.5),
            min_rps: Some(5000.0),
        };
        let failing_metrics = ObservedMetrics {
            rps: Some(3200.0),
            p99_ms: Some(25.4),
            requests_total: Some(1000),
            errors_total: Some(12),
            ..Default::default()
        };

        let eval = policy.evaluate(&failing_metrics);
        assert!(!eval.passed);
        assert_eq!(eval.violations.len(), 3);
        assert!(eval.violations[0].contains("p99 latency"));
        assert!(eval.violations[1].contains("error rate"));
        assert!(eval.violations[2].contains("throughput"));
    }

    #[test]
    fn plugin_registry_validates_compat_and_swaps_active_provider() {
        let mut registry = InfraPluginRegistry::new();

        // Incompatible API major version is rejected
        let bad_plugin = InfraPluginDescriptor {
            id: "legacy-v0".into(),
            version: "0.9.0".into(),
            api_compat: "0.x".into(),
            capabilities: vec![InfraCapability::Metrics],
            requires_external_infra: false,
            description: "Old plugin".into(),
        };
        assert!(
            registry
                .register(bad_plugin, InfraPluginHealth::Healthy)
                .is_err()
        );

        // Register two compatible metrics providers + one unavailable provider
        registry
            .register(
                InfraPluginDescriptor {
                    id: "local-jfr-metrics".into(),
                    version: "0.2.0".into(),
                    api_compat: "1.x".into(),
                    capabilities: vec![InfraCapability::Metrics],
                    requires_external_infra: false,
                    description: "Local benchmark/JFR metrics".into(),
                },
                InfraPluginHealth::Healthy,
            )
            .unwrap();

        registry
            .register(
                InfraPluginDescriptor {
                    id: "prometheus-metrics".into(),
                    version: "0.2.0".into(),
                    api_compat: "1.0".into(),
                    capabilities: vec![InfraCapability::Metrics],
                    requires_external_infra: true,
                    description: "Prometheus PromQL provider".into(),
                },
                InfraPluginHealth::Healthy,
            )
            .unwrap();

        registry
            .register(
                InfraPluginDescriptor {
                    id: "opencost-billing".into(),
                    version: "0.2.0".into(),
                    api_compat: "1.x".into(),
                    capabilities: vec![InfraCapability::Cost],
                    requires_external_infra: true,
                    description: "OpenCost actual billing".into(),
                },
                InfraPluginHealth::Unavailable("OPENCOST_URL not configured".into()),
            )
            .unwrap();

        assert_eq!(registry.list().len(), 3);
        assert_eq!(
            registry
                .resolve_for_capability(InfraCapability::Metrics)
                .unwrap()
                .id,
            "local-jfr-metrics"
        );

        // Swap active metrics provider to prometheus
        registry
            .swap_active(InfraCapability::Metrics, "prometheus-metrics")
            .unwrap();
        assert_eq!(
            registry
                .resolve_for_capability(InfraCapability::Metrics)
                .unwrap()
                .id,
            "prometheus-metrics"
        );

        // Cannot swap to unavailable provider
        assert!(
            registry
                .swap_active(InfraCapability::Cost, "opencost-billing")
                .is_err()
        );

        let summary = registry.health_summary();
        assert_eq!(summary.len(), 3);
        assert_eq!(summary[2].health.label(), "unavailable");
    }

    #[test]
    fn strict_separation_between_estimated_and_actual_billed_cost() {
        let workload = WorkloadReference::new("checkout-api", "kubernetes");
        let window = MeasurementWindow {
            duration_secs: 3600.0,
            samples: 60,
        };

        let billed_adapter = FakeActualCostAdapter {
            cost: CostAttribution {
                kind: CostKind::ActualBilled,
                currency: "USD".into(),
                hourly_cost: 1.42,
                monthly_cost: 1036.60,
                annual_cost: 12439.20,
                basis: "OpenCost cluster allocation invoice".into(),
                pricing_source: "opencost-billing".into(),
            },
        };

        let billed = billed_adapter.attribute_cost(&workload, &window).unwrap();
        assert_eq!(billed.kind, CostKind::ActualBilled);

        let report = EfficiencyAnalyzer::compare(&EfficiencyComparison {
            modality: InfraModality::CoreOnly,
            active_providers: vec!["local-jfr-metrics".into(), "public-pricing".into()],
            baseline_metrics: ObservedMetrics {
                rps: Some(1000.0),
                cpu_us_per_req: Some(400.0),
                ..Default::default()
            },
            optimized_metrics: ObservedMetrics {
                rps: Some(1000.0),
                cpu_us_per_req: Some(200.0),
                ..Default::default()
            },
            pricing: sample_aws_pricing(),
            slo_policy: SloPolicy::default(),
            target_rps: Some(1000.0),
            pod_spec: None,
        });

        assert_eq!(report.baseline_cost.kind, CostKind::Estimated);
        assert_eq!(report.optimized_cost.kind, CostKind::Estimated);
        assert_ne!(report.baseline_cost.kind, billed.kind);
    }
}

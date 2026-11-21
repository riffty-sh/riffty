use crate::model::{
    AutoscalingSummary, CostAttribution, InfraSnapshot, MeasurementWindow, ObservedMetrics,
    PricingModel, WorkloadReference,
};

/// Outbound port for collecting observed workload performance and resource metrics.
pub trait MetricsProviderPort: Send + Sync {
    fn collect(
        &self,
        workload: &WorkloadReference,
        window: &MeasurementWindow,
    ) -> Result<ObservedMetrics, String>;
}

/// Outbound port for inspecting workload infrastructure metadata (pod requests, cluster context).
pub trait InfrastructureProviderPort: Send + Sync {
    fn inspect_workload(&self, workload: &WorkloadReference) -> Result<InfraSnapshot, String>;
}

/// Outbound port for collecting horizontal autoscaling telemetry (e.g. Kubernetes HPA).
pub trait AutoscalingProviderPort: Send + Sync {
    fn collect_scaling(
        &self,
        workload: &WorkloadReference,
        window: &MeasurementWindow,
    ) -> Result<AutoscalingSummary, String>;
}

/// Outbound port for resolving cloud compute pricing profiles.
pub trait PricingProviderPort: Send + Sync {
    fn resolve_pricing(&self, profile: &str) -> Result<PricingModel, String>;
}

/// Outbound port for attributing actual or modeled infrastructure cost.
pub trait CostProviderPort: Send + Sync {
    fn attribute_cost(
        &self,
        workload: &WorkloadReference,
        window: &MeasurementWindow,
    ) -> Result<CostAttribution, String>;
}

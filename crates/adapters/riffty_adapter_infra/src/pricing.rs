use std::fs;
use std::path::{Path, PathBuf};

use riffty_infra_core::{CostAttribution, CostKind, PricingModel, PricingProviderPort};

/// Public reference pricing adapter with normalized cloud compute profiles and custom JSON override.
/// Always produces `CostKind::Estimated` attributions (never `ActualBilled`).
#[derive(Debug, Clone, Default)]
pub struct PublicPricingAdapter {
    pub custom_pricing_file: Option<PathBuf>,
}

impl PublicPricingAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_custom_file(path: impl Into<PathBuf>) -> Self {
        Self {
            custom_pricing_file: Some(path.into()),
        }
    }

    /// Loads a custom `PricingModel` from a JSON file.
    pub fn load_pricing_file(path: &Path) -> Result<PricingModel, String> {
        let raw = fs::read_to_string(path)
            .map_err(|e| format!("failed to read pricing file '{}': {e}", path.display()))?;
        serde_json::from_str::<PricingModel>(&raw)
            .map_err(|e| format!("invalid pricing JSON in '{}': {e}", path.display()))
    }

    /// Lists all built-in normalized reference pricing profile names.
    pub fn builtin_profiles() -> &'static [&'static str] {
        &["aws-us-east-1", "gcp-us-central1", "azure-eastus"]
    }

    /// Helper that builds an explicitly `CostKind::Estimated` attribution from hourly cost.
    pub fn estimated_attribution(
        pricing: &PricingModel,
        hourly_cost: f64,
        basis: impl Into<String>,
    ) -> CostAttribution {
        CostAttribution {
            kind: CostKind::Estimated,
            currency: pricing.currency.clone(),
            hourly_cost,
            monthly_cost: hourly_cost * 730.0,
            annual_cost: hourly_cost * 8760.0,
            basis: basis.into(),
            pricing_source: format!(
                "{} ({}, {})",
                pricing.provider, pricing.region, pricing.billing_model
            ),
        }
    }
}

impl PricingProviderPort for PublicPricingAdapter {
    fn resolve_pricing(&self, profile: &str) -> Result<PricingModel, String> {
        if let Some(ref file) = self.custom_pricing_file {
            return Self::load_pricing_file(file);
        }

        match profile.trim().to_ascii_lowercase().as_str() {
            "aws-us-east-1" | "aws" => Ok(PricingModel {
                provider: "aws".to_string(),
                region: "us-east-1".to_string(),
                currency: "USD".to_string(),
                vcpu_per_hour: 0.04048,
                memory_gib_per_hour: 0.004445,
                billing_model: "on-demand-reference".to_string(),
                target_utilization: 0.70,
            }),
            "gcp-us-central1" | "gcp" => Ok(PricingModel {
                provider: "gcp".to_string(),
                region: "us-central1".to_string(),
                currency: "USD".to_string(),
                vcpu_per_hour: 0.031611,
                memory_gib_per_hour: 0.004237,
                billing_model: "on-demand-reference".to_string(),
                target_utilization: 0.70,
            }),
            "azure-eastus" | "azure" => Ok(PricingModel {
                provider: "azure".to_string(),
                region: "eastus".to_string(),
                currency: "USD".to_string(),
                vcpu_per_hour: 0.04100,
                memory_gib_per_hour: 0.004500,
                billing_model: "on-demand-reference".to_string(),
                target_utilization: 0.70,
            }),
            other => Err(format!(
                "unknown pricing profile '{other}'; built-in profiles: {}",
                Self::builtin_profiles().join(", ")
            )),
        }
    }
}

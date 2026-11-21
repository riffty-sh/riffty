use std::process::Command;

use riffty_infra_core::{
    CostAttribution, CostKind, CostProviderPort, InfraPluginHealth, MeasurementWindow,
    WorkloadReference,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const HOURS_PER_MONTH: f64 = 730.0;
const HOURS_PER_YEAR: f64 = 8760.0;
const BYTES_PER_GIB: f64 = 1_073_741_824.0;
const BYTES_PER_MIB: f64 = 1_048_576.0;

/// Detailed OpenCost allocation separating workload cost from idle (`__idle__`) and shared costs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenCostDetailedAllocation {
    pub allocated: CostAttribution,
    pub idle_hourly_cost: f64,
    pub shared_hourly_cost: f64,
    pub cpu_core_usage_avg: f64,
    pub cpu_core_request_avg: f64,
    pub ram_mib_usage_avg: f64,
    pub ram_mib_request_avg: f64,
    pub minutes: f64,
}

/// Adapter for querying live OpenCost allocation compute costs (`/allocation/compute`).
#[derive(Debug, Clone)]
pub struct OpenCostCostAdapter {
    pub base_url: String,
}

impl OpenCostCostAdapter {
    pub fn new(base_url: impl Into<String>) -> Self {
        let trimmed = base_url.into().trim().trim_end_matches('/').to_string();
        Self { base_url: trimmed }
    }

    /// Probes OpenCost availability via `/healthz`.
    pub fn health_check(&self) -> InfraPluginHealth {
        if self.base_url.is_empty() {
            return InfraPluginHealth::Unavailable("OPENCOST_URL not configured".to_string());
        }

        let health_url = format!("{}/healthz", self.base_url);
        match Command::new("curl")
            .args(["-sf", "--max-time", "2", &health_url])
            .output()
        {
            Ok(out) if out.status.success() => InfraPluginHealth::Healthy,
            Ok(_) => InfraPluginHealth::Degraded(format!(
                "OpenCost endpoint '{}' unreachable or returned non-200",
                self.base_url
            )),
            Err(e) => InfraPluginHealth::Degraded(format!(
                "failed to invoke curl for OpenCost health check: {e}"
            )),
        }
    }
    /// Queries live OpenCost `/allocation/compute` and returns detailed namespace attribution
    /// separated from cluster idle (`__idle__`) and shared (`__shared__` / `sharedCost`) costs.
    pub fn attribute_detailed(
        &self,
        workload: &WorkloadReference,
        window: &MeasurementWindow,
    ) -> Result<OpenCostDetailedAllocation, String> {
        let ns = workload.namespace.as_deref().unwrap_or(&workload.name);
        let mins = ((window.duration_secs / 60.0).round() as u64).max(5);
        let window_arg = format!("window={mins}m");
        let alloc_url = format!("{}/allocation/compute", self.base_url);

        let out = Command::new("curl")
            .args([
                "-sfG",
                "--max-time",
                "10",
                "--data-urlencode",
                &window_arg,
                "--data-urlencode",
                "resolution=1m",
                "--data-urlencode",
                "aggregate=namespace",
                "--data-urlencode",
                "includeIdle=true",
                &alloc_url,
            ])
            .output()
            .map_err(|e| format!("failed to execute curl against OpenCost '{alloc_url}': {e}"))?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(format!(
                "OpenCost allocation query failed ({alloc_url}): {}",
                if stderr.is_empty() {
                    format!("exit status {}", out.status)
                } else {
                    stderr
                }
            ));
        }

        let body = String::from_utf8(out.stdout)
            .map_err(|e| format!("invalid UTF-8 response from OpenCost: {e}"))?;
        parse_opencost_detailed(&body, ns)
    }
}

impl CostProviderPort for OpenCostCostAdapter {
    fn attribute_cost(
        &self,
        workload: &WorkloadReference,
        window: &MeasurementWindow,
    ) -> Result<CostAttribution, String> {
        self.attribute_detailed(workload, window)
            .map(|d| d.allocated)
    }
}

/// Parses an OpenCost `/allocation/compute?aggregate=namespace` JSON response for a namespace.
pub fn parse_opencost_allocation(json: &str, namespace: &str) -> Result<CostAttribution, String> {
    parse_opencost_detailed(json, namespace).map(|d| d.allocated)
}

/// Parses an OpenCost `/allocation/compute?aggregate=namespace` JSON response into a detailed
/// allocation that separates workload cost from cluster idle (`__idle__`) and shared costs.
pub fn parse_opencost_detailed(
    json: &str,
    namespace: &str,
) -> Result<OpenCostDetailedAllocation, String> {
    let root: Value =
        serde_json::from_str(json).map_err(|e| format!("invalid OpenCost JSON response: {e}"))?;

    if let Some(code) = root.get("code").and_then(Value::as_u64) {
        if code != 200 {
            let msg = root
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("non-200 status code");
            return Err(format!("OpenCost returned HTTP code {code}: {msg}"));
        }
    }

    let Some(data) = root.get("data").and_then(Value::as_array) else {
        return Err("OpenCost response missing 'data' array".to_string());
    };

    let mut found_window: Option<&serde_json::Map<String, Value>> = None;
    let mut found_alloc: Option<&Value> = None;

    for window_entry in data {
        let Some(map) = window_entry.as_object() else {
            continue;
        };
        if let Some(entry) = map.get(namespace) {
            found_window = Some(map);
            found_alloc = Some(entry);
            break;
        }
        if let Some(entry) = map
            .values()
            .find(|v| v.get("name").and_then(Value::as_str) == Some(namespace))
        {
            found_window = Some(map);
            found_alloc = Some(entry);
            break;
        }
    }

    let (Some(window_map), Some(alloc)) = (found_window, found_alloc) else {
        return Err(format!(
            "namespace '{namespace}' not found in OpenCost /allocation/compute response"
        ));
    };

    let total_cost = alloc
        .get("totalCost")
        .and_then(Value::as_f64)
        .ok_or_else(|| {
            format!("OpenCost allocation for '{namespace}' missing numeric 'totalCost'")
        })?;
    let minutes = alloc.get("minutes").and_then(Value::as_f64).unwrap_or(1.0);
    let cpu_cores = alloc.get("cpuCores").and_then(Value::as_f64).unwrap_or(0.0);
    let ram_bytes = alloc.get("ramBytes").and_then(Value::as_f64).unwrap_or(0.0);
    let ram_gib = ram_bytes / BYTES_PER_GIB;

    let hourly_cost = total_cost * (60.0 / minutes.max(1.0));
    let monthly_cost = hourly_cost * HOURS_PER_MONTH;
    let annual_cost = hourly_cost * HOURS_PER_YEAR;

    let allocated = CostAttribution {
        kind: CostKind::Estimated,
        currency: "USD".to_string(),
        hourly_cost,
        monthly_cost,
        annual_cost,
        basis: format!(
            "OpenCost namespace '{namespace}' (cpuCores={cpu_cores:.3}, ramGiB={ram_gib:.3}, minutes={minutes:.1})"
        ),
        pricing_source: "opencost (/allocation/compute)".to_string(),
    };

    let idle_hourly_cost = window_map
        .get("__idle__")
        .and_then(|idle| {
            let idle_total = idle.get("totalCost").and_then(Value::as_f64)?;
            let idle_mins = idle
                .get("minutes")
                .and_then(Value::as_f64)
                .unwrap_or(minutes);
            Some(idle_total * (60.0 / idle_mins.max(1.0)))
        })
        .unwrap_or(0.0);

    let shared_cost_from_entry = alloc
        .get("sharedCost")
        .and_then(Value::as_f64)
        .map(|c| c * (60.0 / minutes.max(1.0)))
        .unwrap_or(0.0);
    let shared_cost_from_bucket = window_map
        .get("__shared__")
        .and_then(|shared| {
            let shared_total = shared.get("totalCost").and_then(Value::as_f64)?;
            let shared_mins = shared
                .get("minutes")
                .and_then(Value::as_f64)
                .unwrap_or(minutes);
            Some(shared_total * (60.0 / shared_mins.max(1.0)))
        })
        .unwrap_or(0.0);
    let shared_hourly_cost = if shared_cost_from_entry > 0.0 {
        shared_cost_from_entry
    } else {
        shared_cost_from_bucket
    };

    let cpu_core_usage_avg = alloc
        .get("cpuCoreUsageAverage")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let cpu_core_request_avg = alloc
        .get("cpuCoreRequestAverage")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let ram_mib_usage_avg = alloc
        .get("ramByteUsageAverage")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        / BYTES_PER_MIB;
    let ram_mib_request_avg = alloc
        .get("ramByteRequestAverage")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        / BYTES_PER_MIB;

    Ok(OpenCostDetailedAllocation {
        allocated,
        idle_hourly_cost,
        shared_hourly_cost,
        cpu_core_usage_avg,
        cpu_core_request_avg,
        ram_mib_usage_avg,
        ram_mib_request_avg,
        minutes,
    })
}

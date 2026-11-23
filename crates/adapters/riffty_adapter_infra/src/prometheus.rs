use std::time::Duration;

use riffty_infra_core::{
    InfraPluginHealth, MeasurementWindow, MetricsProviderPort, ObservedMetrics, WorkloadReference,
};
use serde_json::Value;

/// Adapter for querying live Prometheus PromQL HTTP API (`/api/v1/query`).
#[derive(Debug, Clone)]
pub struct PrometheusMetricsAdapter {
    pub base_url: String,
    pub bearer_token: Option<String>,
}

impl PrometheusMetricsAdapter {
    pub fn new(base_url: impl Into<String>) -> Self {
        let trimmed = base_url.into().trim().trim_end_matches('/').to_string();
        let bearer_token = std::env::var("PROMETHEUS_BEARER_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Self {
            base_url: trimmed,
            bearer_token,
        }
    }

    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        let trimmed = token.into().trim().to_string();
        self.bearer_token = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        };
        self
    }

    fn http_agent(timeout_secs: u64) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(timeout_secs)))
            .build()
            .into()
    }

    /// Probes Prometheus readiness via `/-/ready` (falling back to `/api/v1/status/runtimeinfo`).
    pub fn health_check(&self) -> InfraPluginHealth {
        if self.base_url.is_empty() {
            return InfraPluginHealth::Unavailable("PROMETHEUS_URL not configured".to_string());
        }

        let agent = Self::http_agent(2);
        let ready_url = format!("{}/-/ready", self.base_url);
        let mut req = agent.get(&ready_url);
        if let Some(ref tok) = self.bearer_token {
            req = req.header("Authorization", &format!("Bearer {tok}"));
        }
        if req.call().is_ok() {
            return InfraPluginHealth::Healthy;
        }

        let runtime_url = format!("{}/api/v1/status/runtimeinfo", self.base_url);
        let mut req = agent.get(&runtime_url);
        if let Some(ref tok) = self.bearer_token {
            req = req.header("Authorization", &format!("Bearer {tok}"));
        }
        match req.call() {
            Ok(_) => InfraPluginHealth::Healthy,
            Err(e) => InfraPluginHealth::Degraded(format!(
                "Prometheus endpoint '{}' unreachable or returned error: {e}",
                self.base_url
            )),
        }
    }

    fn query_scalar(&self, promql: &str) -> Result<Option<f64>, String> {
        let query_url = format!("{}/api/v1/query", self.base_url);
        let agent = Self::http_agent(5);
        let mut req = agent.get(&query_url).query("query", promql);
        if let Some(ref tok) = self.bearer_token {
            req = req.header("Authorization", &format!("Bearer {tok}"));
        }

        let mut resp = req
            .call()
            .map_err(|e| format!("Prometheus query failed ({query_url}): {e}"))?;

        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("invalid UTF-8 response from Prometheus: {e}"))?;
        parse_prometheus_scalar(&body)
    }

    /// Queries authoritative cumulative CPU core-seconds consumed by all containers in a namespace
    /// over `window_secs`, integrating across pod lifecycles and counter resets.
    pub fn collect_cumulative_cpu_core_seconds(
        &self,
        namespace: &str,
        window_secs: u64,
    ) -> Result<Option<f64>, String> {
        let secs = window_secs.max(1);
        let query = format!(
            "sum(increase(container_cpu_usage_seconds_total{{namespace=\"{namespace}\",container!=\"\"}}[{secs}s]))"
        );
        self.query_scalar(&query)
    }
}

impl MetricsProviderPort for PrometheusMetricsAdapter {
    fn collect(
        &self,
        workload: &WorkloadReference,
        window: &MeasurementWindow,
    ) -> Result<ObservedMetrics, String> {
        let ns = workload.namespace.as_deref().unwrap_or(&workload.name);
        let window_secs = (window.duration_secs.round() as u64).max(1);

        let cumulative_cpu_query = format!(
            "sum(increase(container_cpu_usage_seconds_total{{namespace=\"{ns}\",container!=\"\"}}[{window_secs}s])) / {window_secs} * 1000"
        );
        let mut cpu_millicores_avg = self.query_scalar(&cumulative_cpu_query)?;
        if cpu_millicores_avg.is_none() {
            let rate_cpu_query = format!(
                "sum(rate(container_cpu_usage_seconds_total{{namespace=\"{ns}\",container!=\"\"}}[2m])) * 1000"
            );
            cpu_millicores_avg = self.query_scalar(&rate_cpu_query)?;
        }
        if cpu_millicores_avg.is_none() {
            cpu_millicores_avg =
                self.query_scalar("sum(rate(process_cpu_seconds_total[2m])) * 1000")?;
        }

        let mem_window_query = format!(
            "avg_over_time(sum(container_memory_working_set_bytes{{namespace=\"{ns}\",container!=\"\"}})[{window_secs}s:15s]) / 1048576"
        );
        let mut memory_mib_avg = self.query_scalar(&mem_window_query)?;
        if memory_mib_avg.is_none() {
            let mem_instant_query = format!(
                "sum(container_memory_working_set_bytes{{namespace=\"{ns}\",container!=\"\"}}) / 1048576"
            );
            memory_mib_avg = self.query_scalar(&mem_instant_query)?;
        }

        Ok(ObservedMetrics {
            cpu_millicores_avg,
            memory_mib_avg,
            ..Default::default()
        })
    }
}

/// Parses a Prometheus `/api/v1/query` JSON response into a scalar `f64` value.
pub fn parse_prometheus_scalar(json: &str) -> Result<Option<f64>, String> {
    let root: Value =
        serde_json::from_str(json).map_err(|e| format!("invalid Prometheus JSON response: {e}"))?;

    let status = root.get("status").and_then(Value::as_str).unwrap_or("");
    if status != "success" {
        let err_msg = root
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown Prometheus error");
        return Err(format!(
            "Prometheus query returned status '{status}': {err_msg}"
        ));
    }

    let Some(data) = root.get("data") else {
        return Err("Prometheus response missing 'data' field".to_string());
    };

    let result_type = data.get("resultType").and_then(Value::as_str).unwrap_or("");
    let Some(result) = data.get("result").and_then(Value::as_array) else {
        return Err("Prometheus response missing 'data.result' array".to_string());
    };

    if result.is_empty() {
        return Ok(None);
    }

    let raw_val = if result_type == "scalar" {
        result.get(1)
    } else {
        result
            .first()
            .and_then(|series| series.get("value"))
            .and_then(Value::as_array)
            .and_then(|pair| pair.get(1))
    };

    let Some(val) = raw_val else {
        return Ok(None);
    };

    let parsed = if let Some(s) = val.as_str() {
        s.parse::<f64>()
            .map_err(|e| format!("invalid Prometheus numeric value '{s}': {e}"))?
    } else if let Some(n) = val.as_f64() {
        n
    } else {
        return Err(format!("unexpected Prometheus value token: {val}"));
    };

    if !parsed.is_finite() {
        return Ok(None);
    }

    Ok(Some(parsed))
}

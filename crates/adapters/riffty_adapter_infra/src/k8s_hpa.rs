use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use riffty_infra_core::{
    AutoscalingProviderPort, AutoscalingSummary, InfraPluginHealth, InfraSnapshot,
    InfrastructureProviderPort, MeasurementWindow, WorkloadReference,
};
use serde_json::Value;

/// Adapter for Kubernetes HPA telemetry (`hpa_*.jsonl` sample logs + live `kubectl` health check).
#[derive(Debug, Clone)]
pub struct KubernetesHpaAdapter {
    pub hpa_log_path: Option<PathBuf>,
    pub sample_interval_secs: f64,
    pub vcpu_request: Option<f64>,
    pub mem_gib_request: Option<f64>,
}

impl Default for KubernetesHpaAdapter {
    fn default() -> Self {
        Self {
            hpa_log_path: None,
            sample_interval_secs: 5.0,
            vcpu_request: None,
            mem_gib_request: None,
        }
    }
}

impl KubernetesHpaAdapter {
    pub fn from_log_file(path: impl Into<PathBuf>) -> Self {
        Self {
            hpa_log_path: Some(path.into()),
            ..Self::default()
        }
    }

    pub fn with_pod_requests(mut self, vcpu: Option<f64>, mem_gib: Option<f64>) -> Self {
        self.vcpu_request = vcpu;
        self.mem_gib_request = mem_gib;
        self
    }
}

impl InfrastructureProviderPort for KubernetesHpaAdapter {
    fn inspect_workload(&self, workload: &WorkloadReference) -> Result<InfraSnapshot, String> {
        let cluster = current_kubectl_context();
        Ok(InfraSnapshot {
            workload: workload.clone(),
            cluster,
            vcpu_request: self.vcpu_request,
            mem_gib_request: self.mem_gib_request,
            current_replicas: None,
            min_replicas: None,
            max_replicas: None,
        })
    }
}

impl AutoscalingProviderPort for KubernetesHpaAdapter {
    fn collect_scaling(
        &self,
        _workload: &WorkloadReference,
        _window: &MeasurementWindow,
    ) -> Result<AutoscalingSummary, String> {
        let Some(ref path) = self.hpa_log_path else {
            return Err(
                "no HPA telemetry log path configured (pass --hpa-baseline / --hpa-optimized)"
                    .to_string(),
            );
        };
        parse_hpa_jsonl(path, self.sample_interval_secs)
    }
}

/// Probes local `kubectl` CLI availability, current context, and cluster reachability.
pub fn probe_kubectl_health() -> InfraPluginHealth {
    let ctx_out = match Command::new("kubectl")
        .args(["config", "current-context"])
        .output()
    {
        Ok(out) => out,
        Err(_) => {
            return InfraPluginHealth::Unavailable(
                "kubectl binary not found in PATH (JSONL offline mode still supported)".to_string(),
            );
        }
    };

    if !ctx_out.status.success() {
        return InfraPluginHealth::Degraded(
            "kubectl found but no active kubeconfig context (JSONL offline mode supported)"
                .to_string(),
        );
    }

    let ctx = String::from_utf8_lossy(&ctx_out.stdout).trim().to_string();
    let info_out = Command::new("kubectl")
        .args(["cluster-info", "--request-timeout=2s"])
        .output();

    match info_out {
        Ok(out) if out.status.success() => InfraPluginHealth::Healthy,
        _ => InfraPluginHealth::Degraded(format!(
            "kubectl context '{ctx}' unreachable (JSONL offline mode supported)"
        )),
    }
}

pub fn current_kubectl_context() -> Option<String> {
    let out = Command::new("kubectl")
        .args(["config", "current-context"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let ctx = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if ctx.is_empty() { None } else { Some(ctx) }
}

/// Parses an `hpa_*.jsonl` telemetry log containing periodic `kubectl get hpa` + `kubectl top pods` samples.
pub fn parse_hpa_jsonl(
    path: &Path,
    default_interval_secs: f64,
) -> Result<AutoscalingSummary, String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("failed to read HPA JSONL '{}': {e}", path.display()))?;
    parse_hpa_jsonl_str(&raw, default_interval_secs)
        .map_err(|e| format!("failed to parse HPA JSONL '{}': {e}", path.display()))
}

pub fn parse_hpa_jsonl_str(
    raw: &str,
    default_interval_secs: f64,
) -> Result<AutoscalingSummary, String> {
    let mut replicas_samples: Vec<f64> = Vec::new();
    let mut intervals: Vec<f64> = Vec::new();
    let mut timestamps: Vec<f64> = Vec::new();
    let mut cpu_samples: Vec<f64> = Vec::new();
    let mut mem_samples: Vec<f64> = Vec::new();

    for (idx, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: Value = serde_json::from_str(line)
            .map_err(|e| format!("invalid JSON on line {}: {e}", idx + 1))?;
        let Some(obj) = val.as_object() else {
            return Err(format!("expected JSON object on line {}", idx + 1));
        };

        let replicas = extract_replicas(obj)
            .ok_or_else(|| format!("missing replica count on line {}", idx + 1))?;
        replicas_samples.push(replicas);

        if let Some(dt) = obj
            .get("interval_secs")
            .or_else(|| obj.get("dt_secs"))
            .and_then(Value::as_f64)
        {
            intervals.push(dt.max(0.0));
        }

        if let Some(ts) = obj
            .get("t_sec")
            .or_else(|| obj.get("elapsed_secs"))
            .or_else(|| obj.get("timestamp_epoch_s"))
            .and_then(Value::as_f64)
        {
            timestamps.push(ts);
        }

        if let Some(cpu_m) = extract_cpu_millicores(obj) {
            cpu_samples.push(cpu_m);
        }

        if let Some(mem_mib) = extract_memory_mib(obj) {
            mem_samples.push(mem_mib);
        }
    }

    if replicas_samples.is_empty() {
        return Err("HPA JSONL file contains no samples".to_string());
    }

    let n = replicas_samples.len();
    let sample_dts: Vec<f64> = if intervals.len() == n {
        intervals
    } else if timestamps.len() == n && n >= 2 {
        let mut dts = Vec::with_capacity(n);
        let fallback_dt = (timestamps[1] - timestamps[0]).max(default_interval_secs);
        for i in 0..n {
            if i + 1 < n {
                dts.push((timestamps[i + 1] - timestamps[i]).max(0.0));
            } else {
                dts.push(fallback_dt);
            }
        }
        dts
    } else {
        vec![default_interval_secs.max(1.0); n]
    };

    let duration_secs: f64 = sample_dts.iter().sum();
    let pod_seconds: f64 = replicas_samples
        .iter()
        .zip(sample_dts.iter())
        .map(|(r, dt)| r * dt)
        .sum();
    let pod_minutes = pod_seconds / 60.0;
    let avg_replicas = if duration_secs > 0.0 {
        pod_seconds / duration_secs
    } else {
        replicas_samples.iter().sum::<f64>() / n as f64
    };
    let peak_replicas = replicas_samples
        .iter()
        .copied()
        .fold(0.0_f64, f64::max)
        .round() as u32;

    let avg_cpu_millicores = if cpu_samples.is_empty() {
        None
    } else {
        Some(cpu_samples.iter().sum::<f64>() / cpu_samples.len() as f64)
    };
    let avg_memory_mib = if mem_samples.is_empty() {
        None
    } else {
        Some(mem_samples.iter().sum::<f64>() / mem_samples.len() as f64)
    };

    Ok(AutoscalingSummary {
        samples: n,
        duration_secs,
        avg_replicas,
        peak_replicas,
        pod_minutes,
        avg_cpu_millicores,
        avg_memory_mib,
    })
}

fn extract_replicas(obj: &serde_json::Map<String, Value>) -> Option<f64> {
    for key in [
        "current_replicas",
        "currentReplicas",
        "replicas",
        "ready_replicas",
        "desired_replicas",
    ] {
        if let Some(v) = obj.get(key).and_then(Value::as_f64) {
            return Some(v);
        }
    }
    if let Some(status) = obj.get("status").and_then(Value::as_object) {
        if let Some(v) = status
            .get("currentReplicas")
            .or_else(|| status.get("desiredReplicas"))
            .and_then(Value::as_f64)
        {
            return Some(v);
        }
    }
    if let Some(pods) = obj.get("pods").and_then(Value::as_array) {
        return Some(pods.len() as f64);
    }
    None
}

fn extract_cpu_millicores(obj: &serde_json::Map<String, Value>) -> Option<f64> {
    for key in ["cpu_millicores", "total_cpu_millicores", "cpu_m"] {
        if let Some(v) = obj.get(key).and_then(Value::as_f64) {
            return Some(v);
        }
        if let Some(s) = obj.get(key).and_then(Value::as_str) {
            if let Some(parsed) = parse_k8s_cpu_millicores(s) {
                return Some(parsed);
            }
        }
    }
    if let Some(pods) = obj.get("pods").and_then(Value::as_array) {
        let mut total = 0.0;
        let mut found = false;
        for pod in pods {
            if let Some(c) = pod.get("cpu_millicores").and_then(Value::as_f64) {
                total += c;
                found = true;
            } else if let Some(s) = pod.get("cpu").and_then(Value::as_str) {
                if let Some(c) = parse_k8s_cpu_millicores(s) {
                    total += c;
                    found = true;
                }
            }
        }
        if found {
            return Some(total);
        }
    }
    None
}

fn extract_memory_mib(obj: &serde_json::Map<String, Value>) -> Option<f64> {
    for key in ["memory_mib", "total_memory_mib", "mem_mib"] {
        if let Some(v) = obj.get(key).and_then(Value::as_f64) {
            return Some(v);
        }
        if let Some(s) = obj.get(key).and_then(Value::as_str) {
            if let Some(parsed) = parse_k8s_memory_mib(s) {
                return Some(parsed);
            }
        }
    }
    if let Some(pods) = obj.get("pods").and_then(Value::as_array) {
        let mut total = 0.0;
        let mut found = false;
        for pod in pods {
            if let Some(m) = pod.get("memory_mib").and_then(Value::as_f64) {
                total += m;
                found = true;
            } else if let Some(s) = pod.get("memory").and_then(Value::as_str) {
                if let Some(m) = parse_k8s_memory_mib(s) {
                    total += m;
                    found = true;
                }
            }
        }
        if found {
            return Some(total);
        }
    }
    None
}

fn parse_k8s_cpu_millicores(raw: &str) -> Option<f64> {
    let s = raw.trim();
    if let Some(m) = s.strip_suffix('m') {
        m.parse::<f64>().ok()
    } else if let Some(n) = s.strip_suffix('n') {
        n.parse::<f64>().ok().map(|v| v / 1_000_000.0)
    } else {
        s.parse::<f64>().ok().map(|cores| cores * 1000.0)
    }
}

fn parse_k8s_memory_mib(raw: &str) -> Option<f64> {
    let s = raw.trim();
    if let Some(v) = s.strip_suffix("Gi") {
        v.parse::<f64>().ok().map(|g| g * 1024.0)
    } else if let Some(v) = s.strip_suffix("Mi") {
        v.parse::<f64>().ok()
    } else if let Some(v) = s.strip_suffix("Ki") {
        v.parse::<f64>().ok().map(|k| k / 1024.0)
    } else if let Some(v) = s.strip_suffix('G') {
        v.parse::<f64>().ok().map(|g| g * 953.674)
    } else if let Some(v) = s.strip_suffix('M') {
        v.parse::<f64>().ok().map(|m| m * 0.953674)
    } else {
        s.parse::<f64>().ok()
    }
}

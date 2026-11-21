use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleStats {
    pub n: usize,
    pub min: f64,
    pub median: f64,
    pub mean: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
    pub stddev: f64,
    pub ci95_half_width: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerfComparison {
    pub metric_name: String,
    pub higher_is_better: bool,
    pub warmup_runs: usize,
    pub measured_pairs: usize,
    pub samples_a: Vec<f64>,
    pub samples_b: Vec<f64>,
    pub stats_a: SampleStats,
    pub stats_b: SampleStats,
    /// Percentage change of median B relative to median A: `((median_b - median_a) / median_a) * 100.0`.
    pub delta_pct: f64,
    /// Number of interleaved pairs where Variant B outperformed Variant A.
    pub paired_wins: usize,
    /// True when median improved in the desired direction and B won strictly more than half of pairs.
    pub improved: bool,
}

fn student_t_95(n: usize) -> f64 {
    match n {
        0 | 1 => 0.0,
        2 => 12.706,
        3 => 4.303,
        4 => 3.182,
        5 => 2.776,
        6 => 2.571,
        7 => 2.447,
        8 => 2.365,
        9 => 2.306,
        10 => 2.262,
        _ => 1.96,
    }
}

pub fn compute_stats(samples: &[f64]) -> Result<SampleStats, String> {
    if samples.is_empty() {
        return Err("cannot compute stats on empty sample set".to_string());
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));

    let n = sorted.len();
    let min = sorted[0];
    let max = sorted[n - 1];
    let median = if n.is_multiple_of(2) {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    };

    let mean = sorted.iter().sum::<f64>() / (n as f64);
    let sum_sq_diff: f64 = sorted
        .iter()
        .map(|x| {
            let d = x - mean;
            d * d
        })
        .sum();
    let variance = sum_sq_diff / (n as f64);
    let stddev = variance.sqrt();

    let ci95_half_width = if n > 1 {
        let sample_stddev = (sum_sq_diff / ((n - 1) as f64)).sqrt();
        student_t_95(n) * sample_stddev / (n as f64).sqrt()
    } else {
        0.0
    };

    let p95_idx = ((n as f64) * 0.95).ceil() as usize;
    let p95 = sorted[p95_idx.saturating_sub(1).min(n - 1)];

    let p99_idx = ((n as f64) * 0.99).ceil() as usize;
    let p99 = sorted[p99_idx.saturating_sub(1).min(n - 1)];

    Ok(SampleStats {
        n,
        min,
        median,
        mean,
        p95,
        p99,
        max,
        stddev,
        ci95_half_width,
    })
}

pub fn compute_perf_comparison(
    metric_name: String,
    higher_is_better: bool,
    warmup_runs: usize,
    samples_a: Vec<f64>,
    samples_b: Vec<f64>,
) -> Result<PerfComparison, String> {
    if samples_a.is_empty() || samples_a.len() != samples_b.len() {
        return Err(format!(
            "interleaved samples must be non-empty and equal length (got A={}, B={})",
            samples_a.len(),
            samples_b.len()
        ));
    }

    let stats_a = compute_stats(&samples_a)?;
    let stats_b = compute_stats(&samples_b)?;

    let delta_pct = if stats_a.median.abs() < f64::EPSILON {
        0.0
    } else {
        ((stats_b.median - stats_a.median) / stats_a.median) * 100.0
    };

    let paired_wins = samples_a
        .iter()
        .zip(samples_b.iter())
        .filter(|(a, b)| {
            if higher_is_better {
                b > a
            } else {
                b < a
            }
        })
        .count();

    let median_improved = if higher_is_better {
        stats_b.median > stats_a.median
    } else {
        stats_b.median < stats_a.median
    };
    let improved = median_improved && paired_wins * 2 > samples_a.len();

    Ok(PerfComparison {
        metric_name,
        higher_is_better,
        warmup_runs,
        measured_pairs: samples_a.len(),
        samples_a,
        samples_b,
        stats_a,
        stats_b,
        delta_pct,
        paired_wins,
        improved,
    })
}

/// Extracts a numeric metric from stdout via JSON key or prefix/label pattern,
/// falling back to wall-clock `duration_ms` when neither extractor is configured.
pub fn extract_metric_value(
    stdout: &str,
    fallback_duration_ms: u64,
    metric_prefix_pattern: Option<&str>,
    metric_json_key: Option<&str>,
) -> Result<f64, String> {
    if let Some(json_key) = metric_json_key
        && !json_key.trim().is_empty()
    {
        for line in stdout.lines().rev() {
            let trimmed = line.trim();
            if trimmed.starts_with('{')
                && let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
                && let Some(num) = lookup_json_number(&val, json_key)
            {
                return Ok(num);
            }
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(stdout)
            && let Some(num) = lookup_json_number(&val, json_key)
        {
            return Ok(num);
        }
        return Err(format!(
            "failed to extract JSON numeric key '{json_key}' from perf stdout"
        ));
    }

    if let Some(pattern) = metric_prefix_pattern
        && !pattern.trim().is_empty()
    {
        // Support literal prefix like "ops/s:" or regex-like "ops/s:\\s*([0-9.]+)"
        let anchor = pattern
            .split(['(', '\\'])
            .next()
            .unwrap_or(pattern)
            .trim();
        for line in stdout.lines() {
            if let Some(idx) = line.find(anchor) {
                let after = &line[idx + anchor.len()..];
                if let Some(num) = parse_first_f64(after) {
                    return Ok(num);
                }
            }
        }
        return Err(format!(
            "failed to extract numeric metric matching '{pattern}' from perf stdout"
        ));
    }

    Ok(fallback_duration_ms as f64)
}

fn lookup_json_number(val: &serde_json::Value, dotted_key: &str) -> Option<f64> {
    let mut cur = val;
    for part in dotted_key.split('.') {
        cur = cur.get(part)?;
    }
    cur.as_f64()
}

fn parse_first_f64(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_digit()
            || (b == b'-' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit())
        {
            let start = i;
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_digit()
                    || matches!(bytes[i], b'.' | b'e' | b'E' | b'+' | b'-'))
            {
                i += 1;
            }
            if let Ok(n) = s[start..i].parse::<f64>() {
                return Some(n);
            }
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_interleaved_stats_lower_is_better() {
        let cmp = compute_perf_comparison(
            "duration_ms".to_string(),
            false,
            1,
            vec![100.0, 102.0, 98.0, 104.0, 100.0],
            vec![60.0, 62.0, 58.0, 61.0, 59.0],
        )
        .unwrap();
        assert_eq!(cmp.stats_a.median, 100.0);
        assert_eq!(cmp.stats_b.median, 60.0);
        assert!((cmp.delta_pct - (-40.0)).abs() < 1e-6);
        assert_eq!(cmp.paired_wins, 5);
        assert!(cmp.improved);
    }

    #[test]
    fn computes_interleaved_stats_higher_is_better() {
        let cmp = compute_perf_comparison(
            "ops_per_sec".to_string(),
            true,
            1,
            vec![1000.0, 1050.0, 980.0],
            vec![1500.0, 1520.0, 1490.0],
        )
        .unwrap();
        assert!(cmp.delta_pct > 45.0);
        assert_eq!(cmp.paired_wins, 3);
        assert!(cmp.improved);
    }

    #[test]
    fn extracts_metrics_from_json_and_regex_like_prefix() {
        let json_out = "warmup done\n{\"metrics\": {\"p95_ms\": 12.75}}\n";
        let v1 = extract_metric_value(json_out, 99, None, Some("metrics.p95_ms")).unwrap();
        assert!((v1 - 12.75).abs() < 1e-6);

        let text_out = "benchmark finished\nthroughput ops/s:   4250.5\n";
        let v2 = extract_metric_value(text_out, 99, Some("ops/s:\\s*([0-9.]+)"), None).unwrap();
        assert!((v2 - 4250.5).abs() < 1e-6);

        let v3 = extract_metric_value("plain output", 123, None, None).unwrap();
        assert_eq!(v3, 123.0);
    }

    #[test]
    fn computes_p99_max_and_ci95_half_width() {
        // n = 5 samples: [10, 12, 14, 16, 18]
        // mean = 14.0, sum_sq_diff = 40.0, sample_stddev = sqrt(40 / 4) = sqrt(10)
        // t_{0.975, 4} = 2.776 => ci95 = 2.776 * sqrt(10) / sqrt(5) = 2.776 * sqrt(2)
        let samples = vec![18.0, 10.0, 14.0, 12.0, 16.0];
        let stats = compute_stats(&samples).unwrap();
        assert_eq!(stats.n, 5);
        assert_eq!(stats.min, 10.0);
        assert_eq!(stats.max, 18.0);
        assert_eq!(stats.p99, 18.0);
        let expected_ci95 = 2.776 * (10.0_f64).sqrt() / (5.0_f64).sqrt();
        assert!(
            (stats.ci95_half_width - expected_ci95).abs() < 1e-9,
            "expected ci95 {expected_ci95}, got {}",
            stats.ci95_half_width
        );

        // n = 1 => ci95_half_width == 0.0
        let single = compute_stats(&[42.0]).unwrap();
        assert_eq!(single.max, 42.0);
        assert_eq!(single.p99, 42.0);
        assert_eq!(single.ci95_half_width, 0.0);

        // n = 100 => p99 is 99th value, max is 100th value, t = 1.96
        let hundred: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        let stats100 = compute_stats(&hundred).unwrap();
        assert_eq!(stats100.p99, 99.0);
        assert_eq!(stats100.max, 100.0);
        assert!(stats100.ci95_half_width > 5.0 && stats100.ci95_half_width < 6.0);
    }
}

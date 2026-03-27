//! System metrics tool — CPU, memory, and GPU usage for capacity-aware scheduling.

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::mcp::{ContentBlock, ToolResult};

/// Read CPU usage from /proc/loadavg (1-minute load average as percentage of cores).
fn read_cpu_usage() -> Result<f64> {
    let contents = std::fs::read_to_string("/proc/loadavg")?;
    let load1: f64 = contents
        .split_ascii_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);

    // Normalize to percentage: (load / num_cpus) * 100
    let num_cpus = std::thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(1.0);

    Ok((load1 / num_cpus * 100.0).min(100.0))
}

/// Read memory stats from /proc/meminfo.
fn read_memory() -> Result<(f64, f64)> {
    let contents = std::fs::read_to_string("/proc/meminfo")?;

    let mut total_kb: u64 = 0;
    let mut available_kb: u64 = 0;

    for line in contents.lines() {
        if line.starts_with("MemTotal:") {
            total_kb = line
                .split_ascii_whitespace()
                .nth(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        } else if line.starts_with("MemAvailable:") {
            available_kb = line
                .split_ascii_whitespace()
                .nth(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }

    let total_gb = total_kb as f64 / 1_048_576.0;
    let used_gb = (total_kb.saturating_sub(available_kb)) as f64 / 1_048_576.0;

    Ok((used_gb, total_gb))
}

/// Query GPU metrics via nvidia-smi.
async fn read_gpu() -> Option<(f64, f64, f64)> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8(output.stdout).ok()?;
    let parts: Vec<&str> = text.trim().splitn(3, ',').collect();
    if parts.len() < 3 {
        return None;
    }

    let usage_pct: f64 = parts[0].trim().parse().ok()?;
    let mem_used_mib: f64 = parts[1].trim().parse().ok()?;
    let mem_total_mib: f64 = parts[2].trim().parse().ok()?;

    Some((usage_pct, mem_used_mib / 1024.0, mem_total_mib / 1024.0))
}

/// Return system metrics as JSON.
///
/// No input parameters required.
pub async fn system_metrics(_args: Value) -> Result<ToolResult> {
    let cpu_pct = read_cpu_usage().unwrap_or_else(|e| { tracing::warn!("CPU read failed: {e}"); 0.0 });
    let (mem_used_gb, mem_total_gb) = read_memory().unwrap_or_else(|e| { tracing::warn!("memory read failed: {e}"); (0.0, 0.0) });

    let gpu = read_gpu().await;

    let gpu_json = match gpu {
        Some((usage_pct, mem_used_gb, mem_total_gb)) => json!({
            "usage_pct": round2(usage_pct),
            "memory_used_gb": round2(mem_used_gb),
            "memory_total_gb": round2(mem_total_gb),
        }),
        None => json!(null),
    };

    let metrics = json!({
        "cpu": {
            "usage_pct": round2(cpu_pct),
        },
        "memory": {
            "used_gb": round2(mem_used_gb),
            "total_gb": round2(mem_total_gb),
        },
        "gpu": gpu_json,
    });

    let text = serde_json::to_string_pretty(&metrics)?;
    Ok(ToolResult::success(vec![ContentBlock::text(text)]))
}

/// Round to 2 decimal places.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round2_typical() {
        assert!((round2(3.14159) - 3.14).abs() < 1e-9);
    }

    #[test]
    fn round2_zero() {
        assert_eq!(round2(0.0), 0.0);
    }

    #[test]
    fn round2_exact() {
        assert_eq!(round2(1.50), 1.5);
    }

    #[test]
    fn cpu_usage_reads_without_panic() {
        // On any Linux system /proc/loadavg should be readable.
        let _ = read_cpu_usage();
    }

    #[test]
    fn memory_reads_without_panic() {
        let _ = read_memory();
    }

    #[tokio::test]
    async fn system_metrics_returns_valid_json() {
        let result = system_metrics(json!({})).await.unwrap();
        assert!(result.is_error.is_none(), "should not be an error result");
        assert!(!result.content.is_empty());

        let text = result.content[0].text.as_ref().unwrap();
        let parsed: Value = serde_json::from_str(text).expect("should be valid JSON");
        assert!(parsed.get("cpu").is_some());
        assert!(parsed.get("memory").is_some());
        // gpu may be null on systems without nvidia-smi
        assert!(parsed.get("gpu").is_some());
    }
}

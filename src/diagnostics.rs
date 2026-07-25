//! Opt-in, local-only operational diagnostics.
//!
//! Events intentionally contain only fixed event names, booleans, durations,
//! provider names, and HTTP status codes. Prompts, command text, output, paths,
//! model replies, credentials, and memory contents are never recorded.

use crate::config;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagnosticEvent {
    pub timestamp: u64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
}

impl DiagnosticEvent {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            kind: kind.into(),
            success: None,
            duration_ms: None,
            gateway: None,
            status_code: None,
        }
    }

    pub fn outcome(mut self, success: bool, duration_ms: u128) -> Self {
        self.success = Some(success);
        self.duration_ms = Some(duration_ms);
        self
    }

    pub fn gateway(mut self, provider: config::GatewayProvider, status_code: Option<u16>) -> Self {
        self.gateway = Some(provider.as_str().to_owned());
        self.status_code = status_code;
        self
    }
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct DiagnosticSummary {
    pub events: usize,
    pub tool_success_rate: Option<f64>,
    pub gateway_success_rate: Option<f64>,
    pub gateway_latency_p50_ms: Option<u128>,
    pub gateway_latency_p95_ms: Option<u128>,
    pub startup_p95_ms: Option<u128>,
}

pub fn path() -> PathBuf {
    config::get_app_dir().join("diagnostics.jsonl")
}

pub fn record(event: DiagnosticEvent) {
    let Ok(settings) = config::load_or_create_config() else {
        return;
    };
    if !settings.diagnostics_enabled {
        return;
    }
    if let Err(error) = append(&event) {
        eprintln!("warning: could not record local diagnostics: {error}");
    }
}

fn append(event: &DiagnosticEvent) -> Result<()> {
    let path = path();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    set_private_permissions(&path)?;
    serde_json::to_writer(&mut file, event).context("failed to encode diagnostic event")?;
    writeln!(file).context("failed to finish diagnostic event")?;
    Ok(())
}

pub fn read_events() -> Result<Vec<DiagnosticEvent>> {
    let path = path();
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    };
    source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("invalid local diagnostic event"))
        .collect()
}

pub fn summary(events: &[DiagnosticEvent]) -> DiagnosticSummary {
    let tool = events
        .iter()
        .filter(|event| event.kind == "tool.command")
        .collect::<Vec<_>>();
    let gateway = events
        .iter()
        .filter(|event| event.kind.starts_with("gateway.complete."))
        .collect::<Vec<_>>();
    let gateway_latencies = gateway
        .iter()
        .filter_map(|event| event.duration_ms)
        .collect::<Vec<_>>();
    let startups = events
        .iter()
        .filter(|event| event.kind == "startup")
        .filter_map(|event| event.duration_ms)
        .collect::<Vec<_>>();
    DiagnosticSummary {
        events: events.len(),
        tool_success_rate: success_rate(&tool),
        gateway_success_rate: success_rate(&gateway),
        gateway_latency_p50_ms: percentile(gateway_latencies.clone(), 50),
        gateway_latency_p95_ms: percentile(gateway_latencies, 95),
        startup_p95_ms: percentile(startups, 95),
    }
}

fn success_rate(events: &[&DiagnosticEvent]) -> Option<f64> {
    let outcomes = events
        .iter()
        .filter_map(|event| event.success)
        .collect::<Vec<_>>();
    (!outcomes.is_empty())
        .then(|| outcomes.iter().filter(|success| **success).count() as f64 / outcomes.len() as f64)
}

fn percentile(mut values: Vec<u128>, percentile: usize) -> Option<u128> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let index = ((values.len() - 1) * percentile).div_ceil(100);
    values.get(index).copied()
}

pub fn clear() -> Result<()> {
    match fs::remove_file(path()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to clear local diagnostics"),
    }
}

fn set_private_permissions(_path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_calculates_rates_and_percentiles() {
        let events = vec![
            DiagnosticEvent::new("tool.command").outcome(true, 10),
            DiagnosticEvent::new("tool.command").outcome(false, 20),
            DiagnosticEvent::new("gateway.complete.chat").outcome(true, 100),
            DiagnosticEvent::new("gateway.complete.chat").outcome(true, 300),
            DiagnosticEvent::new("startup").outcome(true, 40),
        ];
        let summary = summary(&events);
        assert_eq!(summary.tool_success_rate, Some(0.5));
        assert_eq!(summary.gateway_success_rate, Some(1.0));
        assert_eq!(summary.gateway_latency_p50_ms, Some(300));
        assert_eq!(summary.gateway_latency_p95_ms, Some(300));
        assert_eq!(summary.startup_p95_ms, Some(40));
    }

    #[test]
    fn event_schema_has_no_content_fields() {
        let encoded =
            serde_json::to_string(&DiagnosticEvent::new("gateway.complete.chat")).unwrap();
        for forbidden in ["prompt", "command", "output", "memory", "credential"] {
            assert!(!encoded.contains(forbidden));
        }
    }
}

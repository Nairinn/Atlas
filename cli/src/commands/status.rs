//! `atlas status` — show current service health for the configured project.

use crate::api::ApiClient;
use crate::commands::Format;
use crate::config::AtlasConfig;
use anyhow::Result;
use owo_colors::OwoColorize;
use std::path::Path;

pub async fn run(
    config_path: &Path,
    format: Format,
    mock: bool,
    base_url: Option<String>,
) -> Result<()> {
    let cfg = AtlasConfig::load(config_path)?;
    let client = ApiClient::from_config(&cfg, mock, base_url);
    let resp = client.status(&cfg.project.name).await?;

    if format == Format::Json {
        super::print_json(serde_json::to_value(&resp)?, mock)?;
        return Ok(());
    }

    println!("{} ({})", resp.project_name.bold(), cfg.project.region);
    println!();
    println!(
        "{:<10} {:<8} {:>10} {:>14} {:>12}",
        "SERVICE".dimmed(),
        "STATUS".dimmed(),
        "P95 (ms)".dimmed(),
        "REQ / 24H".dimmed(),
        "ERR RATE".dimmed()
    );
    for s in &resp.services {
        let status_cell = if s.healthy {
            "healthy".green().to_string()
        } else {
            "DOWN".red().to_string()
        };
        // Usage is null until gateway metrics are labelled per project;
        // render the honest "—" rather than a zero that looks measured.
        let p95 = s
            .p95_latency_ms
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".into());
        let reqs = s
            .requests_24h
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".into());
        let err = s
            .error_rate
            .map(|v| format!("{v:.3}%"))
            .unwrap_or_else(|| "—".into());
        println!(
            "{:<10} {:<8} {:>10} {:>14} {:>11}",
            s.name, status_cell, p95, reqs, err
        );
    }
    Ok(())
}

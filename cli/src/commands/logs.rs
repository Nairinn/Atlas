//! `atlas logs [service]` — the project's audit trail.
//!
//! "Logs" here are audit events: the control plane records what was done
//! to the project (deploys, key lifecycle), which is the attributable data
//! it can vouch for. `control-plane` is accepted as a service name because
//! that is the value those rows carry.

use crate::api::ApiClient;
use crate::commands::Format;
use crate::config::AtlasConfig;
use anyhow::{anyhow, Result};
use owo_colors::OwoColorize;
use std::path::Path;

const ALLOWED_SERVICES: &[&str] = &["auth", "geo", "payments", "events", "control-plane"];

pub async fn run(
    config_path: &Path,
    service: Option<String>,
    format: Format,
    mock: bool,
    base_url: Option<String>,
    since: Option<String>,
    limit: Option<u32>,
) -> Result<()> {
    if let Some(s) = &service {
        if !ALLOWED_SERVICES.contains(&s.as_str()) {
            return Err(anyhow!(
                "unknown service '{}'. Expected one of: {:?}",
                s,
                ALLOWED_SERVICES
            ));
        }
    }

    let cfg = AtlasConfig::load(config_path)?;
    let client = ApiClient::from_config(&cfg, mock, base_url);
    let lines = client
        .logs(
            &cfg.project.name,
            service.as_deref(),
            since.as_deref(),
            limit,
        )
        .await?;

    if format == Format::Json {
        super::print_json(serde_json::to_value(&lines)?, mock)?;
        return Ok(());
    }

    for line in &lines {
        let level = match line.level.as_str() {
            "error" => "ERROR".red().to_string(),
            "warn" => "WARN ".yellow().to_string(),
            _ => "INFO ".green().to_string(),
        };
        println!(
            "{} {:<10} {} {}",
            line.timestamp.dimmed(),
            line.service.cyan(),
            level,
            line.message
        );
    }
    Ok(())
}

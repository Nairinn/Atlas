//! `atlas deploy` — validate config then push to the Atlas control plane.

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

    if format == Format::Human {
        println!(
            "Deploying {} to {}...",
            cfg.project.name.bold(),
            cfg.project.region.cyan()
        );
    }

    let resp = client.deploy(&cfg).await?;

    if format == Format::Json {
        super::print_json(serde_json::to_value(&resp)?, mock)?;
        return Ok(());
    }

    for svc in &resp.provisioned {
        let marker = match svc.status.as_str() {
            "ok" => "✓".green().to_string(),
            "skipped" => "-".yellow().to_string(),
            _ => "✗".red().to_string(),
        };
        match &svc.detail {
            Some(d) => println!("{} {} service provisioned ({})", marker, svc.service, d),
            None => println!("{} {} service provisioned", marker, svc.service),
        }
    }

    println!("{} API key active: {}", "✓".green(), cfg.masked_api_key());
    println!("Live at: {}", resp.endpoint.cyan());
    println!(
        "{}",
        format!("Done in {:.1}s", resp.elapsed_ms as f64 / 1000.0).bold()
    );
    Ok(())
}

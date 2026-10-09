//! `atlas validate` — parse and validate `atlas.toml`. No network calls.

use crate::commands::Format;
use crate::config::AtlasConfig;
use anyhow::Result;
use owo_colors::OwoColorize;
use serde_json::json;
use std::path::Path;

pub fn run(config_path: &Path, format: Format) -> Result<()> {
    let cfg = AtlasConfig::load(config_path)?;

    // The per-service tuning sections ([auth], [geo], ...) were removed;
    // they are accepted for one release so existing files keep working.
    let deprecated = cfg.deprecated_sections.clone();

    if format == Format::Json {
        let summary = json!({
            "ok": true,
            "project": {
                "name": cfg.project.name,
                "environment": cfg.project.environment.as_str(),
                "region": cfg.project.region,
                "api_key": cfg.masked_api_key(),
            },
            "services": {
                "auth": cfg.services.auth,
                "geo": cfg.services.geo,
                "payments": cfg.services.payments,
                "events": cfg.services.events,
            },
            "warnings": deprecated
                .iter()
                .map(|s| format!(
                    "[{s}] section is deprecated and ignored: the platform never read it; remove it"
                ))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    // Human output. One line per enabled service, then a summary.
    println!(
        "{} project: {} ({})",
        "✓".green(),
        cfg.project.name.bold(),
        cfg.project.environment.as_str()
    );

    if cfg.services.auth {
        println!("{} auth: enabled", "✓".green());
    }
    if cfg.services.geo {
        println!("{} geo: enabled", "✓".green());
    }
    if cfg.services.payments {
        println!("{} payments: enabled", "✓".green());
    }
    if cfg.services.events {
        println!("{} events: enabled", "✓".green());
    }

    for name in &deprecated {
        println!(
            "{} [{name}] section is deprecated and ignored: the platform never read it; remove it",
            "!".yellow(),
        );
    }

    if deprecated.is_empty() {
        println!("\n{}", "All good.".green().bold());
    } else {
        println!(
            "\n{}",
            format!("Valid, with {} deprecation warning(s).", deprecated.len())
                .yellow()
                .bold()
        );
    }
    Ok(())
}

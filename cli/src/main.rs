//! `atlas` — developer CLI for the Atlas platform.
//!
//! Reads `atlas.toml` from the current directory (or `--config`) and talks
//! to the Atlas control plane over REST. Use `--mock` to exercise commands
//! without a running backend.

mod api;
mod commands;
mod config;

use anyhow::Result;
use clap::{Parser, Subcommand};
use commands::Format;
use owo_colors::OwoColorize;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "atlas",
    version,
    about = "Developer CLI for the Atlas platform.",
    long_about = "Manage Atlas projects from your terminal: validate atlas.toml, deploy, \
                  monitor service status, tail logs, and manage API keys."
)]
struct Cli {
    /// Path to atlas.toml.
    #[arg(long, default_value = "atlas.toml", global = true)]
    config: PathBuf,

    /// Emit machine-readable JSON instead of human-readable output.
    #[arg(long, global = true)]
    json: bool,

    /// Use deterministic in-memory responses instead of the control
    /// plane. Opt-in: the safe default is to touch nothing, so exploring
    /// the CLI must be a deliberate choice. Also set by ATLAS_MOCK=1.
    #[arg(long, global = true)]
    mock: bool,

    /// Override the control plane base URL.
    #[arg(long, global = true)]
    base_url: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Parse and validate atlas.toml.
    Validate,
    /// Validate and deploy the project to the Atlas control plane.
    Deploy,
    /// Print current service health for this project.
    Status,
    /// Tail logs for one or all services.
    Logs {
        /// Service to filter by: auth | geo | payments | events |
        /// control-plane. Omit for all.
        service: Option<String>,
        /// Only rows at or after this RFC 3339 timestamp.
        #[arg(long)]
        since: Option<String>,
        /// At most this many rows (newest first, then reversed for
        /// display). The server caps at 200.
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Manage API keys for this project.
    Keys {
        #[command(subcommand)]
        action: commands::keys::KeysCommand,
    },
}

/// Mock when the flag is passed or ATLAS_MOCK is set; live otherwise.
/// In mock mode every command prints a notice so output is never mistaken
/// for a real deployment's.
fn use_mock(cli: &Cli) -> bool {
    let on = cli.mock
        || std::env::var("ATLAS_MOCK")
            .map(|v| v == "1")
            .unwrap_or(false);
    if on {
        eprintln!("[MOCK — nothing was changed]");
    }
    on
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let format = if cli.json {
        Format::Json
    } else {
        Format::Human
    };
    // Computed once: the match arms move fields out of `cli`.
    let mock = use_mock(&cli);

    let result: Result<()> = match cli.command {
        Command::Validate => commands::validate::run(&cli.config, format),
        Command::Deploy => commands::deploy::run(&cli.config, format, mock, cli.base_url).await,
        Command::Status => commands::status::run(&cli.config, format, mock, cli.base_url).await,
        Command::Logs {
            service,
            since,
            limit,
        } => {
            commands::logs::run(
                &cli.config,
                service,
                format,
                mock,
                cli.base_url,
                since,
                limit,
            )
            .await
        }
        Command::Keys { action } => {
            commands::keys::run(&cli.config, action, format, mock, cli.base_url).await
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if format == Format::Json {
                let payload = serde_json::json!({
                    "ok": false,
                    "error": err.to_string(),
                });
                eprintln!(
                    "{}",
                    serde_json::to_string_pretty(&payload).unwrap_or_default()
                );
            } else {
                eprintln!("{} {}", "error:".red().bold(), err);
                // Walk the error chain so the user sees the root cause.
                let mut source = err.source();
                while let Some(s) = source {
                    eprintln!("  {} {}", "caused by:".dimmed(), s);
                    source = s.source();
                }
            }
            ExitCode::FAILURE
        }
    }
}

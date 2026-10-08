//! Atlas location-consumer entrypoint.
//!
//! Two concurrent jobs:
//!   1. The retention sweep (the substantive one) on a timer.
//!   2. The Kafka ingest loop, for observability.
//!
//! Both stop on SIGTERM / SIGINT.

use anyhow::Context;
use atlas_location_consumer::{config, consumer, metrics, reaper};
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = config::Config::from_env();
    info!(
        topic = %cfg.topic,
        group = %cfg.consumer_group,
        sweep_interval_s = cfg.sweep_interval.as_secs(),
        "starting atlas-location-consumer"
    );

    let metrics_handle = metrics::install_recorder();
    metrics::serve(cfg.metrics_addr, metrics_handle).await?;

    let pool = PgPoolOptions::new()
        .max_connections(cfg.database_pool_size)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&cfg.database_url)
        .await
        .context("postgres connection")?;
    info!("postgres pool ready");

    // The reaper is detached: it has its own error handling and must keep
    // running even if the Kafka loop is stuck on an unreachable broker.
    // Retention is the job that must not stop.
    tokio::spawn(reaper::run(
        pool.clone(),
        cfg.sweep_interval,
        cfg.sweep_batch_size,
        cfg.safety_vote_horizon,
    ));

    let kafka = consumer::build(&cfg.kafka_brokers, &cfg.consumer_group)
        .context("building kafka consumer")?;
    consumer::run(kafka, &cfg.topic, shutdown_signal()).await;

    info!("atlas-location-consumer exited cleanly");
    Ok(())
}

/// Wait for SIGTERM (k8s rolling deploy) or SIGINT (local ctrl-c).
async fn shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async {
        signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

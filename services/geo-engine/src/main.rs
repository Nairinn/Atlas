//! Atlas geo-engine entrypoint. Thin shell over the library crate.
//!
//! Boot order:
//!   1. Install tracing + Prometheus recorder.
//!   2. Connect to Postgres (fail fast if unreachable).
//!   3. Build the Kafka producer (lazy; broker contact happens on first send).
//!   4. Spawn the metrics HTTP server.
//!   5. Start the tonic server with the health service.
//!   6. Trap SIGTERM / SIGINT for graceful shutdown.

use anyhow::Context;
use atlas_geo_engine::{config, db, health, kafka, metrics, pb, service};
use tonic::transport::Server;
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
    info!(?cfg.grpc_addr, ?cfg.metrics_addr, "starting atlas-geo-engine");

    let metrics_handle = metrics::install_recorder();
    metrics::serve(cfg.metrics_addr, metrics_handle).await?;

    let pool = db::connect(&cfg.database_url, cfg.database_pool_size)
        .await
        .context("postgres connection")?;
    info!("postgres pool ready");

    let producer =
        kafka::LocationProducer::new(&cfg.kafka_brokers, cfg.kafka_topic_location_updates.clone())
            .context("kafka producer")?;
    info!(brokers = %cfg.kafka_brokers, topic = %cfg.kafka_topic_location_updates, "kafka producer ready");

    let svc = service::GeoEngineImpl::new(pool, producer);

    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health::mark_serving(&mut health_reporter).await;

    info!(addr = %cfg.grpc_addr, "gRPC server listening");
    Server::builder()
        .add_service(health_service)
        .add_service(pb::geo::geo_engine_server::GeoEngineServer::new(svc))
        .serve_with_shutdown(cfg.grpc_addr, atlas_edge::shutdown::shutdown_signal())
        .await
        .context("tonic server")?;

    info!("atlas-geo-engine exited cleanly");
    Ok(())
}

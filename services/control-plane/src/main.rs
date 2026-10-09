//! Atlas control-plane entrypoint.
//!
//! Boot order:
//!   1. Install tracing + Prometheus recorder.
//!   2. Connect to Postgres — fail fast, since no route works without it.
//!   3. Spawn the metrics HTTP server on its own port.
//!   4. Serve the REST API on 8081, matching the CLI's default base URL.
//!   5. Trap SIGTERM / SIGINT for graceful shutdown.

use anyhow::Context;
use atlas_control_plane::{config, db, metrics, ratelimit, routes, state};
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
        http_addr = %cfg.http_addr,
        metrics_addr = %cfg.metrics_addr,
        gateway_metrics = %cfg.gateway_metrics_url,
        "starting atlas-control-plane"
    );

    let metrics_handle = metrics::install_recorder();
    metrics::serve(cfg.metrics_addr, metrics_handle).await?;

    // Unlike the gateway's lazy upstream channels, this connects eagerly:
    // every route touches Postgres, so a control plane that cannot reach
    // it has nothing to serve and should fail loudly at boot.
    let pool = db::connect(&cfg.database_url, cfg.database_pool_size)
        .await
        .context("postgres connection")?;
    info!("postgres pool ready");

    let limiters = ratelimit::Limiters::new(cfg.rate_limit.clone());
    limiters.spawn_gc();
    info!(
        default_per_minute = cfg.rate_limit.default_per_minute,
        signup_per_minute = cfg.rate_limit.signup_per_minute,
        trusted_proxy_hops = cfg.rate_limit.trusted_proxy_hops,
        enabled = cfg.rate_limit.enabled,
        "rate limiting configured (per replica)"
    );

    let http_addr = cfg.http_addr;
    let app = routes::router(state::AppState::new(pool, cfg)?, limiters);

    let listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .with_context(|| format!("binding {http_addr}"))?;
    info!(addr = %http_addr, "control plane listening");

    // into_make_service_with_connect_info so the limiter sees the real peer
    // address; without it every signup would share one bucket.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(atlas_edge::shutdown::shutdown_signal())
    .await
    .context("axum server")?;

    info!("atlas-control-plane exited cleanly");
    Ok(())
}

//! Atlas gateway entrypoint. Thin shell over the library crate.
//!
//! Boot order:
//!   1. Install tracing + Prometheus recorder.
//!   2. Build lazy gRPC channels to auth / geo / payments.
//!   3. Spawn the metrics HTTP server on its own port.
//!   4. Serve the public REST router.
//!   5. Trap SIGTERM / SIGINT for graceful shutdown.
//!
//! Nothing here blocks on a backend being reachable — see `state.rs` for
//! why the channels are lazy.

use anyhow::Context;
use atlas_gateway::{config, ratelimit, routes, state};
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
        auth = %cfg.auth_addr,
        geo = %cfg.geo_addr,
        payments = %cfg.payments_addr,
        "starting atlas-gateway"
    );

    let metrics_handle = atlas_edge::metrics::install_recorder();
    atlas_edge::metrics::serve(cfg.metrics_addr, metrics_handle).await?;

    let limiters = ratelimit::Limiters::new(cfg.rate_limit.clone());
    limiters.spawn_gc();
    info!(
        default_per_minute = cfg.rate_limit.default_per_minute,
        auth_per_minute = cfg.rate_limit.auth_per_minute,
        trusted_proxy_hops = cfg.rate_limit.trusted_proxy_hops,
        enabled = cfg.rate_limit.enabled,
        "rate limiting configured (per replica)"
    );

    let state = state::AppState::connect(&cfg).context("building upstream channels")?;
    // Expired project-key entries would otherwise accumulate for as long
    // as the process lives, and the negative half of that cache is sized
    // by whoever is sending bad keys.
    state.projects.spawn_gc();
    info!(
        project_cache_ttl_secs = cfg.project_cache_ttl.as_secs(),
        "project key resolution ready (this is also the key revocation window)"
    );

    let app = routes::router(state, limiters);

    let listener = tokio::net::TcpListener::bind(cfg.http_addr)
        .await
        .with_context(|| format!("binding {}", cfg.http_addr))?;
    info!(addr = %cfg.http_addr, "REST gateway listening");

    // into_make_service_with_connect_info so the rate limiter sees the real
    // peer address. Without it every unauthenticated request would share one
    // bucket and the credential limit would throttle all clients together.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(atlas_edge::shutdown::shutdown_signal())
    .await
    .context("axum server")?;

    info!("atlas-gateway exited cleanly");
    Ok(())
}

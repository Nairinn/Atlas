//! Prometheus recorder install + scrape endpoint, shared by every Rust
//! edge service.
//!
//! The exporter binds its OWN listener on the configured metrics address
//! rather than living on the public router: `/metrics` on a public edge
//! would hand out request volumes and error rates to anyone who asked.

use axum::{routing::get, Router};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::net::SocketAddr;
use tracing::info;

pub fn install_recorder() -> PrometheusHandle {
    PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install prometheus recorder")
}

/// Spawn the `/metrics` server. Returns once the listener is bound.
pub async fn serve(addr: SocketAddr, handle: PrometheusHandle) -> anyhow::Result<()> {
    let app = Router::new().route(
        "/metrics",
        get(move || {
            let h = handle.clone();
            async move { h.render() }
        }),
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "metrics exporter listening");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "metrics server crashed");
        }
    });
    Ok(())
}

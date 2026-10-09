//! The gateway's per-request tracking middleware. The recorder install
//! and the scrape endpoint are shared with the control plane and live in
//! `atlas_edge::metrics`.

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use std::time::Instant;

/// Record request count and latency, labelled by route template.
///
/// The label is the *matched path* (`/v1/geo/geofences/:id`), never the
/// concrete URI (`/v1/geo/geofences/9f3c...`). Using the raw path would
/// mint a new time series per geofence id and melt Prometheus. Unmatched
/// requests (404s) collapse to a single `unmatched` bucket for the same
/// reason.
pub async fn track(req: Request, next: Next) -> Response {
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_string());
    let method = req.method().clone();

    let start = Instant::now();
    let response = next.run(req).await;
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

    let status = response.status().as_u16().to_string();
    metrics::counter!(
        "atlas_gateway_requests_total",
        "method" => method.to_string(),
        "path" => path.clone(),
        "status" => status,
    )
    .increment(1);
    metrics::histogram!(
        "atlas_gateway_request_duration_ms",
        "method" => method.to_string(),
        "path" => path,
    )
    .record(latency_ms);

    response
}

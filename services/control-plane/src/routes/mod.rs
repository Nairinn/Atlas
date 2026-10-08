//! REST surface.
//!
//! Paths are dictated by `cli/src/api.rs`, which builds them as
//! `{base_url}/projects`, `{base_url}/projects/{name}/status`, and so on,
//! with `base_url` defaulting to `http://localhost:8081/v1`. So everything
//! is mounted under `/v1` and the service listens on 8081.

pub mod accounts;
pub mod keys;
pub mod logs;
pub mod ops;
pub mod projects;

use axum::{middleware, Router};
use tower_http::trace::TraceLayer;

use crate::ratelimit::{self, Limiters};
use crate::state::AppState;
use std::sync::Arc;

/// Router for production, where the listener supplies a peer address.
pub fn router(state: AppState, limiters: Arc<Limiters>) -> Router {
    base_router(state, Arc::clone(&limiters), true)
}

/// Router for tests and any listener without `ConnectInfo`.
pub fn router_without_peer(state: AppState, limiters: Arc<Limiters>) -> Router {
    base_router(state, limiters, false)
}

fn base_router(state: AppState, limiters: Arc<Limiters>, with_peer: bool) -> Router {
    let v1 = Router::new()
        .merge(accounts::routes())
        .merge(projects::routes())
        .merge(keys::routes())
        .merge(logs::routes());

    // Metrics wraps the limiter so a 429 is counted: a spike of them on
    // /v1/accounts is the signal that someone is trying to mass-create
    // accounts, and a limiter that hides its own effect is untuneable.
    let base = Router::new()
        .merge(ops::routes())
        .nest("/v1", v1)
        .layer(middleware::from_fn(track))
        .layer(TraceLayer::new_for_http());
    if with_peer {
        base.layer(middleware::from_fn_with_state(limiters, ratelimit::layer))
            .with_state(state)
    } else {
        base.layer(middleware::from_fn_with_state(
            limiters,
            ratelimit::layer_without_peer,
        ))
        .with_state(state)
    }
}

/// Request counter and latency histogram, labelled by matched route.
///
/// Labelled by route template, never the concrete URI: project names and
/// key prefixes appear in these paths, and using them raw would mint a new
/// time series per project.
async fn track(req: axum::extract::Request, next: middleware::Next) -> axum::response::Response {
    use axum::extract::MatchedPath;
    use std::time::Instant;

    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_string());
    let method = req.method().clone();

    let start = Instant::now();
    let response = next.run(req).await;
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

    metrics::counter!(
        "atlas_control_plane_requests_total",
        "method" => method.to_string(),
        "path" => path.clone(),
        "status" => response.status().as_u16().to_string(),
    )
    .increment(1);
    metrics::histogram!(
        "atlas_control_plane_request_duration_ms",
        "method" => method.to_string(),
        "path" => path,
    )
    .record(latency_ms);

    response
}

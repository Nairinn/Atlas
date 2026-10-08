//! REST surface. One module per backend namespace, mirroring the
//! `atlas.auth` / `atlas.geo` / `atlas.payments` split in the SDK.
//!
//! Route naming follows the platform's public vocabulary rather than the
//! gRPC method names, because this is the surface the TypeScript, Dart,
//! and Rust SDKs will wrap.

pub mod auth;
pub mod geo;
pub mod ops;
pub mod payments;

use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::Method;
use axum::{middleware, Router};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::metrics;
use crate::ratelimit::{self, Limiters};
use crate::state::AppState;
use crate::tenant::{self, Tenant};
use std::sync::Arc;

/// How a request's project is determined.
///
/// Only [`TenantSource::Key`] is used by [`router`], the constructor
/// `main.rs` calls, so it is the only variant that runs in production.
#[derive(Clone)]
pub enum TenantSource {
    /// Resolve the `X-Atlas-Key` header against `control.api_keys`.
    Key,
    /// Use this project for every request without resolving anything.
    /// Test-only; see [`crate::tenant::fixed_layer`].
    Fixed(Box<Tenant>),
}

/// Router for production, where the listener supplies a peer address.
pub fn router(state: AppState, limiters: Arc<Limiters>) -> Router {
    base_router(state, TenantSource::Key, limiters, true)
}

/// Router for tests and for any listener without `ConnectInfo`. Identical
/// except that every request shares the "unknown" address bucket.
pub fn router_without_peer(
    state: AppState,
    limiters: Arc<Limiters>,
    tenants: TenantSource,
) -> Router {
    base_router(state, tenants, limiters, false)
}

fn base_router(
    state: AppState,
    tenants: TenantSource,
    limiters: Arc<Limiters>,
    with_peer: bool,
) -> Router {
    // Any-origin CORS is correct for a token-authenticated public API:
    // credentials never ride in cookies, so there is no CSRF surface to
    // protect, and browser SDK users can call from any origin. Note that
    // `allow_credentials` must stay off — the CORS spec forbids pairing
    // it with a wildcard origin, and turning it on would be the change
    // that introduces the CSRF surface.
    // `x-atlas-key` is DELIBERATELY NOT in the allowed headers: the key
    // is a server-side secret (a browser that can send it is a browser
    // that has it, which is the leak), and CORS-listing it would be an
    // invitation to ship it in a browser bundle. A browser-based
    // frontend calls its own backend, which holds the key.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::DELETE])
        .allow_headers([AUTHORIZATION, CONTENT_TYPE]);

    // The tenant layer wraps the three `/v1` sub-routers as a group rather
    // than being applied per handler. That is the point: a route added to
    // any of them later is tenant-checked without anyone remembering to
    // check it. `ops::routes()` (health, readiness) sits outside, because
    // a probe carries no customer key and must answer whether or not
    // Postgres is reachable.
    //
    // It sits INSIDE the rate limiter (applied by the callers above), so a
    // flood of invalid keys is throttled before it can become a flood of
    // database lookups.
    let v1 = Router::new()
        .nest("/auth", auth::routes())
        .nest("/geo", geo::routes())
        .nest("/payments", payments::routes());
    let v1 = match tenants {
        TenantSource::Key => v1.layer(middleware::from_fn_with_state(state.clone(), tenant::layer)),
        TenantSource::Fixed(t) => v1.layer(middleware::from_fn_with_state(*t, tenant::fixed_layer)),
    };

    // Layer order is outermost-first: metrics wraps everything —
    // including the rate limiter — so a panic-turned-500, a rejected
    // body, AND a 429 all get counted. A limiter that hides its own
    // effect is untuneable, and a spike of 429s is exactly what a
    // dashboard should show.
    let base = Router::new()
        .merge(ops::routes())
        .nest("/v1", v1)
        .layer(middleware::from_fn(metrics::track))
        .layer(TraceLayer::new_for_http())
        .layer(cors);
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

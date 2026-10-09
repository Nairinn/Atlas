//! Request rate limiting: the control plane's vocabulary over the shared
//! atlas-edge limiter core.
//!
//! The GCRA machinery, key derivation, and the per-address anti-rotation
//! ceiling live in `atlas_edge::ratelimit`; this module owns what is
//! control-plane-specific: the signup quota and what the 429 body looks
//! like. The per-replica caveat (N pods behind a balancer is N times the
//! configured rate) is documented in the shared crate.
//!
//! # The endpoint the strict quota exists for
//!
//! `POST /v1/accounts` is the only unauthenticated write in the platform
//! and it mints an API key. Unthrottled, anyone who can reach the port
//! can create unlimited accounts and credentials. Its quota is
//! deliberately tiny — signing up is a once-per-person action, not an
//! API call.
//!
//! # Keying
//!
//! Control-plane callers are CLIs and CI runners that frequently share
//! an egress address, so authenticated requests are keyed by a digest of
//! the API key; keying those by IP would throttle a whole office
//! together. Unauthenticated requests key on the client address.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use atlas_edge::ratelimit as core;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// The control plane's one strict-quota profile: account creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictProfile {
    Signup,
}

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub default_per_minute: u32,
    /// Requests per minute for account creation, per client address.
    /// Deliberately tiny: signing up is a once-per-person action.
    pub signup_per_minute: u32,
    /// Per-address requests per minute applied to every non-exempt
    /// request. The anti-rotation cap.
    pub ip_ceiling_per_minute: u32,
    /// How many proxies sit in front of this service.
    pub trusted_proxy_hops: usize,
    pub enabled: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            // The CLI makes a handful of calls per invocation, so this is
            // generous for a human and still bounds a runaway script.
            default_per_minute: 120,
            // Three signups a minute from one address. A person creating
            // a second account for a colleague is fine; a script is not.
            signup_per_minute: 3,
            // CLI traffic is a handful of calls per invocation; an order
            // of magnitude above the per-key quota covers an office of
            // them behind one egress while capping token rotation.
            ip_ceiling_per_minute: 1200,
            trusted_proxy_hops: 0,
            enabled: true,
        }
    }
}

impl RateLimitConfig {
    fn into_core(self) -> core::Quotas<StrictProfile> {
        core::Quotas {
            default_per_minute: self.default_per_minute,
            ip_ceiling_per_minute: self.ip_ceiling_per_minute,
            trusted_proxy_hops: self.trusted_proxy_hops,
            enabled: self.enabled,
            strict: vec![(StrictProfile::Signup, self.signup_per_minute)],
        }
    }
}

pub struct Limiters {
    inner: Arc<core::Limiters<StrictProfile>>,
}

impl Limiters {
    pub fn new(config: RateLimitConfig) -> Arc<Self> {
        Arc::new(Self {
            inner: core::Limiters::new(config.into_core()),
        })
    }

    pub fn spawn_gc(self: &Arc<Self>) {
        self.inner.spawn_gc();
    }

    fn check(
        &self,
        peer: Option<SocketAddr>,
        headers: &HeaderMap,
        path: &str,
    ) -> Result<(), Duration> {
        if !self.inner.quotas().enabled {
            return Ok(());
        }
        let signup = is_signup_path(path);
        let ip_key = client_key(headers, peer, self.inner.quotas().trusted_proxy_hops);
        self.inner.check_ip_ceiling(&ip_key)?;

        // Signup has no credential to key on by definition, so it is
        // keyed by address. Everything else prefers the API key.
        let key = if signup {
            ip_key
        } else {
            token_key(headers).unwrap_or_else(|| {
                client_key(headers, peer, self.inner.quotas().trusted_proxy_hops)
            })
        };
        let profile = if signup {
            Some(&StrictProfile::Signup)
        } else {
            None
        };
        self.inner.check_primary(profile, &key)
    }
}

/// Paths that must never be throttled (Kubernetes probes; see the
/// gateway's module for why limiting them converts load spikes into
/// rolling outages).
fn is_exempt_path(path: &str) -> bool {
    path == "/healthz" || path == "/readyz"
}

/// Whether a path creates an account, and so deserves the strict quota.
fn is_signup_path(path: &str) -> bool {
    // Matched on the concrete path rather than the route template because
    // the middleware runs before routing has resolved a MatchedPath.
    path == "/v1/accounts"
}

/// Address key: X-Forwarded-For read from the right by trusted hops (see
/// the shared crate for why the leftmost entry is never trusted).
pub fn client_key(headers: &HeaderMap, peer: Option<SocketAddr>, hops: usize) -> String {
    core::client_key(
        headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
        peer,
        hops,
    )
}

/// Token key: digest of the parsed API key bearer, so a live credential
/// is not sitting in a map key in memory.
fn token_key(headers: &HeaderMap) -> Option<String> {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = crate::auth::parse_bearer(header).ok()?;
    core::token_key(Some(token))
}

pub async fn layer(
    State(limiters): State<Arc<Limiters>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    check(&limiters, Some(peer), req, next).await
}

/// Same as [`layer`] but without `ConnectInfo`, for the test harness and
/// for any deployment where the listener does not provide a peer address.
pub async fn layer_without_peer(
    State(limiters): State<Arc<Limiters>>,
    req: Request,
    next: Next,
) -> Response {
    check(&limiters, None, req, next).await
}

async fn check(
    limiters: &Limiters,
    peer: Option<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    if is_exempt_path(&path) {
        return next.run(req).await;
    }
    match limiters.check(peer, req.headers(), &path) {
        Ok(()) => next.run(req).await,
        Err(wait) => {
            // The scope label mirrors which bucket refused: the primary
            // bucket names the abuser better, but the ceiling is what
            // actually caps a token-rotation attack.
            let signup = is_signup_path(&path);
            metrics::counter!(
                "atlas_control_plane_rate_limited_total",
                "scope" => if signup { "signup" } else { "ip_ceiling" },
            )
            .increment(1);
            too_many_requests(wait)
        }
    }
}

fn too_many_requests(wait: Duration) -> Response {
    // Same envelope and code every other control-plane error uses, so a
    // client branching on `code` needs no new case.
    let retry_after = wait.as_secs().max(1);
    (
        StatusCode::TOO_MANY_REQUESTS,
        [("retry-after", retry_after.to_string())],
        Json(json!({
            "error": {
                "code": "resource_exhausted",
                "message": "rate limit exceeded; retry after the interval in the Retry-After header"
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    fn peer(addr: &str) -> Option<SocketAddr> {
        Some(addr.parse().unwrap())
    }

    #[test]
    fn only_account_creation_gets_the_strict_quota() {
        assert!(is_signup_path("/v1/accounts"));
        assert!(!is_signup_path("/v1/projects"));
        assert!(!is_signup_path("/v1/projects/demo/keys"));
    }

    #[test]
    fn health_probes_are_exempt() {
        assert!(is_exempt_path("/healthz"));
        assert!(is_exempt_path("/readyz"));
        assert!(!is_exempt_path("/v1/accounts"));
    }

    /// With no trusted proxies a forged header must be ignored: honouring
    /// it would hand an attacker a fresh signup bucket per request.
    #[test]
    fn forged_forwarded_for_is_ignored_when_no_proxy_is_trusted() {
        let h = headers(&[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(client_key(&h, peer("10.0.0.7:5000"), 0), "ip:10.0.0.7");
    }

    #[test]
    fn spoofed_entries_prepended_by_the_client_are_skipped() {
        let h = headers(&[("x-forwarded-for", "9.9.9.9, 203.0.113.9")]);
        assert_eq!(client_key(&h, peer("10.0.0.7:5000"), 1), "ip:203.0.113.9");
    }

    #[test]
    fn a_short_header_falls_back_to_the_peer_address() {
        let h = headers(&[("x-forwarded-for", "203.0.113.9")]);
        assert_eq!(client_key(&h, peer("10.0.0.7:5000"), 3), "ip:10.0.0.7");
    }

    #[test]
    fn api_keys_are_hashed_not_stored_raw() {
        let h = headers(&[(
            "authorization",
            "Bearer atl_live_0123456789abcdef0123456789abcdef",
        )]);
        let key = token_key(&h).expect("key");
        assert!(key.starts_with("tok:"));
        assert!(!key.contains("0123456789abcdef"));
    }

    #[test]
    fn the_signup_quota_stops_at_the_limit() {
        let limiters = Limiters::new(RateLimitConfig {
            signup_per_minute: 2,
            ..RateLimitConfig::default()
        });
        let headers = HeaderMap::new();
        assert!(limiters.check(None, &headers, "/v1/accounts").is_ok());
        assert!(limiters.check(None, &headers, "/v1/accounts").is_ok());
        assert!(
            limiters.check(None, &headers, "/v1/accounts").is_err(),
            "third signup from one address must be rejected"
        );
    }

    #[test]
    fn exhausting_signups_does_not_block_ordinary_api_calls() {
        let limiters = Limiters::new(RateLimitConfig {
            signup_per_minute: 1,
            default_per_minute: 100,
            ..RateLimitConfig::default()
        });
        let headers = HeaderMap::new();
        assert!(limiters.check(None, &headers, "/v1/accounts").is_ok());
        assert!(limiters.check(None, &headers, "/v1/accounts").is_err());
        assert!(limiters.check(None, &headers, "/v1/projects").is_ok());
    }

    #[test]
    fn retry_after_is_at_least_one_second() {
        let response = too_many_requests(Duration::from_millis(1));
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get("retry-after").unwrap(), "1");
    }
}

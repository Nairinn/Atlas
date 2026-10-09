//! Request rate limiting: the gateway's vocabulary over the shared
//! atlas-edge limiter core.
//!
//! The GCRA machinery, key derivation, and the per-address anti-rotation
//! ceiling live in `atlas_edge::ratelimit`; this module owns what is
//! gateway-specific: which paths get the strict credential quota, how a
//! request is classified, and what the 429 body looks like. The
//! per-replica caveat (three pods behind a balancer is three times the
//! configured rate) is documented in the shared crate.
//!
//! # Keying
//!
//! Authenticated requests are keyed by a digest of the bearer token, not
//! by IP. Behind carrier-grade NAT or a corporate egress, thousands of
//! legitimate users share one address, and an IP-keyed limit would
//! throttle all of them together while doing nothing about a single
//! attacker rotating addresses. Unauthenticated requests have nothing
//! else to key on, so they use the client address.

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

/// The gateway's one strict-quota profile: credential endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictProfile {
    Credential,
}

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub default_per_minute: u32,
    /// Requests per minute for credential endpoints (login, register).
    /// Much lower: these are the ones worth brute forcing.
    pub auth_per_minute: u32,
    /// Per-address requests per minute, applied to EVERY non-exempt
    /// request in addition to the primary bucket. This is the
    /// anti-rotation cap.
    pub ip_ceiling_per_minute: u32,
    /// How many proxies sit in front of this service.
    pub trusted_proxy_hops: usize,
    pub enabled: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            // Generous: a mobile client posting location every second is
            // normal traffic, and throttling real users is worse than the
            // marginal protection of a tight cap.
            default_per_minute: 600,
            // Ten credential attempts a minute is far above what a human
            // needs and far below what a password-guessing run wants.
            auth_per_minute: 10,
            // Ten per second from one address sustained is not normal
            // client behaviour even behind CGNAT; an attacker rotating
            // bogus tokens hits it immediately.
            ip_ceiling_per_minute: 6000,
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
            strict: vec![(StrictProfile::Credential, self.auth_per_minute)],
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
        let credential = is_credential_path(path);
        let ip_key = client_key(headers, peer, self.inner.quotas().trusted_proxy_hops);
        self.inner.check_ip_ceiling(&ip_key)?;

        // Credential endpoints are keyed by address even when a token is
        // present: the point is to limit guessing at *other people's*
        // credentials, and an attacker with any valid token could
        // otherwise exempt themselves from the strict quota.
        let key = if credential {
            ip_key
        } else {
            token_key(headers).unwrap_or_else(|| {
                client_key(headers, peer, self.inner.quotas().trusted_proxy_hops)
            })
        };
        let profile = if credential {
            Some(&StrictProfile::Credential)
        } else {
            None
        };
        self.inner.check_primary(profile, &key)
    }
}

/// Paths that must never be throttled.
///
/// Kubernetes probes this replica on a fixed interval regardless of load.
/// Rate limiting the liveness probe means a busy pod starts failing it,
/// gets restarted, sheds its traffic onto its peers, and pushes them over
/// the same limit — a limiter that converts a load spike into a rolling
/// outage. The probes are also cheap and unauthenticated by design, so
/// there is nothing to protect here.
fn is_exempt_path(path: &str) -> bool {
    path == "/healthz" || path == "/readyz"
}

/// Whether a path is a credential endpoint deserving the strict quota.
///
/// Password reset and email verification belong here for two separate
/// reasons. The REQUEST halves send mail to an address the caller names,
/// so an unthrottled one is a way to use Atlas to flood somebody's inbox
/// — and to burn the sending domain's reputation while doing it. The
/// CONFIRM halves take a token as the entire credential, which makes them
/// the one place in the API where guessing has a prize; 256 bits is not
/// brute-forceable, but a limiter is what keeps that true if the token
/// generator is ever weakened.
fn is_credential_path(path: &str) -> bool {
    // Matched on the concrete path rather than the route template because
    // the middleware runs before routing has resolved a MatchedPath.
    path.starts_with("/v1/auth/login")
        || path.starts_with("/v1/auth/register")
        || path.starts_with("/v1/auth/password-reset")
        || path.starts_with("/v1/auth/email/verify")
}

/// Address key: X-Forwarded-For read from the right by trusted hops.
pub fn client_key(headers: &HeaderMap, peer: Option<SocketAddr>, hops: usize) -> String {
    core::client_key(
        headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
        peer,
        hops,
    )
}

/// Token key: digest of the VALIDATED bearer (gateway shape).
fn token_key(headers: &HeaderMap) -> Option<String> {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = crate::validate::bearer_token(header).ok()?;
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
            let credential = is_credential_path(&path);
            metrics::counter!(
                "atlas_gateway_rate_limited_total",
                "scope" => if credential { "credential" } else { "ip_ceiling" },
            )
            .increment(1);
            too_many_requests(wait)
        }
    }
}

fn too_many_requests(wait: Duration) -> Response {
    // Same envelope and code the gateway uses for an upstream
    // ResourceExhausted, so an SDK branching on `code` needs no new case.
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
    fn health_probes_are_exempt() {
        assert!(is_exempt_path("/healthz"));
        assert!(is_exempt_path("/readyz"));
        // Nothing else gets a free pass — /metrics is on its own port and
        // never reaches this router.
        assert!(!is_exempt_path("/v1/auth/login"));
        assert!(!is_exempt_path("/v1/geo/nearby"));
    }

    #[test]
    fn credential_paths_are_recognised() {
        assert!(is_credential_path("/v1/auth/login"));
        assert!(is_credential_path("/v1/auth/register"));
        // Both halves of each flow: one mails a stranger, the other
        // accepts a token as the whole credential.
        assert!(is_credential_path("/v1/auth/password-reset"));
        assert!(is_credential_path("/v1/auth/password-reset/confirm"));
        assert!(is_credential_path("/v1/auth/email/verify"));
        assert!(is_credential_path("/v1/auth/email/verify/confirm"));
        assert!(!is_credential_path("/v1/auth/me"));
        assert!(!is_credential_path("/v1/geo/nearby"));
        assert!(!is_credential_path("/healthz"));
    }

    /// With no trusted proxies, a forged header must be ignored entirely.
    /// Honouring it would let an attacker mint a fresh bucket per request.
    #[test]
    fn forged_forwarded_for_is_ignored_when_no_proxy_is_trusted() {
        let h = headers(&[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(client_key(&h, peer("10.0.0.7:5000"), 0), "ip:10.0.0.7");
    }

    #[test]
    fn one_trusted_proxy_uses_the_address_it_saw() {
        // Client 203.0.113.9 -> our ingress. The ingress appends what it saw.
        let h = headers(&[("x-forwarded-for", "203.0.113.9")]);
        assert_eq!(client_key(&h, peer("10.0.0.7:5000"), 1), "ip:203.0.113.9");
    }

    /// The attack this defends against: the client prepends junk hoping
    /// the leftmost entry is used. Counting from the right ignores it.
    #[test]
    fn spoofed_entries_prepended_by_the_client_are_skipped() {
        let h = headers(&[("x-forwarded-for", "9.9.9.9, 8.8.8.8, 203.0.113.9")]);
        // One trusted proxy: the real client address is the last entry.
        assert_eq!(client_key(&h, peer("10.0.0.7:5000"), 1), "ip:203.0.113.9");
        // Two trusted proxies: one further left.
        assert_eq!(client_key(&h, peer("10.0.0.7:5000"), 2), "ip:8.8.8.8");
    }

    /// Fewer hops than configured means the request did not come through
    /// the proxies we expect, so nothing in the header is trustworthy.
    #[test]
    fn a_short_header_falls_back_to_the_peer_address() {
        let h = headers(&[("x-forwarded-for", "203.0.113.9")]);
        assert_eq!(client_key(&h, peer("10.0.0.7:5000"), 3), "ip:10.0.0.7");
    }

    #[test]
    fn a_garbage_header_falls_back_to_the_peer_address() {
        let h = headers(&[("x-forwarded-for", "not-an-ip")]);
        assert_eq!(client_key(&h, peer("10.0.0.7:5000"), 1), "ip:10.0.0.7");
    }

    #[test]
    fn tokens_are_hashed_not_stored_raw() {
        let h = headers(&[("authorization", "Bearer super.secret.token")]);
        let key = token_key(&h).expect("token key");
        assert!(key.starts_with("tok:"));
        assert!(!key.contains("super.secret.token"));
        // Same token, same bucket.
        assert_eq!(token_key(&h).unwrap(), key);
    }

    #[test]
    fn different_tokens_get_different_buckets() {
        let a = token_key(&headers(&[("authorization", "Bearer aaa.bbb.ccc")])).unwrap();
        let b = token_key(&headers(&[("authorization", "Bearer ddd.eee.fff")])).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_missing_or_malformed_authorization_header_has_no_token_key() {
        assert!(token_key(&HeaderMap::new()).is_none());
        assert!(token_key(&headers(&[("authorization", "Basic abc")])).is_none());
    }

    #[test]
    fn the_strict_quota_actually_stops_at_the_limit() {
        let limiters = Limiters::new(RateLimitConfig {
            ip_ceiling_per_minute: 6000,
            auth_per_minute: 3,
            ..RateLimitConfig::default()
        });
        let headers = HeaderMap::new();
        for i in 0..3 {
            assert!(
                limiters.check(None, &headers, "/v1/auth/login").is_ok(),
                "request {i} should pass"
            );
        }
        assert!(
            limiters.check(None, &headers, "/v1/auth/login").is_err(),
            "the fourth request must be rejected"
        );
    }

    #[test]
    fn the_default_quota_is_separate_from_the_credential_quota() {
        let limiters = Limiters::new(RateLimitConfig {
            auth_per_minute: 1,
            default_per_minute: 100,
            ip_ceiling_per_minute: 6000,
            ..RateLimitConfig::default()
        });
        let headers = HeaderMap::new();
        assert!(limiters.check(None, &headers, "/v1/auth/login").is_ok());
        assert!(
            limiters.check(None, &headers, "/v1/auth/login").is_err(),
            "credential quota spent"
        );
        // Exhausting credential attempts must not lock the client out of
        // ordinary reads.
        assert!(limiters.check(None, &headers, "/v1/geo/nearby").is_ok());
    }

    #[test]
    fn retry_after_is_at_least_one_second() {
        let response = too_many_requests(Duration::from_millis(1));
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get("retry-after").unwrap(), "1");
    }

    #[test]
    fn disabled_config_passes_everything_through() {
        let limiters = Limiters::new(RateLimitConfig {
            enabled: false,
            ..RateLimitConfig::default()
        });
        assert!(limiters
            .check(None, &HeaderMap::new(), "/v1/auth/login")
            .is_ok());
    }

    #[test]
    fn gc_does_not_panic_on_an_empty_limiter() {
        let limiters = Limiters::new(RateLimitConfig::default());
        limiters.inner.retain_recent();
    }

    /// The rotation attack the ceiling exists for: a fresh bearer string
    /// per request used to mean a fresh token bucket per request, so N
    /// garbage tokens from one address were unlimited. The ceiling caps
    /// the ADDRESS, which the attacker cannot rotate without the network
    /// cost of actually doing so.
    #[test]
    fn token_rotation_from_one_address_still_hits_the_ceiling() {
        let limiters = Limiters::new(RateLimitConfig {
            ip_ceiling_per_minute: 5,
            ..RateLimitConfig::default()
        });
        // Five rotating bearers: each gets a distinct token bucket, so
        // the default limiter never complains — but the ceiling does.
        for i in 0..5 {
            let headers = headers(&[("authorization", &format!("Bearer garbage-token-{i}"))]);
            assert!(limiters.check(None, &headers, "/v1/geo/nearby").is_ok());
        }
        // The sixth, with yet another fresh token, is refused by the
        // ceiling even though its token bucket is brand new.
        let headers = headers(&[("authorization", "Bearer garbage-token-6")]);
        assert!(
            limiters.check(None, &headers, "/v1/geo/nearby").is_err(),
            "the ceiling, not the token bucket, must stop rotation"
        );
    }
}

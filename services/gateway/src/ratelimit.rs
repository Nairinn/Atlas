//! Request rate limiting.
//!
//! # What this does and does not protect
//!
//! This is **per-replica**, in-process limiting. With three gateway pods a
//! configured 60/min is effectively 180/min globally, because a client's
//! requests are spread across replicas by the load balancer. That is
//! deliberate rather than overlooked: a globally exact limit needs shared
//! state (Redis) on the hot path of every request, which is a latency and
//! an availability cost this does not yet justify.
//!
//! Treat it as the layer that stops one client from exhausting one replica,
//! sitting underneath the ingress rate limit which is the real global cap.
//! Both are worth having — the ingress limit disappears the moment anything
//! reaches the service by another route, which is exactly the situation
//! in-process limiting covers.
//!
//! # Keying
//!
//! Authenticated requests are keyed by a digest of the bearer token, not by
//! IP. Behind carrier-grade NAT or a corporate egress, thousands of
//! legitimate users share one address, and an IP-keyed limit would throttle
//! all of them together while doing nothing about a single attacker
//! rotating addresses.
//!
//! Unauthenticated requests have nothing else to key on, so they use the
//! client address — see [`client_key`] for why that address is not simply
//! taken from `X-Forwarded-For`.

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use serde_json::json;
use sha2::{Digest, Sha256};

type Keyed = RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock>;

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Requests per minute for ordinary authenticated traffic.
    pub default_per_minute: u32,
    /// Requests per minute for credential endpoints (login, register).
    /// Much lower: these are the ones worth brute forcing.
    pub auth_per_minute: u32,
    /// Per-address requests per minute, applied to EVERY non-exempt request
    /// in addition to the primary bucket. This is the anti-rotation cap:
    /// without it a client minting a fresh bearer string per request gets
    /// a fresh token bucket each time. Set high enough that carrier-grade
    /// NAT users never hit it in normal traffic.
    pub ip_ceiling_per_minute: u32,
    /// How many proxies sit in front of this service. See [`client_key`].
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

pub struct Limiters {
    default: Keyed,
    auth: Keyed,
    /// A per-address cap that applies even to authenticated traffic, so a
    /// client rotating garbage bearer tokens cannot mint a fresh bucket per
    /// request from one address. Set far above the per-token quota so
    /// carrier-grade NAT users never feel it; only the rotation attack does.
    ip_ceiling: Keyed,
    config: RateLimitConfig,
}

impl Limiters {
    pub fn new(config: RateLimitConfig) -> Arc<Self> {
        let quota = |per_minute: u32| {
            let n = NonZeroU32::new(per_minute.max(1)).expect("nonzero");
            // Burst equal to the per-minute rate: GCRA refills continuously,
            // so this allows a short spike then settles to the steady rate,
            // which matches how a real client opening a screen behaves.
            Quota::per_minute(n).allow_burst(n)
        };
        Arc::new(Self {
            default: RateLimiter::keyed(quota(config.default_per_minute)),
            auth: RateLimiter::keyed(quota(config.auth_per_minute)),
            ip_ceiling: RateLimiter::keyed(quota(config.ip_ceiling_per_minute)),
            config,
        })
    }

    /// Drop buckets for keys that have not been seen recently.
    ///
    /// Without this the key map grows for every distinct IP or token
    /// forever, which turns the rate limiter itself into a memory
    /// exhaustion vector — the opposite of its job.
    pub fn retain_recent(&self) {
        self.default.retain_recent();
        self.auth.retain_recent();
        self.ip_ceiling.retain_recent();
    }

    /// Spawn the periodic sweep. Cheap: it walks only live keys.
    pub fn spawn_gc(self: &Arc<Self>) {
        let limiters = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                limiters.retain_recent();
            }
        });
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

/// Build the limiter key for a request.
///
/// # Why the client address is not just `X-Forwarded-For[0]`
///
/// `X-Forwarded-For` is client-supplied. Anyone can send
/// `X-Forwarded-For: 1.2.3.4` and, if the first entry is trusted, get a
/// fresh rate-limit bucket per forged value — which defeats the limiter
/// entirely and is a worse position than not having one, because it looks
/// protected.
///
/// The only safe read is to count back from the right by the number of
/// proxies you actually run: each appends the address it saw, so the
/// (hops+1)-th from the end is the last address a trusted proxy observed.
/// `trusted_proxy_hops = 0` means no proxy is trusted and the socket peer
/// address is used, which is correct for direct exposure and for tests.
pub fn client_key(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted_proxy_hops: usize,
) -> String {
    if trusted_proxy_hops > 0 {
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            let hops: Vec<&str> = xff
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            // hops.len() - trusted_proxy_hops is the index of the address the
            // outermost trusted proxy saw. A shorter list than expected means
            // the request did not traverse the proxies we think it did, so
            // fall through to the peer address rather than trusting it.
            if let Some(idx) = hops.len().checked_sub(trusted_proxy_hops) {
                if let Some(addr) = hops.get(idx) {
                    if let Ok(ip) = addr.parse::<IpAddr>() {
                        return format!("ip:{ip}");
                    }
                }
            }
        }
    }
    match peer {
        Some(addr) => format!("ip:{}", addr.ip()),
        // Only reachable when ConnectInfo is absent, which in practice means
        // a test harness driving the router directly.
        None => "ip:unknown".to_string(),
    }
}

/// Key authenticated requests by a digest of the token.
///
/// Hashed rather than used raw so a bearer token is not sitting in a map
/// key in memory, and truncated because 16 hex characters is ample to keep
/// buckets distinct.
fn token_key(headers: &HeaderMap) -> Option<String> {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = crate::validate::bearer_token(header).ok()?;
    let digest = Sha256::digest(token.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    Some(format!("tok:{hex}"))
}

pub async fn layer(
    State(limiters): State<Arc<Limiters>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    check(&limiters, Some(peer), req, next).await
}

/// Same as [`layer`] but without `ConnectInfo`, for the test harness and for
/// any deployment where the listener does not provide a peer address.
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
    if !limiters.config.enabled {
        return next.run(req).await;
    }

    let path = req.uri().path().to_string();
    if is_exempt_path(&path) {
        return next.run(req).await;
    }
    let credential = is_credential_path(&path);

    // The per-address ceiling applies to every non-exempt request. It is
    // checked second: the primary bucket names the abuser better (scope
    // label), but the ceiling is what actually caps a token-rotation
    // attack, which otherwise mints a fresh primary bucket per request.
    let ip_key = format!(
        "ip:{}",
        client_key(req.headers(), peer, limiters.config.trusted_proxy_hops)
    );
    if let Err(negative) = limiters.ip_ceiling.check_key(&ip_key) {
        let wait = negative.wait_time_from(DefaultClock::default().now());
        metrics::counter!(
            "atlas_gateway_rate_limited_total",
            "scope" => "ip_ceiling",
        )
        .increment(1);
        return too_many_requests(wait);
    }

    // Credential endpoints are keyed by address even when a token is
    // present: the point is to limit guessing at *other people's*
    // credentials, and an attacker with any valid token could otherwise
    // exempt themselves from the strict quota.
    let key = if credential {
        client_key(req.headers(), peer, limiters.config.trusted_proxy_hops)
    } else {
        token_key(req.headers())
            .unwrap_or_else(|| client_key(req.headers(), peer, limiters.config.trusted_proxy_hops))
    };

    let limiter = if credential {
        &limiters.auth
    } else {
        &limiters.default
    };

    match limiter.check_key(&key) {
        Ok(_) => next.run(req).await,
        Err(negative) => {
            let wait = negative.wait_time_from(DefaultClock::default().now());
            metrics::counter!(
                "atlas_gateway_rate_limited_total",
                "scope" => if credential { "credential" } else { "default" },
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

    /// The attack this defends against: the client prepends junk hoping the
    /// leftmost entry is used. Counting from the right ignores it.
    #[test]
    fn spoofed_entries_prepended_by_the_client_are_skipped() {
        let h = headers(&[("x-forwarded-for", "9.9.9.9, 8.8.8.8, 203.0.113.9")]);
        // One trusted proxy: the real client address is the last entry.
        assert_eq!(client_key(&h, peer("10.0.0.7:5000"), 1), "ip:203.0.113.9");
        // Two trusted proxies: one further left.
        assert_eq!(client_key(&h, peer("10.0.0.7:5000"), 2), "ip:8.8.8.8");
    }

    /// Fewer hops than configured means the request did not come through the
    /// proxies we expect, so nothing in the header is trustworthy.
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
        let key = "ip:198.51.100.1".to_string();
        for i in 0..3 {
            assert!(
                limiters.auth.check_key(&key).is_ok(),
                "request {i} should pass"
            );
        }
        assert!(
            limiters.auth.check_key(&key).is_err(),
            "the fourth request must be rejected"
        );
        // A different client is unaffected.
        assert!(limiters
            .auth
            .check_key(&"ip:198.51.100.2".to_string())
            .is_ok());
    }

    #[test]
    fn the_default_quota_is_separate_from_the_credential_quota() {
        let limiters = Limiters::new(RateLimitConfig {
            auth_per_minute: 1,
            default_per_minute: 100,
            ip_ceiling_per_minute: 6000,
            ..RateLimitConfig::default()
        });
        let key = "ip:198.51.100.3".to_string();
        assert!(limiters.auth.check_key(&key).is_ok());
        assert!(
            limiters.auth.check_key(&key).is_err(),
            "credential quota spent"
        );
        // Exhausting credential attempts must not lock the client out of
        // ordinary reads.
        assert!(limiters.default.check_key(&key).is_ok());
    }

    #[test]
    fn retry_after_is_at_least_one_second() {
        let response = too_many_requests(Duration::from_millis(1));
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get("retry-after").unwrap(), "1");
    }

    #[test]
    fn disabled_config_is_representable() {
        let limiters = Limiters::new(RateLimitConfig {
            enabled: false,
            ..RateLimitConfig::default()
        });
        assert!(!limiters.config.enabled);
    }

    #[test]
    fn gc_does_not_panic_on_an_empty_limiter() {
        let limiters = Limiters::new(RateLimitConfig::default());
        limiters.retain_recent();
        let _ = DefaultClock::default().now();
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
        let ip_key = "ip:203.0.113.9".to_string();
        // Five rotating bearers: each gets a distinct token bucket, so
        // the default limiter never complains — but the ceiling does.
        for i in 0..5 {
            let headers = headers(&[("authorization", &format!("Bearer garbage-token-{i}"))]);
            let token = token_key(&headers).unwrap();
            assert!(limiters.default.check_key(&token).is_ok());
            assert!(limiters.ip_ceiling.check_key(&ip_key).is_ok());
        }
        // The sixth, with yet another fresh token, is refused by the
        // ceiling even though its token bucket is brand new.
        let headers = headers(&[("authorization", "Bearer garbage-token-6")]);
        let token = token_key(&headers).unwrap();
        assert!(limiters.default.check_key(&token).is_ok());
        assert!(
            limiters.ip_ceiling.check_key(&ip_key).is_err(),
            "the ceiling, not the token bucket, must stop rotation"
        );
    }
}

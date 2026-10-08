//! Request rate limiting for the control plane.
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
//! # The endpoint this exists for
//!
//! `POST /v1/accounts` is the only unauthenticated write in the platform
//! and it mints an API key. Unthrottled, anyone who can reach the port can
//! create unlimited accounts and credentials. Its quota is deliberately
//! tiny — signing up is a once-per-person action, not an API call.
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
    /// Requests per minute for account creation, per client address.
    /// Deliberately tiny: signing up is a once-per-person action.
    pub signup_per_minute: u32,
    /// Per-address requests per minute applied to every non-exempt
    /// request. The anti-rotation cap: without it a client minting fresh
    /// bearer strings gets a fresh token bucket per request.
    pub ip_ceiling_per_minute: u32,
    /// How many proxies sit in front of this service. See [`client_key`].
    pub trusted_proxy_hops: usize,
    pub enabled: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            // The CLI makes a handful of calls per invocation, so this is
            // generous for a human and still bounds a runaway script.
            default_per_minute: 120,
            // Three signups a minute from one address. A person creating a
            // second account for a colleague is fine; a script is not.
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

pub struct Limiters {
    default: Keyed,
    signup: Keyed,
    /// Per-address cap on every non-exempt request; see the config field.
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
            signup: RateLimiter::keyed(quota(config.signup_per_minute)),
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
        self.signup.retain_recent();
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

/// Whether a path creates an account, and so deserves the strict quota.
fn is_signup_path(path: &str) -> bool {
    // Matched on the concrete path rather than the route template because
    // the middleware runs before routing has resolved a MatchedPath.
    path == "/v1/accounts"
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

/// Key authenticated requests by a digest of the API key.
///
/// Hashed rather than used raw so a live credential is not sitting in a map
/// key in memory, and truncated because 16 hex characters is ample to keep
/// buckets distinct.
fn token_key(headers: &HeaderMap) -> Option<String> {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = crate::auth::parse_bearer(header).ok()?;
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
    let signup = is_signup_path(&path);

    // Per-address ceiling on every non-exempt request, so rotating bearer
    // strings cannot mint fresh token buckets. Checked before the
    // primary bucket for the same reason as the gateway.
    let ip_key = format!(
        "ip:{}",
        client_key(req.headers(), peer, limiters.config.trusted_proxy_hops)
    );
    if let Err(negative) = limiters.ip_ceiling.check_key(&ip_key) {
        let wait = negative.wait_time_from(DefaultClock::default().now());
        metrics::counter!(
            "atlas_control_plane_rate_limited_total",
            "scope" => "ip_ceiling",
        )
        .increment(1);
        return too_many_requests(wait);
    }

    // Signup has no credential to key on by definition, so it is keyed by
    // address. Everything else prefers the API key: control-plane callers
    // are CLIs and CI runners that frequently share an egress address, and
    // keying those by IP would throttle a whole office together.
    let key = if signup {
        client_key(req.headers(), peer, limiters.config.trusted_proxy_hops)
    } else {
        token_key(req.headers())
            .unwrap_or_else(|| client_key(req.headers(), peer, limiters.config.trusted_proxy_hops))
    };

    let limiter = if signup {
        &limiters.signup
    } else {
        &limiters.default
    };

    match limiter.check_key(&key) {
        Ok(_) => next.run(req).await,
        Err(negative) => {
            let wait = negative.wait_time_from(DefaultClock::default().now());
            metrics::counter!(
                "atlas_control_plane_rate_limited_total",
                "scope" => if signup { "signup" } else { "default" },
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

    /// With no trusted proxies a forged header must be ignored: honouring it
    /// would hand an attacker a fresh signup bucket per request.
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
        let key = "ip:198.51.100.1".to_string();
        assert!(limiters.signup.check_key(&key).is_ok());
        assert!(limiters.signup.check_key(&key).is_ok());
        assert!(
            limiters.signup.check_key(&key).is_err(),
            "third signup from one address must be rejected"
        );
        // A different address is unaffected.
        assert!(limiters
            .signup
            .check_key(&"ip:198.51.100.2".to_string())
            .is_ok());
    }

    #[test]
    fn exhausting_signups_does_not_block_ordinary_api_calls() {
        let limiters = Limiters::new(RateLimitConfig {
            signup_per_minute: 1,
            default_per_minute: 100,
            ..RateLimitConfig::default()
        });
        let key = "ip:198.51.100.3".to_string();
        assert!(limiters.signup.check_key(&key).is_ok());
        assert!(limiters.signup.check_key(&key).is_err());
        assert!(limiters.default.check_key(&key).is_ok());
    }

    #[test]
    fn retry_after_is_at_least_one_second() {
        let response = too_many_requests(Duration::from_millis(1));
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get("retry-after").unwrap(), "1");
    }
}

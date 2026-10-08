//! Environment-driven configuration.
//!
//! Defaults point at the docker-compose topology so `cargo run -p
//! atlas-gateway` works against a local stack with no env vars set.
//! Production overrides everything through the container environment.

use std::env;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    /// Public REST listener. This is the only port exposed to the internet.
    pub http_addr: SocketAddr,
    /// Prometheus scrape listener. Kept on a separate port so `/metrics`
    /// is never reachable from the public interface.
    pub metrics_addr: SocketAddr,
    pub auth_addr: String,
    pub geo_addr: String,
    pub payments_addr: String,
    /// Per-RPC deadline applied to every upstream call. Without this a
    /// wedged backend would hold gateway connections until the client
    /// gives up.
    pub upstream_timeout: Duration,
    /// TCP connect timeout for a lazily-established upstream channel.
    pub upstream_connect_timeout: Duration,

    /// Read-only connection to the control-plane database, used to resolve
    /// a project key to its project. See `tenant.rs` for why the gateway
    /// reads this table rather than calling the control plane.
    pub database_url: String,
    pub database_pool_size: u32,
    /// How long a resolved (or rejected) project key stays cached. This is
    /// also the revocation window: `atlas keys revoke` takes effect within
    /// it, not instantly.
    pub project_cache_ttl: Duration,

    /// Per-replica request limiting. See `ratelimit.rs` for what this does
    /// and does not protect.
    pub rate_limit: crate::ratelimit::RateLimitConfig,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            http_addr: parse_addr("HTTP_ADDR", "0.0.0.0:8080"),
            metrics_addr: parse_addr("METRICS_ADDR", "0.0.0.0:9090"),
            auth_addr: env::var("AUTH_SERVICE_ADDR")
                .unwrap_or_else(|_| "http://localhost:50051".to_string()),
            geo_addr: env::var("GEO_ENGINE_ADDR")
                .unwrap_or_else(|_| "http://localhost:50052".to_string()),
            payments_addr: env::var("PAYMENTS_SERVICE_ADDR")
                .unwrap_or_else(|_| "http://localhost:50053".to_string()),
            upstream_timeout: parse_secs("UPSTREAM_TIMEOUT_SECONDS", 10),
            upstream_connect_timeout: parse_secs("UPSTREAM_CONNECT_TIMEOUT_SECONDS", 5),
            database_url: env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgres://atlas:atlas_dev@localhost:5432/atlas".to_string()),
            database_pool_size: parse_u32("DATABASE_POOL_SIZE", 8),
            // 30s to match the token validation cache in auth-service, so
            // the two credentials on a request go stale on the same clock.
            project_cache_ttl: parse_secs("PROJECT_CACHE_TTL_SECONDS", 30),
            rate_limit: crate::ratelimit::RateLimitConfig {
                default_per_minute: parse_u32("RATE_LIMIT_PER_MINUTE", 600),
                auth_per_minute: parse_u32("RATE_LIMIT_AUTH_PER_MINUTE", 10),
                // The per-address ceiling; see ratelimit.rs for why it
                // exists. High default: CGNAT users must not feel it.
                ip_ceiling_per_minute: parse_u32("RATE_LIMIT_IP_CEILING_PER_MINUTE", 6000),
                // Defaults to 0: trust nothing in X-Forwarded-For unless the
                // operator states how many proxies actually sit in front.
                // Behind the ingress-nginx in infra/k8s this is 1.
                trusted_proxy_hops: parse_u32("TRUSTED_PROXY_HOPS", 0) as usize,
                enabled: env::var("RATE_LIMIT_ENABLED")
                    .map(|v| v != "false" && v != "0")
                    .unwrap_or(true),
            },
        }
    }
}

fn parse_addr(var: &str, default: &str) -> SocketAddr {
    env::var(var)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .unwrap_or_else(|_| panic!("{var} must be a valid socket address"))
}

/// Parse a required-integer env var, failing at startup on garbage.
///
/// Falling back to the default on a malformed value would silently run
/// with config the operator believes they set: a typo like
/// `RATE_LIMIT_PER_MINUTE=6OO` (letter O) must stop the deploy, not
/// quietly run at 600.
fn parse_u32(var: &str, default: u32) -> u32 {
    match env::var(var) {
        Ok(v) => v
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("{var} must be an integer, got {v:?}")),
        Err(_) => default,
    }
}

fn parse_secs(var: &str, default: u64) -> Duration {
    Duration::from_secs(parse_u64(var, default))
}

fn parse_u64(var: &str, default: u64) -> u64 {
    match env::var(var) {
        Ok(v) => v
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("{var} must be an integer, got {v:?}")),
        Err(_) => default,
    }
}

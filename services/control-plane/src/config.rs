//! Environment-driven configuration.
//!
//! Defaults match the docker-compose topology. The CLI's
//! `DEFAULT_BASE_URL` is `http://localhost:8081/v1`, so the HTTP port
//! here is 8081 and every route is mounted under `/v1`.

use std::env;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub http_addr: SocketAddr,
    pub metrics_addr: SocketAddr,
    pub database_url: String,
    pub database_pool_size: u32,

    /// gRPC addresses probed by `GET /projects/:name/status`, in
    /// `host:port` form, no scheme: these are dialled as HTTP/2 origins.
    pub auth_addr: String,
    pub geo_addr: String,
    pub payments_addr: String,
    /// Kafka bootstrap servers. The `events` namespace has no gRPC health
    /// service, so its health is a TCP reachability check against these.
    pub kafka_brokers: String,
    /// Where to scrape real request counters and latency quantiles from.
    pub gateway_metrics_url: String,

    /// Public URL template for a provisioned project's endpoint. `{name}`
    /// is substituted with the project name.
    pub endpoint_template: String,

    pub probe_timeout: Duration,

    /// Per-replica request limiting. Chiefly guards POST /v1/accounts.
    pub rate_limit: crate::ratelimit::RateLimitConfig,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            http_addr: parse_addr("HTTP_ADDR", "0.0.0.0:8081"),
            metrics_addr: parse_addr("METRICS_ADDR", "0.0.0.0:9090"),
            database_url: env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgres://atlas:atlas_dev@localhost:5432/atlas".to_string()),
            database_pool_size: env::var("DATABASE_POOL_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            auth_addr: env::var("AUTH_SERVICE_ADDR")
                .unwrap_or_else(|_| "http://localhost:50051".to_string()),
            geo_addr: env::var("GEO_ENGINE_ADDR")
                .unwrap_or_else(|_| "http://localhost:50052".to_string()),
            payments_addr: env::var("PAYMENTS_SERVICE_ADDR")
                .unwrap_or_else(|_| "http://localhost:50053".to_string()),
            kafka_brokers: env::var("KAFKA_BROKERS")
                .unwrap_or_else(|_| "localhost:9092".to_string()),
            gateway_metrics_url: env::var("GATEWAY_METRICS_URL")
                .unwrap_or_else(|_| "http://localhost:9090/metrics".to_string()),
            endpoint_template: env::var("ENDPOINT_TEMPLATE")
                .unwrap_or_else(|_| "https://api.atlas.dev/v1/{name}".to_string()),
            probe_timeout: Duration::from_millis(
                env::var("PROBE_TIMEOUT_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(2_000),
            ),
            rate_limit: crate::ratelimit::RateLimitConfig {
                default_per_minute: parse_u32("RATE_LIMIT_PER_MINUTE", 120),
                signup_per_minute: parse_u32("RATE_LIMIT_SIGNUP_PER_MINUTE", 3),
                // Per-address ceiling; see ratelimit.rs for why it exists.
                ip_ceiling_per_minute: parse_u32("RATE_LIMIT_IP_CEILING_PER_MINUTE", 1200),
                // 0 = trust nothing in X-Forwarded-For. Behind the
                // ingress-nginx in infra/k8s this is 1.
                trusted_proxy_hops: parse_u32("TRUSTED_PROXY_HOPS", 0) as usize,
                enabled: env::var("RATE_LIMIT_ENABLED")
                    .map(|v| v != "false" && v != "0")
                    .unwrap_or(true),
            },
        }
    }

    pub fn endpoint_for(&self, project_name: &str) -> String {
        self.endpoint_template.replace("{name}", project_name)
    }
}

/// Parse an integer env var, failing at startup on garbage rather than
/// falling back: a typo'd value must stop the deploy, not quietly run
/// with config the operator believes they set.
fn parse_u32(var: &str, default: u32) -> u32 {
    match env::var(var) {
        Ok(v) => v
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("{var} must be an integer, got {v:?}")),
        Err(_) => default,
    }
}

fn parse_addr(var: &str, default: &str) -> SocketAddr {
    env::var(var)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .unwrap_or_else(|_| panic!("{var} must be a valid socket address"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            http_addr: "0.0.0.0:8081".parse().unwrap(),
            metrics_addr: "0.0.0.0:9090".parse().unwrap(),
            database_url: String::new(),
            database_pool_size: 1,
            auth_addr: String::new(),
            geo_addr: String::new(),
            payments_addr: String::new(),
            kafka_brokers: String::new(),
            gateway_metrics_url: String::new(),
            endpoint_template: "https://api.atlas.dev/v1/{name}".to_string(),
            probe_timeout: Duration::from_millis(1),
            rate_limit: crate::ratelimit::RateLimitConfig::default(),
        }
    }

    #[test]
    fn endpoint_substitutes_the_project_name() {
        assert_eq!(
            cfg().endpoint_for("my-mobility-app"),
            "https://api.atlas.dev/v1/my-mobility-app"
        );
    }
}

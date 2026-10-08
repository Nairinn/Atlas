//! Environment-driven configuration, with defaults matching the
//! docker-compose topology.

use std::env;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub database_pool_size: u32,
    pub kafka_brokers: String,
    pub topic: String,
    /// Consumer group id. Unlike auth-service's cache-invalidation
    /// consumer (which uses a unique group per instance so every instance
    /// sees every event), this is a *shared* group: the TTL sweep and
    /// ingest metrics want each message handled once, so partitions
    /// should be divided across instances, not broadcast to all of them.
    pub consumer_group: String,
    pub metrics_addr: SocketAddr,
    /// How often the reaper runs. The TTL is 24h, so sweeping every few
    /// minutes is plenty — this exists to bound table growth, not to
    /// delete rows the instant they expire.
    pub sweep_interval: Duration,
    /// Rows deleted per statement. Batching keeps each DELETE's lock
    /// footprint small so the reaper never blocks the ingest path.
    pub sweep_batch_size: i64,
    /// Safety votes older than this are pruned. Votes are opinions about a
    /// place, and a place at 2am is not the place at 2pm: a year of them is
    /// plenty of signal. Default 365 days.
    pub safety_vote_horizon: Duration,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            database_url: env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgres://atlas:atlas_dev@localhost:5432/atlas".to_string()),
            database_pool_size: parse_or("DATABASE_POOL_SIZE", 5),
            kafka_brokers: env::var("KAFKA_BROKERS")
                .unwrap_or_else(|_| "localhost:9092".to_string()),
            topic: env::var("KAFKA_TOPIC_LOCATION_UPDATES")
                .unwrap_or_else(|_| "atlas.location.updates".to_string()),
            consumer_group: env::var("CONSUMER_GROUP")
                .unwrap_or_else(|_| "atlas-location-consumer".to_string()),
            metrics_addr: env::var("METRICS_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:9090".to_string())
                .parse()
                .expect("METRICS_ADDR must be a valid socket address"),
            sweep_interval: Duration::from_secs(parse_or("SWEEP_INTERVAL_SECONDS", 300)),
            sweep_batch_size: parse_or("SWEEP_BATCH_SIZE", 5_000),
            safety_vote_horizon: Duration::from_secs(parse_or(
                "SAFETY_VOTE_HORIZON_SECONDS",
                365 * 24 * 3600,
            )),
        }
    }
}

fn parse_or<T: std::str::FromStr>(var: &str, default: T) -> T {
    env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

//! Shared edge-service plumbing for the gateway and the control plane.
//!
//! Both services run the same shape of HTTP edge: a Prometheus exporter on
//! its own listener, a SIGTERM-aware shutdown, env-var config with
//! fail-fast parsing, and a GCRA rate limiter keyed on client address
//! plus a token digest. The per-service differences are exactly the parts
//! NOT in this crate: which quotas exist, which paths are strict, and
//! what the error envelope looks like on a 429.

pub mod env;
pub mod metrics;
pub mod ratelimit;
pub mod shutdown;

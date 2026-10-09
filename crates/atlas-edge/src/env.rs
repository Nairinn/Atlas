//! Fail-fast environment parsing, shared by the edge services.
//!
//! A malformed `RATE_LIMIT_PER_MINUTE=abc` used to fall back to the
//! default silently, which meant an operator who typo'd a value in a
//! deploy had no limit at all and no signal. These helpers panic in
//! `Config::from_env` before the listener binds, so the deploy fails
//! loudly instead of running with config nobody set.

/// Parse a `u32` env var. Missing var: the default. Present but
/// malformed: panic, naming the var.
pub fn parse_u32(var: &str, default: u32) -> u32 {
    match std::env::var(var) {
        Ok(v) => v
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("{var} must be an integer, got {v:?}")),
        Err(_) => default,
    }
}

/// Parse a `u64` env var with the same missing-vs-malformed contract.
pub fn parse_u64(var: &str, default: u64) -> u64 {
    match std::env::var(var) {
        Ok(v) => v
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("{var} must be an integer, got {v:?}")),
        Err(_) => default,
    }
}

/// Parse a `Duration` in seconds from a `u64` env var.
pub fn parse_secs(var: &str, default: u64) -> std::time::Duration {
    std::time::Duration::from_secs(parse_u64(var, default))
}

/// Parse a `SocketAddr` env var, panicking on a malformed value.
pub fn parse_addr(var: &str, default: &str) -> std::net::SocketAddr {
    std::env::var(var)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .unwrap_or_else(|_| panic!("{var} must be a valid socket address"))
}

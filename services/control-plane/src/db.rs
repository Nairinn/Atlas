//! Postgres pool.
//!
//! The control plane is low-traffic compared to the data-plane services —
//! it sees CLI invocations, not user requests — so the default pool is
//! deliberately smaller than geo-engine's 20.

use anyhow::Context;
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

pub async fn connect(database_url: &str, pool_size: u32) -> anyhow::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(pool_size)
        .acquire_timeout(Duration::from_secs(5))
        .connect(database_url)
        .await
        .with_context(|| format!("connecting to postgres at {}", redact_url(database_url)))
}

/// The URL minus userinfo: the connect error is logged, and a
/// `postgres://user:pass@host` string in a log line is the credential in
/// the one place with the weakest access control.
fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://").map(|i| i + 3) {
        let rest = &url[scheme_end..];
        if let Some(at) = rest.find('@') {
            return format!("{}***{}", &url[..scheme_end], &rest[at..]);
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_never_appears_in_the_error_context() {
        let url = "postgres://atlas:supersecret@localhost:5432/atlas";
        let redacted = redact_url(url);
        assert_eq!(redacted, "postgres://***@localhost:5432/atlas");
        assert!(!redacted.contains("supersecret"));

        // No userinfo at all: unchanged.
        assert_eq!(
            redact_url("postgres://localhost/atlas"),
            "postgres://localhost/atlas"
        );
    }
}

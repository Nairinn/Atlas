//! The retention sweeps: `geo.locations`, `geo.safety_votes`, and stale
//! `geo.geofence_memberships`.
//!
//! Migration 0020 gives every location row a 24h `expires_at`, and
//! `nearby` already filters expired rows out of results; this deletes
//! them for real, which is what keeps the GIST index and the table from
//! growing without bound.
//!
//! Safety votes carry no `expires_at` — they are opinions, not pings — so
//! their horizon is a configured age. Memberships have no TTL either: a
//! membership whose user has no unexpired location is a user who stopped
//! pinging, and holding their "inside" state forever keeps a fence alert
//! from ever being wrong only by never being able to fire again.

use sqlx::PgPool;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Delete one batch of expired rows. Returns how many went.
///
/// The `ctid IN (SELECT ... LIMIT n)` shape is deliberate. A bare
/// `DELETE ... WHERE expires_at < NOW()` would take row locks on every
/// expired row in one transaction — after an outage backlog that can be
/// millions of rows, holding locks and bloating WAL while the ingest path
/// waits. Batching by physical row id bounds each transaction instead.
pub async fn sweep_once(pool: &PgPool, batch_size: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"
        DELETE FROM geo.locations
        WHERE ctid IN (
            SELECT ctid
            FROM geo.locations
            WHERE expires_at < NOW()
            LIMIT $1
        )
        "#,
    )
    .bind(batch_size)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Sweep until nothing expired remains, then return the total deleted.
///
/// Capped at `max_batches` so a pathological backlog cannot monopolise
/// the task forever — whatever is left is picked up on the next tick.
pub async fn sweep_until_clear(
    pool: &PgPool,
    batch_size: i64,
    max_batches: u32,
) -> Result<u64, sqlx::Error> {
    let mut total = 0;
    for _ in 0..max_batches {
        let deleted = sweep_once(pool, batch_size).await?;
        total += deleted;
        // A short batch means the backlog is drained.
        if (deleted as i64) < batch_size {
            break;
        }
    }
    Ok(total)
}

/// One pass of the three retention sweeps. Each prune is counted on its
/// own metric so a wedge in one does not hide the others.
pub struct Reaped {
    pub locations: u64,
    pub votes: u64,
    pub memberships: u64,
}

/// Prune safety votes older than the horizon. Batched like locations.
pub async fn sweep_votes_once(
    pool: &PgPool,
    horizon: Duration,
    batch_size: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"
        DELETE FROM geo.safety_votes
        WHERE ctid IN (
            SELECT ctid
            FROM geo.safety_votes
            WHERE created_at < NOW() - ($1::bigint * INTERVAL '1 second')
            LIMIT $2
        )
        "#,
    )
    .bind(horizon.as_secs() as i64)
    .bind(batch_size)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Drop memberships whose user has no unexpired location row: the user
/// stopped pinging, so "inside fence X" is stale state that blocks nothing
/// but can mislead a later diff if the user reappears inside the same
/// fence — a fresh ENTERED is the correct signal then, and the stale row
/// would suppress it.
pub async fn sweep_memberships_once(pool: &PgPool, batch_size: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"
        DELETE FROM geo.geofence_memberships
        WHERE ctid IN (
            SELECT m.ctid
            FROM geo.geofence_memberships m
            WHERE NOT EXISTS (
                SELECT 1 FROM geo.locations l
                WHERE l.project_id = m.project_id
                  AND l.user_id = m.user_id
                  AND l.expires_at > NOW()
            )
            LIMIT $1
        )
        "#,
    )
    .bind(batch_size)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Run the sweeps on a timer until cancelled.
pub async fn run(pool: PgPool, interval: Duration, batch_size: i64, vote_horizon: Duration) {
    const MAX_BATCHES_PER_TICK: u32 = 100;
    let mut ticker = tokio::time::interval(interval);
    // The first tick fires immediately; that is wanted, so a restart after
    // a long outage starts draining rather than waiting out a full period.
    loop {
        ticker.tick().await;
        match sweep_all(&pool, batch_size, MAX_BATCHES_PER_TICK, vote_horizon).await {
            Ok(reaped) => {
                if reaped.locations > 0 {
                    metrics::counter!("atlas_location_rows_reaped_total", "table" => "locations")
                        .increment(reaped.locations);
                }
                if reaped.votes > 0 {
                    metrics::counter!("atlas_location_rows_reaped_total", "table" => "safety_votes")
                        .increment(reaped.votes);
                }
                if reaped.memberships > 0 {
                    metrics::counter!("atlas_location_rows_reaped_total", "table" => "memberships")
                        .increment(reaped.memberships);
                }
                if reaped.locations + reaped.votes + reaped.memberships == 0 {
                    debug!("retention sweep: nothing to prune");
                } else {
                    info!(
                        locations = reaped.locations,
                        votes = reaped.votes,
                        memberships = reaped.memberships,
                        "retention sweeps complete"
                    );
                }
            }
            Err(e) => {
                // A failed sweep must not kill the task — the next tick
                // retries, and rows stay queryable-but-filtered meanwhile.
                metrics::counter!("atlas_location_reap_errors_total").increment(1);
                warn!(error = %e, "retention sweep failed; will retry next tick");
            }
        }
    }
}

/// All three sweeps, each batched until drained or capped.
pub async fn sweep_all(
    pool: &PgPool,
    batch_size: i64,
    max_batches: u32,
    vote_horizon: Duration,
) -> Result<Reaped, sqlx::Error> {
    let mut locations = 0;
    let mut votes = 0;
    let mut memberships = 0;
    for _ in 0..max_batches {
        let n = sweep_once(pool, batch_size).await?;
        locations += n;
        if (n as i64) < batch_size {
            break;
        }
    }
    for _ in 0..max_batches {
        let n = sweep_votes_once(pool, vote_horizon, batch_size).await?;
        votes += n;
        if (n as i64) < batch_size {
            break;
        }
    }
    for _ in 0..max_batches {
        let n = sweep_memberships_once(pool, batch_size).await?;
        memberships += n;
        if (n as i64) < batch_size {
            break;
        }
    }
    Ok(Reaped {
        locations,
        votes,
        memberships,
    })
}

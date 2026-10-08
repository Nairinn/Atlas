//! Location ping inserts and proximity queries.
//!
//! Why these queries use `query()` / `query_as()` (the runtime path) rather
//! than `query!()` / `query_as!()` (compile-time checked against a live DB):
//! the compile-time variants require `DATABASE_URL` to be set at build
//! time, which breaks CI and offline builds. The runtime path keeps the
//! crate buildable without a running Postgres.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// Insert a location ping.
///
/// `expires_at` is `recorded_at + 24h`, set explicitly rather than left to
/// the column default (`NOW() + 24h`): retention must start when the fix
/// was taken, not when the server happened to receive it — a batched
/// upload of hour-old pings would otherwise live an hour longer than the
/// retention window says. The service layer clamps `recorded_at` into the
/// window, so the arithmetic here cannot resurrect an already-expired row.
pub async fn insert_location(
    pool: &PgPool,
    project_id: Uuid,
    user_id: Uuid,
    lat: f64,
    lng: f64,
    recorded_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO geo.locations (project_id, user_id, position, recorded_at, expires_at)
        VALUES (
            $5,
            $1,
            ST_SetSRID(ST_MakePoint($2, $3), 4326),
            $4,
            $4 + INTERVAL '24 hours'
        )
        "#,
    )
    .bind(user_id)
    // PostGIS ST_MakePoint takes (lng, lat) — easy to flip; we keep the
    // bind order matching the SQL parameter order for readability.
    .bind(lng)
    .bind(lat)
    .bind(recorded_at)
    .bind(project_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub struct NearbyRow {
    pub user_id: Uuid,
    pub lng: f64,
    pub lat: f64,
    pub distance_m: f64,
    pub safety_score: f64,
    /// Distinct voters behind `safety_score`. Zero means the score is the
    /// neutral default rather than a measurement, and a caller that cannot
    /// tell those apart will present the first as if it were the second.
    pub safety_vote_count: i64,
}

/// Find users near (lat, lng) within `radius_m` meters, excluding the
/// requester, each with the safety score of the place they are standing.
///
/// The score comes from `geo.safety_votes` — each voter's most recent
/// verdict within 200m, smoothed toward neutral by `geo.safety_score`.
/// Somewhere nobody has voted scores exactly 1500, which is what this
/// returned for everyone before votes existed.
///
/// # Why the project filter is load-bearing here specifically
///
/// Most queries in Atlas reach a row through a user id, and user ids are
/// UUIDs — so even unscoped, they could not accidentally return another
/// tenant's data. This one is different: it searches by POSITION. Without
/// `l.project_id = $6` it returns every Atlas user near that point no
/// matter whose application asked, which is a cross-tenant read of people's
/// locations. It was exactly that until this filter existed.
///
/// The safety_votes join is scoped for the same reason: the scores are
/// derived from one tenant's users' votes, and letting another tenant's
/// votes move the number would leak behaviour across the boundary.
///
/// # Why every spatial predicate casts to `::geography`
///
/// `geo.locations.position` is `GEOMETRY(Point, 4326)`. On a *geometry*,
/// `ST_DWithin` and `ST_Distance` work in the SRID's own units, and 4326's
/// unit is the **degree** — so `ST_DWithin(a, b, 250)` asks "within 250
/// degrees", roughly 27,000 km, and matches every row on Earth. Casting
/// both operands to `geography` switches PostGIS to spheroidal maths in
/// **meters**, which is what the `_m` suffix has always claimed.
pub async fn nearby(
    pool: &PgPool,
    project_id: Uuid,
    lat: f64,
    lng: f64,
    radius_m: f64,
    requester_user_id: Uuid,
    limit: i64,
) -> Result<Vec<NearbyRow>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (Uuid, f64, f64, f64, f64, i64)>(
        r#"
        SELECT
            l.user_id,
            ST_X(l.position::geometry) AS lng,
            ST_Y(l.position::geometry) AS lat,
            ST_Distance(
                l.position::geography,
                ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography
            ) AS distance_m,
            s.score AS safety_score,
            s.vote_count AS safety_vote_count
        FROM geo.locations l
        -- LATERAL so each row is scored at ITS OWN position rather than at
        -- the query point: two users 400m apart are in different places,
        -- and giving them one shared score would be a different (and
        -- wrong) answer.
        --
        -- DISTINCT ON collapses each voter to their latest verdict before
        -- counting, so re-voting corrects rather than accumulates.
        LEFT JOIN LATERAL (
            WITH latest AS (
                SELECT DISTINCT ON (sv.user_id) sv.user_id, sv.vote
                FROM geo.safety_votes sv
                WHERE sv.project_id = $6
                  AND ST_DWithin(sv.position::geography, l.position::geography, 200)
                ORDER BY sv.user_id, sv.created_at DESC
            )
            SELECT
                geo.safety_score(
                    COUNT(*) FILTER (WHERE vote = 'safe'),
                    COUNT(*) FILTER (WHERE vote = 'unsafe')
                ) AS score,
                COUNT(*)::bigint AS vote_count
            FROM latest
        ) s ON TRUE
        WHERE
            l.project_id = $6
            AND ST_DWithin(
                l.position::geography,
                ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography,
                $3
            )
            AND l.expires_at > NOW()
            AND l.user_id != $4
        ORDER BY distance_m ASC
        LIMIT $5
        "#,
    )
    .bind(lng)
    .bind(lat)
    .bind(radius_m)
    .bind(requester_user_id)
    .bind(limit)
    .bind(project_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(user_id, lng, lat, distance_m, safety_score, safety_vote_count)| NearbyRow {
                user_id,
                lng,
                lat,
                distance_m,
                safety_score,
                safety_vote_count,
            },
        )
        .collect())
}

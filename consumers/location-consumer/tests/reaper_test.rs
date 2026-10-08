//! Reaper behaviour against a real database.
//!
//! Marked `#[ignore]` so `cargo test` stays green without a database. Run
//! with:
//!
//!     docker compose up -d postgres
//!     cargo run -p atlas-migrator -- run
//!     cargo test -p atlas-location-consumer -- --include-ignored
//!
//! The unit-level batching logic is trivial; what these tests actually pin
//! is the SQL: that `sweep_once` deletes only expired rows, that
//! `sweep_votes_once` respects the horizon and nothing else, and that
//! `sweep_memberships_once` drops only memberships whose user has no live
//! location. A reaper bug deletes people's data, so the WHERE clauses
//! deserve tests of their own.

use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use uuid::Uuid;

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("ATLAS_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://atlas:atlas_dev@localhost:5432/atlas".to_string());
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("test database (start postgres + run the migrator)")
}

/// A project + user pair with schema-satisfying rows, so each test writes
/// real FK-resolvable data.
async fn fixture(pool: &sqlx::PgPool) -> (Uuid, Uuid) {
    let account: Uuid =
        sqlx::query_scalar("INSERT INTO control.accounts (email) VALUES ($1) RETURNING id")
            .bind(format!("reaper-{}@atlas.test", Uuid::new_v4()))
            .fetch_one(pool)
            .await
            .unwrap();
    let project: Uuid = sqlx::query_scalar(
        r#"INSERT INTO control.projects (account_id, name, region, environment, endpoint)
           VALUES ($1, $2, 'us-central1', 'development', '')
           RETURNING id"#,
    )
    .bind(account)
    .bind(format!("reaper-{}", Uuid::new_v4().simple()))
    .fetch_one(pool)
    .await
    .unwrap();
    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (project_id, email, password_hash) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(project)
    .bind(format!("u-{}@atlas.test", Uuid::new_v4()))
    .bind("x")
    .fetch_one(pool)
    .await
    .unwrap();
    (project, user)
}

async fn insert_location(pool: &sqlx::PgPool, project: Uuid, user: Uuid, expires_at: &str) {
    sqlx::query(
        r#"
        INSERT INTO geo.locations (project_id, user_id, position, recorded_at, expires_at)
        VALUES ($1, $2, ST_SetSRID(ST_MakePoint(1, 2), 4326), NOW(), $3::timestamptz)
        "#,
    )
    .bind(project)
    .bind(user)
    .bind(expires_at)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs a migrated Postgres (docker compose up -d postgres + migrator run)"]
async fn locations_sweep_deletes_only_expired_rows() {
    let pool = test_pool().await;
    let (project, user) = fixture(&pool).await;
    insert_location(&pool, project, user, "NOW() - INTERVAL '2 hours'").await;
    insert_location(&pool, project, user, "NOW() + INTERVAL '2 hours'").await;

    let deleted = atlas_location_consumer::reaper::sweep_once(&pool, 100)
        .await
        .unwrap();
    assert_eq!(deleted, 1, "only the expired row goes");

    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM geo.locations WHERE project_id = $1 AND user_id = $2",
    )
    .bind(project)
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remaining, 1, "the unexpired row stays");
}

#[tokio::test]
#[ignore = "needs a migrated Postgres (docker compose up -d postgres + migrator run)"]
async fn votes_sweep_respects_the_horizon() {
    let pool = test_pool().await;
    let (project, user) = fixture(&pool).await;
    for age_days in [10i64, 400] {
        sqlx::query(
            r#"
            INSERT INTO geo.safety_votes (project_id, user_id, position, vote, created_at)
            VALUES ($1, $2, ST_SetSRID(ST_MakePoint(1, 2), 4326), 'safe',
                    NOW() - ($3::bigint * INTERVAL '1 day'))
            "#,
        )
        .bind(project)
        .bind(user)
        .bind(age_days)
        .execute(&pool)
        .await
        .unwrap();
    }

    let deleted = atlas_location_consumer::reaper::sweep_votes_once(
        &pool,
        Duration::from_secs(365 * 24 * 3600),
        100,
    )
    .await
    .unwrap();
    assert_eq!(deleted, 1, "only the vote past the horizon goes");

    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM geo.safety_votes WHERE project_id = $1 AND user_id = $2",
    )
    .bind(project)
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remaining, 1, "the recent vote stays");
}

#[tokio::test]
#[ignore = "needs a migrated Postgres (docker compose up -d postgres + migrator run)"]
async fn memberships_sweep_drops_only_users_with_no_live_location() {
    let pool = test_pool().await;
    let (project, user) = fixture(&pool).await;
    let (_, other_user) = {
        let user2: Uuid = sqlx::query_scalar(
            "INSERT INTO auth.users (project_id, email, password_hash) VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(project)
        .bind(format!("u2-{}@atlas.test", Uuid::new_v4()))
        .bind("x")
        .fetch_one(&pool)
        .await
        .unwrap();
        (project, user2)
    };
    let fence: Uuid = sqlx::query_scalar(
        r#"INSERT INTO geo.geofences (project_id, user_id, label, center, radius_m, active)
           VALUES ($1, $2, 'f', ST_SetSRID(ST_MakePoint(1, 2), 4326), 100, TRUE)
           RETURNING id"#,
    )
    .bind(project)
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();

    // user: expired location + membership -> swept.
    insert_location(&pool, project, user, "NOW() - INTERVAL '2 hours'").await;
    sqlx::query(
        "INSERT INTO geo.geofence_memberships (project_id, user_id, geofence_id) VALUES ($1, $2, $3)",
    )
    .bind(project)
    .bind(user)
    .bind(fence)
    .execute(&pool)
    .await
    .unwrap();

    // other_user: live location + membership -> kept.
    insert_location(&pool, project, other_user, "NOW() + INTERVAL '2 hours'").await;
    sqlx::query(
        "INSERT INTO geo.geofence_memberships (project_id, user_id, geofence_id) VALUES ($1, $2, $3)",
    )
    .bind(project)
    .bind(other_user)
    .bind(fence)
    .execute(&pool)
    .await
    .unwrap();

    let deleted = atlas_location_consumer::reaper::sweep_memberships_once(&pool, 100)
        .await
        .unwrap();
    assert_eq!(deleted, 1, "only the stale-user membership goes");

    let kept: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM geo.geofence_memberships WHERE project_id = $1 AND user_id = $2",
    )
    .bind(project)
    .bind(other_user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, 1, "the live user's membership stays");
}

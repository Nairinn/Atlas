//! `atlas logs [service]`.
//!
//! # What this returns, and what it deliberately does not
//!
//! This serves the control plane's own audit trail — deploys, key
//! issuance, key revocations — scoped to one project. That is real,
//! attributable data the service owns and can vouch for.
//!
//! It is *not* application log shipping. `atlas logs auth` does not return
//! stdout from auth-service, because nothing in this platform collects
//! that. Doing it properly means a log aggregator (Loki, Cloud Logging,
//! an ELK stack) scraping container stdout, with the control plane
//! querying it and enforcing per-project tenancy on the results. That is
//! an infrastructure component and a query-federation problem, not a
//! table this service can add.
//!
//! The alternative — an ingest endpoint the services POST their logs to —
//! was considered and rejected: it would mean building a log pipeline
//! nobody writes to, and shipping an empty `atlas logs` that looks broken
//! rather than one that returns less than you hoped but is honest about
//! what it is.
//!
//! The `?service=` filter still works, because audit rows carry a service
//! column. Today every row is `control-plane`; when log federation lands,
//! per-service rows join the same result set without a contract change.

use axum::extract::{Path, Query, State};
use axum::{routing::get, Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::auth::AuthedKey;
use crate::error::ApiError;
use crate::models::LogLine;
use crate::routes::projects::{resolve_project, rfc3339};
use crate::state::AppState;

/// Newest N events. `atlas logs` renders to a terminal, so an unbounded
/// result would be both slow and useless.
const LOG_LIMIT: i64 = 200;

pub fn routes() -> Router<AppState> {
    Router::new().route("/projects/:name/logs", get(project_logs))
}

#[derive(Debug, Deserialize)]
pub struct LogParams {
    /// `auth` | `geo` | `payments` | `events` | `control-plane`.
    pub service: Option<String>,
    /// RFC 3339 lower bound on `created_at`.
    pub since: Option<DateTime<Utc>>,
    /// At most this many rows; capped at LOG_LIMIT.
    pub limit: Option<i64>,
}

async fn project_logs(
    State(state): State<AppState>,
    key: AuthedKey,
    Path(name): Path<String>,
    Query(params): Query<LogParams>,
) -> Result<Json<Vec<LogLine>>, ApiError> {
    let project = resolve_project(&state, &key, &name).await?;

    // Take the newest N rows, then flip to chronological order: logs
    // read top-to-bottom oldest-first, but "most recent N" is the window
    // you want, not "first N ever".
    let limit = params.limit.unwrap_or(LOG_LIMIT).clamp(1, LOG_LIMIT);
    let rows: Vec<(DateTime<Utc>, String, String, String)> = sqlx::query_as(
        r#"
        SELECT created_at, service, level, message
        FROM (
            SELECT created_at, service, level, message
            FROM control.audit_events
            WHERE project_id = $1
              AND ($2::text IS NULL OR service = $2)
              AND ($4::timestamptz IS NULL OR created_at >= $4)
            ORDER BY created_at DESC
            LIMIT $3
        ) recent
        ORDER BY created_at ASC
        "#,
    )
    .bind(project.id)
    .bind(params.service.as_deref())
    .bind(limit)
    .bind(params.since)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|(ts, service, level, message)| LogLine {
                timestamp: rfc3339(ts),
                service,
                level,
                message,
            })
            .collect(),
    ))
}

//! Project provisioning (`atlas deploy`) and status (`atlas status`).

use axum::extract::{Path, State};
use axum::{routing::get, routing::post, Json, Router};
use chrono::{DateTime, Utc};
use std::time::Instant;
use uuid::Uuid;

use crate::auth::AuthedKey;
use crate::error::ApiError;
use crate::models::{
    DeployRequest, DeployResponse, ProvisionedService, ServiceStatus, StatusResponse,
};
use crate::state::AppState;
use crate::status::{self, SERVICES};

/// Mirrors `cli::config::KNOWN_REGIONS`. A region the CLI would refuse to
/// write must not be accepted here either.
const KNOWN_REGIONS: [&str; 3] = ["us-central1", "eu-west1", "ap-southeast1"];
const ENVIRONMENTS: [&str; 3] = ["development", "staging", "production"];

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/projects", post(deploy))
        .route("/projects/:name/status", get(project_status))
}

/// A project row plus the caller's right to touch it.
pub struct ResolvedProject {
    pub id: Uuid,
    pub name: String,
    pub environment: String,
}

/// Load a project by name and authorize the key against it.
///
/// Returns 404 rather than 403 when the project belongs to another
/// account. A developer guessing project names should not be able to
/// enumerate which ones exist.
pub async fn resolve_project(
    state: &AppState,
    key: &AuthedKey,
    name: &str,
) -> Result<ResolvedProject, ApiError> {
    let row: Option<(Uuid, String, String)> = sqlx::query_as(
        r#"
        SELECT id, name, environment
        FROM control.projects
        WHERE name = $1 AND account_id = $2
        "#,
    )
    .bind(name)
    .bind(key.account_id)
    .fetch_optional(&state.pool)
    .await?;

    let (id, name, environment) =
        row.ok_or_else(|| ApiError::NotFound(format!("project '{name}' not found")))?;

    if !key.may_access(id) {
        return Err(ApiError::Forbidden(
            "this key is scoped to a different project".into(),
        ));
    }

    Ok(ResolvedProject {
        id,
        name,
        environment,
    })
}

// --- deploy -----------------------------------------------------------------

fn validate_project_name(name: &str) -> Result<(), ApiError> {
    // Same rule as `cli::config::is_valid_project_name`. Duplicated rather
    // than shared because the CLI and the control plane are separate
    // deployables that version independently; if they drift, the server is
    // the one that has to be strict.
    let ok = (3..=40).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !ok {
        return Err(ApiError::BadRequest(format!(
            "project name '{name}' is invalid: 3-40 chars, lowercase alphanumeric and hyphens, \
             not starting or ending with a hyphen"
        )));
    }
    Ok(())
}

async fn deploy(
    State(state): State<AppState>,
    key: AuthedKey,
    Json(body): Json<DeployRequest>,
) -> Result<Json<DeployResponse>, ApiError> {
    let started = Instant::now();

    key.require_admin()?;
    validate_project_name(&body.name)?;
    if !KNOWN_REGIONS.contains(&body.region.as_str()) {
        return Err(ApiError::BadRequest(format!(
            "region '{}' is not a known Atlas region. Known: {:?}",
            body.region, KNOWN_REGIONS
        )));
    }
    if !ENVIRONMENTS.contains(&body.environment.as_str()) {
        return Err(ApiError::BadRequest(format!(
            "environment '{}' must be one of {:?}",
            body.environment, ENVIRONMENTS
        )));
    }
    if body.services_enabled.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one service must be enabled".into(),
        ));
    }
    for s in &body.services_enabled {
        if !SERVICES.contains(&s.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "unknown service '{s}'. Known: {SERVICES:?}"
            )));
        }
    }

    let endpoint = state.cfg.endpoint_for(&body.name);
    let mut tx = state.pool.begin().await?;

    // Is the name taken, and by whom?
    let existing: Option<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, account_id FROM control.projects WHERE name = $1")
            .bind(&body.name)
            .fetch_optional(&mut *tx)
            .await?;

    let (project_id, first_provision) = match existing {
        Some((_, owner)) if owner != key.account_id => {
            // Project names are globally unique because they appear in the
            // endpoint URL. Someone else holds this one.
            return Err(ApiError::Conflict(format!(
                "project name '{}' is already taken",
                body.name
            )));
        }
        Some((id, _)) => {
            if !key.may_access(id) {
                return Err(ApiError::Forbidden(
                    "this key is scoped to a different project".into(),
                ));
            }
            sqlx::query(
                r#"
                UPDATE control.projects
                SET region = $2, environment = $3, endpoint = $4, updated_at = NOW()
                WHERE id = $1
                "#,
            )
            .bind(id)
            .bind(&body.region)
            .bind(&body.environment)
            .bind(&endpoint)
            .execute(&mut *tx)
            .await?;
            (id, false)
        }
        None => {
            // Only an account-scoped key can create a project: a
            // project-scoped key is, by definition, bound to a project
            // that already exists.
            if key.project_id.is_some() {
                return Err(ApiError::Forbidden(
                    "this key is scoped to a single project and cannot create new ones".into(),
                ));
            }
            // A concurrent deploy of the same name races this INSERT;
            // the unique violation is a name collision, not a fault, and
            // surfacing it as 500 would page somebody for a race.
            let id: Uuid = match sqlx::query_scalar(
                r#"
                INSERT INTO control.projects (account_id, name, region, environment, endpoint)
                VALUES ($1, $2, $3, $4, $5)
                RETURNING id
                "#,
            )
            .bind(key.account_id)
            .bind(&body.name)
            .bind(&body.region)
            .bind(&body.environment)
            .bind(&endpoint)
            .fetch_one(&mut *tx)
            .await
            {
                Ok(id) => id,
                Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                    return Err(ApiError::Conflict(format!(
                        "project name '{}' is already taken",
                        body.name
                    )));
                }
                Err(e) => return Err(e.into()),
            };
            (id, true)
        }
    };

    // Which services were already on before this deploy? Used to tell a
    // first provision from a no-op re-apply in the response.
    let previously_enabled: Vec<String> = sqlx::query_scalar(
        "SELECT service FROM control.project_services WHERE project_id = $1 AND enabled = TRUE",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;

    let mut provisioned = Vec::new();
    for service in SERVICES {
        let enabled = body.services_enabled.iter().any(|s| s == service);
        let was_enabled = previously_enabled.iter().any(|s| s == service);

        sqlx::query(
            r#"
            INSERT INTO control.project_services (project_id, service, enabled, status, detail, provisioned_at)
            VALUES ($1, $2, $3, $4, $5, NOW())
            ON CONFLICT (project_id, service) DO UPDATE
            SET enabled = EXCLUDED.enabled,
                status = EXCLUDED.status,
                detail = EXCLUDED.detail,
                provisioned_at = NOW()
            "#,
        )
        .bind(project_id)
        .bind(service)
        .bind(enabled)
        .bind(if enabled { "ok" } else { "skipped" })
        .bind(if enabled { None } else { Some("disabled in atlas.toml") })
        .execute(&mut *tx)
        .await?;

        // The response lists every namespace, not just the enabled ones:
        // seeing `payments  skipped` is how a developer confirms that
        // turning it off in atlas.toml actually took effect.
        provisioned.push(ProvisionedService {
            service: service.to_string(),
            status: if enabled { "ok" } else { "skipped" }.to_string(),
            detail: match (enabled, was_enabled) {
                (true, true) => Some("already provisioned".to_string()),
                (true, false) => None,
                (false, true) => Some("disabled by this deploy".to_string()),
                (false, false) => Some("not enabled".to_string()),
            },
        });
    }

    let elapsed_ms = started.elapsed().as_millis() as u64;

    sqlx::query(
        r#"
        INSERT INTO control.deployments (project_id, services_requested, status, elapsed_ms)
        VALUES ($1, $2, 'ok', $3)
        "#,
    )
    .bind(project_id)
    .bind(&body.services_enabled)
    .bind(elapsed_ms as i64)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO control.audit_events (project_id, account_id, actor_key_prefix, level, action, message)
        VALUES ($1, $2, $3, 'info', $4, $5)
        "#,
    )
    .bind(project_id)
    .bind(key.account_id)
    .bind(&key.prefix)
    .bind(if first_provision { "project.created" } else { "project.redeployed" })
    .bind(format!(
        "{} '{}' in {} ({}) with services: {}",
        if first_provision { "provisioned" } else { "re-deployed" },
        body.name,
        body.region,
        body.environment,
        body.services_enabled.join(", ")
    ))
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    tracing::info!(
        project = %body.name,
        first_provision,
        elapsed_ms,
        "deploy applied"
    );

    Ok(Json(DeployResponse {
        project_name: body.name,
        region: body.region,
        provisioned,
        endpoint,
        elapsed_ms,
    }))
}

// --- status -----------------------------------------------------------------

async fn project_status(
    State(state): State<AppState>,
    key: AuthedKey,
    Path(name): Path<String>,
) -> Result<Json<StatusResponse>, ApiError> {
    let project = resolve_project(&state, &key, &name).await?;

    let enabled: Vec<String> = sqlx::query_scalar(
        "SELECT service FROM control.project_services WHERE project_id = $1 AND enabled = TRUE",
    )
    .bind(project.id)
    .fetch_all(&state.pool)
    .await?;

    let cfg = &state.cfg;
    // Probe everything at once: four sequential dials at a 2s timeout
    // would make `atlas status` take eight seconds in the worst case.
    let (auth_up, geo_up, payments_up, events_up, usage) = tokio::join!(
        status::grpc_healthy(&cfg.auth_addr, cfg.probe_timeout),
        status::grpc_healthy(&cfg.geo_addr, cfg.probe_timeout),
        status::grpc_healthy(&cfg.payments_addr, cfg.probe_timeout),
        status::tcp_reachable(&cfg.kafka_brokers, cfg.probe_timeout),
        status::fetch_gateway_metrics(&state.http, &cfg.gateway_metrics_url),
    );

    let services = SERVICES
        .iter()
        .filter(|s| enabled.iter().any(|e| e == *s))
        .map(|service| {
            let healthy = match *service {
                "auth" => auth_up,
                "geo" => geo_up,
                "payments" => payments_up,
                _ => events_up,
            };
            // Gateway metrics carry no project label: the counters are
            // platform-wide, so reporting them as this project's traffic
            // would be a wrong number that looks exact. Null until the
            // gateway labels by tenant; health still means something.
            let _ = usage.get(service).copied().unwrap_or_default();
            ServiceStatus {
                name: service.to_string(),
                healthy,
                p95_latency_ms: None,
                requests_24h: None,
                error_rate: None,
            }
        })
        .collect();

    Ok(Json(StatusResponse {
        project_name: project.name,
        services,
        scope: "platform".to_string(),
    }))
}

/// Format a timestamp the way the CLI's fixtures do (`2026-05-26T15:42:11Z`).
pub fn rfc3339(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_names_follow_the_cli_rule() {
        assert!(validate_project_name("my-mobility-app").is_ok());
        assert!(validate_project_name("abc").is_ok());
        assert!(validate_project_name(&"a".repeat(40)).is_ok());

        assert!(validate_project_name("ab").is_err(), "too short");
        assert!(validate_project_name(&"a".repeat(41)).is_err(), "too long");
        assert!(validate_project_name("-leading").is_err());
        assert!(validate_project_name("trailing-").is_err());
        assert!(validate_project_name("Has-Upper").is_err());
        assert!(validate_project_name("under_score").is_err());
        assert!(validate_project_name("has space").is_err());
    }

    #[test]
    fn rfc3339_matches_the_cli_fixture_format() {
        let ts = DateTime::parse_from_rfc3339("2026-05-26T15:42:11.123Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(rfc3339(ts), "2026-05-26T15:42:11Z");
    }
}

//! `atlas keys list | create | revoke`.
//!
//! # Only project-scoped keys are visible here
//!
//! Every query filters on `project_id = $project`, so the account-scoped
//! bootstrap key (whose `project_id` is NULL) never appears in a listing
//! and cannot be revoked through these routes.
//!
//! That is intentional rather than an oversight. The bootstrap key is the
//! one credential that can create projects; revoking it through a routine
//! `atlas keys revoke` — with a prefix typo, say — would lock the
//! developer out of their own account with no way back in short of
//! database surgery. Retiring it should be a deliberate account-level
//! operation, which is a route this service does not yet have.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{routing::delete, routing::get, Json, Router};
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::auth::AuthedKey;
use crate::error::ApiError;
use crate::keys as keygen;
use crate::models::{ApiKeyView, CreateKeyRequest, CreatedKeyResponse};
use crate::routes::projects::{resolve_project, rfc3339};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/projects/:name/keys", get(list_keys).post(create_key))
        .route("/projects/:name/keys/:prefix", delete(revoke_key))
}

type KeyRow = (
    String,
    String,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    String,
    String,
);

fn to_view(row: KeyRow) -> ApiKeyView {
    ApiKeyView {
        name: row.0,
        prefix: row.1,
        created_at: rfc3339(row.2),
        last_used_at: row.3.map(rfc3339),
        status: row.4,
        scope: row.5,
    }
}

async fn list_keys(
    State(state): State<AppState>,
    key: AuthedKey,
    Path(name): Path<String>,
) -> Result<Json<Vec<ApiKeyView>>, ApiError> {
    key.require_admin()?;
    let project = resolve_project(&state, &key, &name).await?;

    let rows: Vec<KeyRow> = sqlx::query_as(
        r#"
        SELECT name, key_prefix, created_at, last_used_at, status, scope
        FROM control.api_keys
        WHERE project_id = $1
        ORDER BY created_at ASC
        "#,
    )
    .bind(project.id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(rows.into_iter().map(to_view).collect()))
}

async fn create_key(
    State(state): State<AppState>,
    key: AuthedKey,
    Path(name): Path<String>,
    Json(body): Json<CreateKeyRequest>,
) -> Result<(StatusCode, Json<CreatedKeyResponse>), ApiError> {
    key.require_admin()?;
    let project = resolve_project(&state, &key, &name).await?;

    let key_name = body.name.trim().to_string();
    if key_name.is_empty() {
        return Err(ApiError::BadRequest("key name is required".into()));
    }
    if key_name.len() > 64 {
        return Err(ApiError::BadRequest(
            "key name must be at most 64 characters".into(),
        ));
    }

    // Scope: absent or 'data' = gateway only; 'admin' also allows
    // control-plane mutations. Anything else is rejected rather than
    // guessed.
    let scope = match body.scope.as_deref().map(str::trim) {
        None | Some("") | Some("data") => "data",
        Some("admin") => "admin",
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "unknown scope '{other}': known: data, admin"
            )))
        }
    };

    // Tier follows the project's environment, so a production project
    // mints `atl_live_` keys and a dev project mints `atl_dev_` keys.
    // Re-roll while the 4-char prefix collides with one of the project's
    // ACTIVE keys: a duplicate makes one key unrevokable by prefix, so it
    // is cheaper to roll again than to disambiguate later.
    let taken: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT key_prefix FROM control.api_keys
        WHERE project_id = $1 AND status = 'active'
        "#,
    )
    .bind(project.id)
    .fetch_all(&state.pool)
    .await?;
    let mut generated = keygen::generate(&project.environment);
    while taken.contains(&generated.prefix) {
        generated = keygen::generate(&project.environment);
    }
    let expires_at = body.expiry.days().map(|d| Utc::now() + Duration::days(d));

    // Key + audit in ONE transaction: an audit trail that can miss the
    // event it exists to record is not an audit trail.
    let mut tx = state.pool.begin().await?;
    let created_at: DateTime<Utc> = sqlx::query_scalar(
        r#"
        INSERT INTO control.api_keys
            (account_id, project_id, name, key_prefix, key_hash, expires_at, scope)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING created_at
        "#,
    )
    .bind(key.account_id)
    .bind(project.id)
    .bind(&key_name)
    .bind(&generated.prefix)
    .bind(&generated.hash)
    .bind(expires_at)
    .bind(scope)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO control.audit_events (project_id, account_id, actor_key_prefix, level, action, message)
        VALUES ($1, $2, $3, 'info', 'key.created', $4)
        "#,
    )
    .bind(project.id)
    .bind(key.account_id)
    .bind(&key.prefix)
    .bind(format!(
        "issued key '{}' ({}) expiring {}",
        key_name,
        generated.prefix,
        expires_at.map(rfc3339).unwrap_or_else(|| "never".into())
    ))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::info!(project = %project.name, prefix = %generated.prefix, "api key created");

    Ok((
        StatusCode::CREATED,
        Json(CreatedKeyResponse {
            key: ApiKeyView {
                name: key_name,
                prefix: generated.prefix,
                created_at: rfc3339(created_at),
                last_used_at: None,
                status: "active".to_string(),
                scope: scope.to_string(),
            },
            api_key: generated.plaintext,
        }),
    ))
}

async fn revoke_key(
    State(state): State<AppState>,
    key: AuthedKey,
    Path((name, prefix)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    key.require_admin()?;
    let project = resolve_project(&state, &key, &name).await?;

    // Match active keys only, so revoking twice is a clean 404 rather than
    // a silent success on an already-dead key.
    let matches: Vec<(Uuid, String)> = sqlx::query_as(
        r#"
        SELECT id, key_prefix
        FROM control.api_keys
        WHERE project_id = $1 AND key_prefix = $2 AND status = 'active'
        "#,
    )
    .bind(project.id)
    .bind(&prefix)
    .fetch_all(&state.pool)
    .await?;

    match matches.len() {
        0 => {
            return Err(ApiError::NotFound(format!(
                "no active key with prefix '{prefix}' in project '{name}'"
            )))
        }
        1 => {}
        n => {
            // A display prefix is only four secret characters, so a
            // collision inside one project is unlikely but possible.
            // Revoking an arbitrary one of them would be worse than
            // refusing.
            return Err(ApiError::Conflict(format!(
                "prefix '{prefix}' matches {n} active keys; revoke by full prefix"
            )));
        }
    }

    let (key_id, key_prefix) = matches.into_iter().next().expect("exactly one match");

    let mut tx = state.pool.begin().await?;
    sqlx::query(
        r#"
        UPDATE control.api_keys
        SET status = 'revoked', revoked_at = NOW()
        WHERE id = $1
        "#,
    )
    .bind(key_id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO control.audit_events (project_id, account_id, actor_key_prefix, level, action, message)
        VALUES ($1, $2, $3, 'warn', 'key.revoked', $4)
        "#,
    )
    .bind(project.id)
    .bind(key.account_id)
    .bind(&key.prefix)
    .bind(format!("revoked key {key_prefix}"))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::info!(project = %project.name, prefix = %key_prefix, "api key revoked");

    // The CLI ignores the body on success and only checks the status.
    Ok(StatusCode::NO_CONTENT)
}

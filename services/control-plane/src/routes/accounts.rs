//! Account bootstrap.
//!
//! # The chicken-and-egg problem this solves
//!
//! Every other route needs a key in the `Authorization` header, and
//! `atlas.toml` requires the developer to paste one in before `atlas
//! deploy` will even parse the file. Something has to mint the first key
//! without already holding one. That is this route.
//!
//! It is therefore the single unauthenticated write in the service, and
//! the only place a key is ever returned in plaintext.
//!
//! # What it is not
//!
//! This is not a signup flow. There is no password, no email
//! verification, and no ownership proof — anyone who can reach the port
//! can mint an account. That is acceptable for a control plane running
//! inside a private network or on a developer's laptop, and it is not
//! acceptable on the public internet. Before this is exposed publicly it
//! needs, at minimum: verified email, a rate limit keyed on source
//! address, and an abuse story. Deploy it behind the same perimeter as
//! the rest of the control plane until then.

use axum::extract::State;
use axum::http::StatusCode;
use axum::{routing::post, Json, Router};
use uuid::Uuid;

use crate::error::ApiError;
use crate::models::{CreateAccountRequest, CreateAccountResponse};
use crate::state::AppState;
use atlas_keys as keys;

pub fn routes() -> Router<AppState> {
    Router::new().route("/accounts", post(create_account))
}

async fn create_account(
    State(state): State<AppState>,
    Json(body): Json<CreateAccountRequest>,
) -> Result<(StatusCode, Json<CreateAccountResponse>), ApiError> {
    let email = body.email.trim().to_lowercase();
    if email.is_empty() {
        return Err(ApiError::BadRequest("email is required".into()));
    }
    // Deliberately shallow: the local part of an address can contain
    // almost anything, and a regex here would reject valid addresses to
    // catch typos it cannot actually detect. Real validation is a
    // confirmation email, which this service does not send.
    if !email.contains('@') || email.starts_with('@') || email.ends_with('@') {
        return Err(ApiError::BadRequest(
            "email does not look like an address".into(),
        ));
    }

    let mut tx = state.pool.begin().await?;

    let existing: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM control.accounts WHERE email = $1")
            .bind(&email)
            .fetch_optional(&mut *tx)
            .await?;
    if existing.is_some() {
        // Not "here is a fresh key for that account" — that would turn
        // this route into a credential reset for anyone who knows an
        // email address.
        return Err(ApiError::Conflict(format!(
            "an account already exists for {email}"
        )));
    }

    let account_id: Uuid =
        sqlx::query_scalar("INSERT INTO control.accounts (email) VALUES ($1) RETURNING id")
            .bind(&email)
            .fetch_one(&mut *tx)
            .await?;

    // Account-scoped: project_id stays NULL so this key can create the
    // developer's first project.
    //
    // Admin-scoped (migration 0110): the column defaults to 'data', which
    // is right for CI keys minted through /v1/projects/{p}/keys, but the
    // bootstrap key exists to create projects, deploy, and mint those CI
    // keys — all control-plane mutations that require_admin gates. A
    // data-scoped bootstrap key can create an account and then do nothing
    // with it.
    //
    // Minted in the `development` tier (`atl_dev_`). An account has no
    // environment of its own — only projects do — and issuing a
    // production-looking `atl_live_` key before any project exists would
    // misrepresent what it is.
    let generated = keys::generate("development");
    sqlx::query(
        r#"
        INSERT INTO control.api_keys (account_id, project_id, name, key_prefix, key_hash, scope)
        VALUES ($1, NULL, $2, $3, $4, 'admin')
        "#,
    )
    .bind(account_id)
    .bind("bootstrap")
    .bind(&generated.prefix)
    .bind(&generated.hash)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO control.audit_events (project_id, account_id, actor_key_prefix, level, action, message)
        VALUES (NULL, $1, $2, 'info', 'account.created', $3)
        "#,
    )
    .bind(account_id)
    .bind(&generated.prefix)
    .bind(format!("account created for {email}"))
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    tracing::info!(%account_id, prefix = %generated.prefix, "account created");

    Ok((
        StatusCode::CREATED,
        Json(CreateAccountResponse {
            account_id: account_id.to_string(),
            api_key: generated.plaintext,
            prefix: generated.prefix,
            warning: "This key is shown once and cannot be recovered. \
                      Store it in atlas.toml as project.api_key."
                .to_string(),
        }),
    ))
}

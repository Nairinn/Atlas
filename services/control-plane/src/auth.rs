//! Bearer-key authentication.
//!
//! Any handler that takes an [`AuthedKey`] is authenticated; axum runs the
//! extractor before the handler body, so a bad key short-circuits into a
//! 401 without the handler ever running. `POST /v1/accounts` and the
//! health probes omit it and are public by construction.
//!
//! Lookup is by SHA-256 digest against a unique index, so the presented
//! key is never compared in application code and never appears in a query
//! log — only its digest does.

use axum::async_trait;
use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::error::ApiError;
use crate::keys;
use crate::state::AppState;

/// A caller whose key was resolved this request.
#[derive(Debug, Clone)]
pub struct AuthedKey {
    pub key_id: Uuid,
    pub account_id: Uuid,
    /// `None` for an account-scoped key, which may create projects and
    /// act on any project in its account.
    pub project_id: Option<Uuid>,
    pub prefix: String,
    /// 'data' or 'admin' (D4). Mutations here require admin; the gateway
    /// requires data.
    pub scope: String,
}

impl AuthedKey {
    /// Whether this key may act on `project_id`.
    ///
    /// Account-scoped keys pass for any project in the account — the
    /// account check happens at lookup time, since the project is loaded
    /// with an `account_id` filter. Project-scoped keys must match exactly.
    pub fn may_access(&self, project_id: Uuid) -> bool {
        match self.project_id {
            None => true,
            Some(scoped) => scoped == project_id,
        }
    }

    /// 403 unless this key may mutate the control plane (D4: `admin`).
    /// Called from every write handler; reads stay open to any scope.
    /// The account row itself is public-by-rate-limit (POST /v1/accounts),
    /// so it is not guarded here.
    pub fn require_admin(&self) -> Result<(), ApiError> {
        if self.scope == "admin" {
            Ok(())
        } else {
            Err(ApiError::Forbidden(
                "this key is data-scoped; control-plane mutations need an admin key".into(),
            ))
        }
    }
}

struct KeyRow {
    id: Uuid,
    account_id: Uuid,
    project_id: Option<Uuid>,
    key_prefix: String,
    status: String,
    expires_at: Option<DateTime<Utc>>,
    scope: String,
}

#[async_trait]
impl FromRequestParts<AppState> for AuthedKey {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .ok_or_else(|| ApiError::Unauthorized("missing Authorization header".into()))?
            .to_str()
            .map_err(|_| {
                ApiError::Unauthorized("Authorization header is not valid UTF-8".into())
            })?;

        let token = parse_bearer(header)?;

        // Shape check first: a junk token should not cost a query.
        if !keys::looks_like_key(token) {
            return Err(ApiError::Unauthorized("malformed API key".into()));
        }

        let digest = keys::hash(token);
        let row = sqlx::query_as::<
            _,
            (
                Uuid,
                Uuid,
                Option<Uuid>,
                String,
                String,
                Option<DateTime<Utc>>,
                String,
            ),
        >(
            r#"
            SELECT id, account_id, project_id, key_prefix, status, expires_at, scope
            FROM control.api_keys
            WHERE key_hash = $1
            "#,
        )
        .bind(&digest)
        .fetch_optional(&state.pool)
        .await?
        .map(|r| KeyRow {
            id: r.0,
            account_id: r.1,
            project_id: r.2,
            key_prefix: r.3,
            status: r.4,
            expires_at: r.5,
            scope: r.6,
        });

        // One message for "no such key", "revoked", and "expired" alike.
        // Distinguishing them would tell an attacker holding a random
        // string whether it happened to name a real key.
        let row = row.ok_or_else(|| ApiError::Unauthorized("invalid API key".into()))?;
        if row.status != "active" {
            return Err(ApiError::Unauthorized("invalid API key".into()));
        }
        if let Some(expiry) = row.expires_at {
            if Utc::now() >= expiry {
                return Err(ApiError::Unauthorized("invalid API key".into()));
            }
        }

        touch_last_used(state, row.id).await;

        Ok(AuthedKey {
            key_id: row.id,
            account_id: row.account_id,
            project_id: row.project_id,
            prefix: row.key_prefix,
            scope: row.scope,
        })
    }
}

/// Record that the key was used, at most once a minute.
///
/// `last_used_at` is a convenience column shown by `atlas keys list`, not
/// an audit record, so writing it on literally every request would add a
/// row lock to the hot path for no benefit. The `WHERE` clause makes the
/// update a no-op most of the time.
///
/// A failure here is logged and swallowed: the caller authenticated
/// successfully, and losing a usage timestamp is not a reason to fail
/// their request.
async fn touch_last_used(state: &AppState, key_id: Uuid) {
    let result = sqlx::query(
        r#"
        UPDATE control.api_keys
        SET last_used_at = NOW()
        WHERE id = $1
          AND (last_used_at IS NULL OR last_used_at < NOW() - INTERVAL '1 minute')
        "#,
    )
    .bind(key_id)
    .execute(&state.pool)
    .await;

    if let Err(e) = result {
        tracing::warn!(error = %e, "failed to update last_used_at");
    }
}

/// Pull the token out of `Authorization: Bearer <token>`.
pub fn parse_bearer(header: &str) -> Result<&str, ApiError> {
    let (scheme, token) = header
        .split_once(' ')
        .ok_or_else(|| ApiError::Unauthorized("expected 'Authorization: Bearer <key>'".into()))?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(ApiError::Unauthorized(
            "authorization scheme must be Bearer".into(),
        ));
    }
    let token = token.trim();
    if token.is_empty() {
        return Err(ApiError::Unauthorized("bearer token is empty".into()));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_unauthorized(e: ApiError) -> bool {
        matches!(e, ApiError::Unauthorized(_))
    }

    #[test]
    fn bearer_parsing_accepts_any_scheme_casing() {
        assert_eq!(parse_bearer("Bearer atl_live_x").unwrap(), "atl_live_x");
        assert_eq!(parse_bearer("bearer atl_live_x").unwrap(), "atl_live_x");
        assert_eq!(parse_bearer("BEARER atl_live_x").unwrap(), "atl_live_x");
    }

    #[test]
    fn bearer_parsing_rejects_malformed() {
        assert!(is_unauthorized(parse_bearer("atl_live_x").unwrap_err()));
        assert!(is_unauthorized(parse_bearer("Basic abc").unwrap_err()));
        assert!(is_unauthorized(parse_bearer("Bearer ").unwrap_err()));
    }

    #[test]
    fn account_scoped_keys_reach_any_project() {
        let key = AuthedKey {
            key_id: Uuid::new_v4(),
            account_id: Uuid::new_v4(),
            project_id: None,
            prefix: "atl_live_abcd".into(),
            scope: "admin".into(),
        };
        assert!(key.may_access(Uuid::new_v4()));
    }

    #[test]
    fn project_scoped_keys_reach_only_their_own() {
        let mine = Uuid::new_v4();
        let other = Uuid::new_v4();
        let key = AuthedKey {
            key_id: Uuid::new_v4(),
            account_id: Uuid::new_v4(),
            project_id: Some(mine),
            prefix: "atl_live_abcd".into(),
            scope: "admin".into(),
        };
        assert!(key.may_access(mine));
        assert!(
            !key.may_access(other),
            "a project-scoped key must not reach another project"
        );
    }
}

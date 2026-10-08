//! `atlas.auth` — the only namespace with unauthenticated routes.
//!
//! `POST /register` and `POST /login` are public by necessity. Everything
//! else takes an [`AuthUser`], which makes it authenticated.
//!
//! `auth.IssueToken` has no route here, deliberately: proto/auth.proto
//! marks it "Internal / refresh primitive. Not exposed via the gateway",
//! and for good reason — it mints a valid token for any `user_id` with no
//! credential check. Exposing it would be a complete authentication
//! bypass.

use axum::extract::State;
use axum::http::StatusCode;
use axum::{routing::get, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tonic::Request;

use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::pb::auth::{
    AuthRequest, GetUserRequest, RegisterRequest, RequestEmailVerificationRequest,
    RequestPasswordResetRequest, ResetPasswordRequest, RevokeTokenRequest, VerifyEmailRequest,
};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::validate;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/register", post(register))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/me", get(me))
        .route("/password-reset", post(request_password_reset))
        .route("/password-reset/confirm", post(confirm_password_reset))
        .route("/email/verify", post(request_email_verification))
        .route("/email/verify/confirm", post(confirm_email_verification))
}

#[derive(Debug, Deserialize)]
pub struct RegisterBody {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct RegisterOut {
    pub user_id: String,
}

async fn register(
    State(state): State<AppState>,
    tenant: Tenant,
    Json(body): Json<RegisterBody>,
) -> Result<(StatusCode, Json<RegisterOut>), ApiError> {
    // Email/password rules (format, length, strength) live in the
    // auth-service domain layer. The gateway only rejects the empty case
    // so a blank form does not cost a gRPC round trip.
    if body.email.trim().is_empty() || body.password.is_empty() {
        return Err(ApiError::BadRequest(
            "email and password are required".to_string(),
        ));
    }

    let resp = state
        .auth
        .clone()
        .register(Request::new(RegisterRequest {
            email: body.email,
            password: body.password,
            // From the resolved key, never the body — `RegisterBody` has
            // no project field for a caller to set.
            project_id: tenant.project_id.to_string(),
        }))
        .await
        .map_err(|s| ApiError::upstream("auth", s))?
        .into_inner();

    Ok((
        StatusCode::CREATED,
        Json(RegisterOut {
            user_id: resp.user_id,
        }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct LoginBody {
    pub email: String,
    pub password: String,
    /// Optional position stamped into the token's geospatial claims.
    pub lat: Option<f64>,
    pub lng: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct TokenOut {
    pub token: String,
    pub expires_at: i64,
}

async fn login(
    State(state): State<AppState>,
    tenant: Tenant,
    Json(body): Json<LoginBody>,
) -> Result<Json<TokenOut>, ApiError> {
    if body.email.trim().is_empty() || body.password.is_empty() {
        return Err(ApiError::BadRequest(
            "email and password are required".to_string(),
        ));
    }

    // The proto has no optional scalars, and auth-service reads (0, 0) as
    // "no location supplied". Send zeros when the client omits both, but
    // validate whatever it did send — an out-of-range coordinate should
    // be a 400, not a silently stored bad claim.
    let (lat, lng) = match (body.lat, body.lng) {
        (Some(lat), Some(lng)) => {
            validate::lat_lng(lat, lng)?;
            (lat, lng)
        }
        (None, None) => (0.0, 0.0),
        _ => {
            return Err(ApiError::BadRequest(
                "lat and lng must be supplied together".to_string(),
            ))
        }
    };

    let resp = state
        .auth
        .clone()
        .authenticate(Request::new(AuthRequest {
            email: body.email,
            password: body.password,
            lat,
            lng,
            project_id: tenant.project_id.to_string(),
        }))
        .await
        .map_err(|s| ApiError::upstream("auth", s))?
        .into_inner();

    Ok(Json(TokenOut {
        token: resp.token,
        expires_at: resp.expires_at,
    }))
}

#[derive(Debug, Serialize)]
pub struct LogoutOut {
    pub success: bool,
}

/// Revoke the caller's own token. The token comes from the validated
/// `Authorization` header, so a caller can only ever log itself out —
/// there is no body field naming a token to revoke.
async fn logout(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<LogoutOut>, ApiError> {
    let resp = state
        .auth
        .clone()
        .revoke_token(Request::new(RevokeTokenRequest { token: user.token }))
        .await
        .map_err(|s| ApiError::upstream("auth", s))?
        .into_inner();

    Ok(Json(LogoutOut {
        success: resp.success,
    }))
}

#[derive(Debug, Serialize)]
pub struct MeOut {
    pub user_id: String,
    pub session_id: String,
    pub last_lat: f64,
    pub last_lng: f64,
    pub issued_at: i64,
    pub expires_at: i64,
    pub email: String,
    /// Unix seconds, or `null` when the address has never been confirmed.
    ///
    /// A nullable timestamp rather than a boolean because "when" is the
    /// question asked in every support conversation, and a boolean cannot
    /// be widened into one later without having already lost the answer.
    pub email_verified_at: Option<i64>,
    pub created_at: i64,
}

/// The caller's session and profile.
///
/// # Why this costs two round trips
///
/// The claims come from `ValidateToken`, which is served from a 30s cache;
/// the profile comes from `GetUser`, which is not cached. They are
/// separate calls because they have different lifetimes: claims are fixed
/// at issue, while a profile CHANGES while a token is live — verifying an
/// address is exactly such a change. Folding the profile into the cached
/// claims would show a stale "unverified" for up to 30 seconds to the one
/// user guaranteed to be looking: the one who just clicked the link.
///
/// This is not a hot path. It is called when an app opens, not per
/// location ping.
async fn me(State(state): State<AppState>, user: AuthUser) -> Result<Json<MeOut>, ApiError> {
    let profile = state
        .auth
        .clone()
        .get_user(Request::new(GetUserRequest {
            user_id: user.user_id.clone(),
            project_id: user.project_id.clone(),
        }))
        .await
        .map_err(|s| ApiError::upstream("auth", s))?
        .into_inner();

    Ok(Json(MeOut {
        user_id: user.user_id,
        session_id: user.session_id,
        last_lat: user.last_lat,
        last_lng: user.last_lng,
        issued_at: user.issued_at,
        expires_at: user.expires_at,
        email: profile.email,
        // The proto carries 0 for "never verified"; the JSON carries null,
        // because 0 is a real timestamp and a client doing date maths on
        // it would render 1970.
        email_verified_at: (profile.email_verified_at != 0).then_some(profile.email_verified_at),
        created_at: profile.created_at,
    }))
}

// --- password reset ---------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct EmailBody {
    pub email: String,
}

#[derive(Debug, Serialize)]
pub struct AcceptedOut {
    /// Always true. See the handler doc for why there is nothing else here.
    pub accepted: bool,
}

/// Ask for a password reset link.
///
/// # This endpoint deliberately tells you nothing
///
/// It answers 202 with the same body whether or not the address belongs to
/// a user, and it does so for every syntactically plausible input. An
/// endpoint that distinguished them would be an account enumeration
/// oracle: anyone could feed it a list of addresses and learn which ones
/// have accounts, and those are exactly the ones worth attacking.
///
/// The cost is real — someone who mistypes their address gets the same
/// answer as someone who did not — and it is smaller than the alternative.
async fn request_password_reset(
    State(state): State<AppState>,
    tenant: Tenant,
    Json(body): Json<EmailBody>,
) -> Result<(StatusCode, Json<AcceptedOut>), ApiError> {
    // Not even the empty case is rejected differently: a 400 here would
    // still be a distinguishable response, and there is no value in
    // telling a caller their empty string is empty. Non-auth failures
    // are logged though: a 202 that swallowed a 500 leaves nobody to
    // notice that reset emails stopped going out.
    if let Err(status) = state
        .auth
        .clone()
        .request_password_reset(Request::new(RequestPasswordResetRequest {
            email: body.email,
            project_id: tenant.project_id.to_string(),
        }))
        .await
    {
        if status.code() != tonic::Code::NotFound {
            tracing::warn!(
                code = ?status.code(),
                detail = status.message(),
                "password reset request failed upstream; still answering 202"
            );
        }
    }

    Ok((StatusCode::ACCEPTED, Json(AcceptedOut { accepted: true })))
}

#[derive(Debug, Deserialize)]
pub struct ConfirmResetBody {
    pub token: String,
    pub new_password: String,
}

#[derive(Debug, Serialize)]
pub struct UserIdOut {
    pub user_id: String,
}

/// Redeem a reset token and set a new password.
///
/// Takes no project key check beyond the layer's, and needs no bearer
/// token: whoever holds the emailed token is, for this one call, the
/// person the account belongs to. The token names its own project, so
/// there is nothing for the caller to supply.
///
/// Succeeding here revokes every session the user had. That is the point
/// — a reset says the old credential may be compromised, so a session
/// established with it must not survive.
async fn confirm_password_reset(
    State(state): State<AppState>,
    Json(body): Json<ConfirmResetBody>,
) -> Result<Json<UserIdOut>, ApiError> {
    if body.token.trim().is_empty() || body.new_password.is_empty() {
        return Err(ApiError::BadRequest(
            "token and new_password are required".to_string(),
        ));
    }

    let resp = state
        .auth
        .clone()
        .reset_password(Request::new(ResetPasswordRequest {
            token: body.token,
            new_password: body.new_password,
        }))
        .await
        .map_err(|s| ApiError::upstream("auth", s))?
        .into_inner();

    Ok(Json(UserIdOut {
        user_id: resp.user_id,
    }))
}

// --- email verification -----------------------------------------------------

/// Ask for a verification link. Silent about existence, same as reset.
async fn request_email_verification(
    State(state): State<AppState>,
    tenant: Tenant,
    Json(body): Json<EmailBody>,
) -> Result<(StatusCode, Json<AcceptedOut>), ApiError> {
    if let Err(status) = state
        .auth
        .clone()
        .request_email_verification(Request::new(RequestEmailVerificationRequest {
            email: body.email,
            project_id: tenant.project_id.to_string(),
        }))
        .await
    {
        // NotFound is the "no such address" answer we mean to swallow;
        // anything else means the flow is broken and must be visible.
        if status.code() != tonic::Code::NotFound {
            tracing::warn!(
                code = ?status.code(),
                detail = status.message(),
                "email verification request failed upstream; still answering 202"
            );
        }
    }

    Ok((StatusCode::ACCEPTED, Json(AcceptedOut { accepted: true })))
}

#[derive(Debug, Deserialize)]
pub struct ConfirmVerificationBody {
    pub token: String,
}

/// Redeem a verification token.
///
/// Unlike a reset this does not revoke sessions: confirming an address is
/// not evidence that anything leaked, and logging someone out for doing
/// what they were asked to do is a bad trade.
async fn confirm_email_verification(
    State(state): State<AppState>,
    Json(body): Json<ConfirmVerificationBody>,
) -> Result<Json<UserIdOut>, ApiError> {
    if body.token.trim().is_empty() {
        return Err(ApiError::BadRequest("token is required".to_string()));
    }

    let resp = state
        .auth
        .clone()
        .verify_email(Request::new(VerifyEmailRequest { token: body.token }))
        .await
        .map_err(|s| ApiError::upstream("auth", s))?
        .into_inner();

    Ok(Json(UserIdOut {
        user_id: resp.user_id,
    }))
}

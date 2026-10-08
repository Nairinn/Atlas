//! `atlas.geo` — location, proximity, route scoring, geofences.
//!
//! Every route here is authenticated, and every `user_id` sent upstream
//! comes from the token claims. None of the request DTOs in this module
//! has a `user_id` field; that absence is the enforcement mechanism, so
//! do not add one.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{routing::delete, routing::get, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tonic::Request;

use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::pb::geo::{
    safety_vote_request::Verdict, CreateGeofenceRequest, DeleteGeofenceRequest,
    GeofenceCheckRequest, LatLng, ListGeofencesRequest, LocationUpdate, NearbyRequest,
    RouteCandidate, RouteScoreRequest, SafetyVoteRequest,
};
use crate::state::AppState;
use crate::validate;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/locations", post(update_location))
        .route("/nearby", get(nearby))
        .route("/routes/score", post(score_route))
        .route("/geofences", post(create_geofence).get(list_geofences))
        .route("/geofences/:id", delete(delete_geofence))
        .route("/geofences/check", post(check_geofences))
        .route("/safety/votes", post(cast_safety_vote))
}

// --- locations --------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LocationBody {
    pub lat: f64,
    pub lng: f64,
    /// Unix epoch seconds. Omit (or send 0) to let geo-engine stamp "now".
    pub recorded_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct AckOut {
    pub ok: bool,
}

async fn update_location(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<LocationBody>,
) -> Result<Json<AckOut>, ApiError> {
    validate::lat_lng(body.lat, body.lng)?;

    let resp = state
        .geo
        .clone()
        .update_location(Request::new(LocationUpdate {
            // Both identities come from the request's credentials.
            // `AuthUser::project_id` was already checked to match the key
            // the request arrived with, so using it here cannot disagree
            // with the tenant layer.
            project_id: user.project_id,
            user_id: user.user_id,
            lat: body.lat,
            lng: body.lng,
            recorded_at: body.recorded_at.unwrap_or(0),
        }))
        .await
        .map_err(|s| ApiError::upstream("geo", s))?
        .into_inner();

    Ok(Json(AckOut { ok: resp.ok }))
}

// --- nearby -----------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct NearbyParams {
    pub lat: f64,
    pub lng: f64,
    pub radius_m: f64,
    /// "driver" | "walker". Free-form: geo-engine uses it only as a
    /// metrics label, so the gateway does not enumerate the values, it
    /// just bounds the length before sending them on.
    pub role: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct NearbyUserOut {
    pub user_id: String,
    pub lat: f64,
    pub lng: f64,
    pub distance_m: f64,
    pub safety_score: f64,
}

#[derive(Debug, Serialize)]
pub struct NearbyOut {
    pub users: Vec<NearbyUserOut>,
}

async fn nearby(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<NearbyParams>,
) -> Result<Json<NearbyOut>, ApiError> {
    validate::lat_lng(params.lat, params.lng)?;
    let radius = validate::radius_m(params.radius_m)?;

    let resp = state
        .geo
        .clone()
        .get_nearby(Request::new(NearbyRequest {
            project_id: user.project_id,
            requester_user_id: user.user_id,
            lat: params.lat,
            lng: params.lng,
            radius_m: radius,
            role: params
                .role
                .map(|r| r.chars().take(16).collect())
                .unwrap_or_default(),
            limit: validate::nearby_limit(params.limit),
        }))
        .await
        .map_err(|s| ApiError::upstream("geo", s))?
        .into_inner();

    Ok(Json(NearbyOut {
        users: resp
            .users
            .into_iter()
            .map(|u| NearbyUserOut {
                user_id: u.user_id,
                lat: u.lat,
                lng: u.lng,
                distance_m: u.distance_m,
                safety_score: u.safety_score,
            })
            .collect(),
    }))
}

// --- route scoring ----------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PointIn {
    pub lat: f64,
    pub lng: f64,
}

#[derive(Debug, Deserialize)]
pub struct CandidateIn {
    pub route_id: String,
    pub points: Vec<PointIn>,
}

#[derive(Debug, Deserialize)]
pub struct ScoreBody {
    pub candidates: Vec<CandidateIn>,
}

#[derive(Debug, Serialize)]
pub struct ScoredRouteOut {
    pub route_id: String,
    pub score: f64,
}

#[derive(Debug, Serialize)]
pub struct ScoreOut {
    pub best_route_id: String,
    pub score: f64,
    pub all_scores: Vec<ScoredRouteOut>,
}

async fn score_route(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<ScoreBody>,
) -> Result<Json<ScoreOut>, ApiError> {
    if body.candidates.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one route candidate is required".to_string(),
        ));
    }
    if body.candidates.len() > validate::MAX_ROUTE_CANDIDATES {
        return Err(ApiError::BadRequest(format!(
            "at most {} candidates per request",
            validate::MAX_ROUTE_CANDIDATES
        )));
    }
    // Validate every point up front. A non-finite coordinate would reach
    // ST_MakePoint and come back as an opaque 500 from PostGIS.
    for c in &body.candidates {
        validate::route_points(&c.route_id, c.points.len())?;
        for p in &c.points {
            validate::lat_lng(p.lat, p.lng)?;
        }
    }

    let resp = state
        .geo
        .clone()
        .score_route(Request::new(RouteScoreRequest {
            project_id: user.project_id,
            user_id: user.user_id,
            candidates: body
                .candidates
                .into_iter()
                .map(|c| RouteCandidate {
                    route_id: c.route_id,
                    points: c
                        .points
                        .into_iter()
                        .map(|p| LatLng {
                            lat: p.lat,
                            lng: p.lng,
                        })
                        .collect(),
                })
                .collect(),
        }))
        .await
        .map_err(|s| ApiError::upstream("geo", s))?
        .into_inner();

    Ok(Json(ScoreOut {
        best_route_id: resp.best_route_id,
        score: resp.score,
        all_scores: resp
            .all_scores
            .into_iter()
            .map(|s| ScoredRouteOut {
                route_id: s.route_id,
                score: s.score,
            })
            .collect(),
    }))
}

// --- geofences --------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateGeofenceBody {
    pub label: Option<String>,
    pub center_lat: f64,
    pub center_lng: f64,
    pub radius_m: f64,
}

#[derive(Debug, Serialize)]
pub struct GeofenceOut {
    pub id: String,
    pub user_id: String,
    pub label: String,
    pub center_lat: f64,
    pub center_lng: f64,
    pub radius_m: f64,
    pub active: bool,
    pub created_at: i64,
}

impl From<crate::pb::geo::Geofence> for GeofenceOut {
    fn from(g: crate::pb::geo::Geofence) -> Self {
        Self {
            id: g.id,
            user_id: g.user_id,
            label: g.label,
            center_lat: g.center_lat,
            center_lng: g.center_lng,
            radius_m: g.radius_m,
            active: g.active,
            created_at: g.created_at,
        }
    }
}

async fn create_geofence(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateGeofenceBody>,
) -> Result<(StatusCode, Json<GeofenceOut>), ApiError> {
    validate::lat_lng(body.center_lat, body.center_lng)?;
    let radius = validate::radius_m(body.radius_m)?;

    let resp = state
        .geo
        .clone()
        .create_geofence(Request::new(CreateGeofenceRequest {
            project_id: user.project_id,
            user_id: user.user_id,
            label: body.label.unwrap_or_default(),
            center_lat: body.center_lat,
            center_lng: body.center_lng,
            radius_m: radius,
        }))
        .await
        .map_err(|s| ApiError::upstream("geo", s))?
        .into_inner();

    Ok((StatusCode::CREATED, Json(resp.into())))
}

#[derive(Debug, Deserialize)]
pub struct ListGeofenceParams {
    #[serde(default)]
    pub active_only: bool,
}

#[derive(Debug, Serialize)]
pub struct ListGeofencesOut {
    pub geofences: Vec<GeofenceOut>,
}

async fn list_geofences(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<ListGeofenceParams>,
) -> Result<Json<ListGeofencesOut>, ApiError> {
    let resp = list_for_user(&state, &user.project_id, &user.user_id, params.active_only).await?;
    Ok(Json(ListGeofencesOut {
        geofences: resp.into_iter().map(Into::into).collect(),
    }))
}

/// Delete (deactivate) one of the caller's geofences.
///
/// # Ownership is enforced in SQL now, not here
///
/// This used to list the caller's geofences first and require the id to
/// be among them, because `DeleteGeofenceRequest` carried a bare
/// geofence_id and the UPDATE had nothing to scope by. That closed the
/// hole for gateway traffic only, cost an extra round trip, and was
/// TOCTOU-racy.
///
/// `DeleteGeofenceRequest` now carries user_id and project_id, and
/// `queries::geofences::deactivate` scopes the UPDATE by all three, so
/// the check holds for every caller including anything speaking gRPC to
/// geo-engine directly. The pre-check is gone.
///
/// A fence that is not yours matches nothing, so `deleted` comes back
/// false and this returns 404 — the same answer as an id that never
/// existed, which is the point. Re-deleting your OWN already-deactivated
/// fence still matches the row, so it stays idempotent.
async fn delete_geofence(
    State(state): State<AppState>,
    user: AuthUser,
    Path(geofence_id): Path<String>,
) -> Result<Json<DeleteOut>, ApiError> {
    let resp = state
        .geo
        .clone()
        // user_id and project_id are what make this a scoped delete
        // rather than an IDOR — see queries::geofences::deactivate.
        .delete_geofence(Request::new(DeleteGeofenceRequest {
            geofence_id,
            user_id: user.user_id,
            project_id: user.project_id,
        }))
        .await
        .map_err(|s| ApiError::upstream("geo", s))?
        .into_inner();

    if !resp.deleted {
        // Not yours, or never existed — deliberately the same answer.
        return Err(ApiError::upstream(
            "geo",
            tonic::Status::not_found("geofence not found"),
        ));
    }

    Ok(Json(DeleteOut {
        deleted: resp.deleted,
    }))
}

#[derive(Debug, Serialize)]
pub struct DeleteOut {
    pub deleted: bool,
}

async fn list_for_user(
    state: &AppState,
    project_id: &str,
    user_id: &str,
    active_only: bool,
) -> Result<Vec<crate::pb::geo::Geofence>, ApiError> {
    Ok(state
        .geo
        .clone()
        .list_geofences(Request::new(ListGeofencesRequest {
            project_id: project_id.to_string(),
            user_id: user_id.to_string(),
            active_only,
        }))
        .await
        .map_err(|s| ApiError::upstream("geo", s))?
        .into_inner()
        .geofences)
}

#[derive(Debug, Deserialize)]
pub struct CheckBody {
    pub lat: f64,
    pub lng: f64,
}

#[derive(Debug, Serialize)]
pub struct CheckOut {
    pub triggered: bool,
    pub geofence_ids: Vec<String>,
}

async fn check_geofences(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CheckBody>,
) -> Result<Json<CheckOut>, ApiError> {
    validate::lat_lng(body.lat, body.lng)?;

    let resp = state
        .geo
        .clone()
        .trigger_geofence_check(Request::new(GeofenceCheckRequest {
            project_id: user.project_id,
            user_id: user.user_id,
            lat: body.lat,
            lng: body.lng,
        }))
        .await
        .map_err(|s| ApiError::upstream("geo", s))?
        .into_inner();

    Ok(Json(CheckOut {
        triggered: resp.triggered,
        geofence_ids: resp.geofence_ids,
    }))
}

// --- safety votes -----------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SafetyVoteBody {
    pub lat: f64,
    pub lng: f64,
    /// `"safe"` or `"unsafe"`. Anything else is a 400 rather than a
    /// silently discarded vote — a typo that scored nothing would be
    /// invisible to the caller and would quietly bias the aggregate.
    pub verdict: String,
}

#[derive(Debug, Serialize)]
pub struct SafetyVoteOut {
    /// The area's score after this vote, in 1000..2000, neutral 1500.
    pub safety_score: f64,
    /// Distinct voters behind it, this caller included.
    pub vote_count: i64,
}

/// Record the caller's judgement about the place at (lat, lng).
///
/// The vote is attributed to the token's user, never to a body field, so
/// nobody can vote as somebody else — and because aggregation takes each
/// voter's most recent verdict, voting twice corrects rather than stuffs.
async fn cast_safety_vote(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<SafetyVoteBody>,
) -> Result<Json<SafetyVoteOut>, ApiError> {
    validate::lat_lng(body.lat, body.lng)?;

    let verdict = match body.verdict.trim().to_ascii_lowercase().as_str() {
        "safe" => Verdict::Safe,
        "unsafe" => Verdict::Unsafe,
        other => {
            return Err(ApiError::BadRequest(format!(
                "verdict must be \"safe\" or \"unsafe\", got {other:?}"
            )))
        }
    };

    let resp = state
        .geo
        .clone()
        .cast_safety_vote(Request::new(SafetyVoteRequest {
            project_id: user.project_id,
            user_id: user.user_id,
            lat: body.lat,
            lng: body.lng,
            verdict: verdict as i32,
        }))
        .await
        .map_err(|s| ApiError::upstream("geo", s))?
        .into_inner();

    Ok(Json(SafetyVoteOut {
        safety_score: resp.safety_score,
        vote_count: resp.vote_count,
    }))
}

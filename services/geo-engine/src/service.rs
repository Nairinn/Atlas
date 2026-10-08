//! tonic service implementation. This is where proto types meet business
//! logic. Every handler:
//!   1. Validates inputs (UUID parsing, bounds checks).
//!   2. Calls into `queries::*` for the DB hit.
//!   3. Translates results back to proto messages.
//!   4. Records latency / counters.
//!
//! Trust model: this service trusts the `user_id` and `project_id` strings
//! in request bodies. The gateway validates the JWT and resolves the API
//! key, then injects both; neither has a field in any REST DTO, so a
//! client cannot supply either. We never re-validate against auth-service
//! on the hot path.
//!
//! What this service DOES enforce is that every query is scoped by the
//! project it was handed. `nearby` in particular searches by position
//! rather than by user id, so without that scoping it would return every
//! Atlas user near a point regardless of which customer asked.

use chrono::{TimeZone, Utc};
use sqlx::PgPool;
use std::time::Instant;
use tonic::{Request, Response, Status};
use tracing::{info, warn};
use uuid::Uuid;

use crate::kafka::LocationProducer;
use crate::pb::events::LocationUpdateEvent;
use crate::pb::geo::{
    geo_engine_server::GeoEngine, safety_vote_request::Verdict, CreateGeofenceRequest,
    DeleteGeofenceRequest, DeleteGeofenceResponse, Geofence, GeofenceCheckRequest,
    GeofenceCheckResponse, ListGeofencesRequest, ListGeofencesResponse, LocationAck,
    LocationUpdate, NearbyRequest, NearbyResponse, NearbyUser, RouteScoreRequest,
    RouteScoreResponse, SafetyVoteRequest, SafetyVoteResponse, ScoredRoute,
};
use crate::queries;

// Defaults / caps applied in code (not the proto) so they are easy to
// audit and change without a proto bump.
const DEFAULT_NEARBY_LIMIT: u32 = 20;
const MAX_NEARBY_LIMIT: u32 = 100;
const MAX_RADIUS_M: f64 = 50_000.0; // 50 km — generous upper bound
const MAX_ROUTE_CANDIDATES: usize = 10;
/// Mirrors the gateway's `MAX_ROUTE_POINTS`. Enforced here as well because
/// the gateway is not the only thing that can reach this RPC, and the cost
/// this bounds is paid by the database rather than by the gateway.
const MAX_ROUTE_POINTS: usize = 2_000;

pub struct GeoEngineImpl {
    pool: PgPool,
    producer: LocationProducer,
}

impl GeoEngineImpl {
    pub fn new(pool: PgPool, producer: LocationProducer) -> Self {
        Self { pool, producer }
    }
}

fn parse_user_id(s: &str) -> Result<Uuid, Status> {
    Uuid::parse_str(s).map_err(|_| Status::invalid_argument("user_id is not a valid UUID"))
}

/// Parse the project the gateway injected.
///
/// Strict on purpose: this side of the boundary trusts its callers, so an
/// empty project_id cannot have come from a client — only from a bug in
/// the gateway or a service calling this one directly. Defaulting would
/// turn that bug into silent cross-tenant reads and writes; failing turns
/// it into an error someone has to look at.
fn parse_project_id(s: &str) -> Result<Uuid, Status> {
    if s.is_empty() {
        return Err(Status::invalid_argument(
            "project_id is required; the gateway must inject it",
        ));
    }
    Uuid::parse_str(s).map_err(|_| Status::invalid_argument("project_id is not a valid UUID"))
}

fn clamp_radius(r: f64) -> Result<f64, Status> {
    if !r.is_finite() || r <= 0.0 {
        return Err(Status::invalid_argument("radius_m must be > 0"));
    }
    Ok(r.min(MAX_RADIUS_M))
}

fn clamp_limit(limit: u32) -> i64 {
    let l = if limit == 0 {
        DEFAULT_NEARBY_LIMIT
    } else {
        limit.min(MAX_NEARBY_LIMIT)
    };
    l as i64
}

#[tonic::async_trait]
impl GeoEngine for GeoEngineImpl {
    async fn update_location(
        &self,
        req: Request<LocationUpdate>,
    ) -> Result<Response<LocationAck>, Status> {
        let started = Instant::now();
        let r = req.into_inner();
        let project_id = parse_project_id(&r.project_id)?;
        let user_id = parse_user_id(&r.user_id)?;
        // The proto-supplied timestamp, clamped into the retention window
        // [now - 24h, now + 5min]. Without the clamp a batched or
        // clock-skewed client could stamp a ping in the past — where the
        // reaper deletes it immediately — or in the far future, where it
        // would survive retention until the clock catches up. Zero means
        // "not set" and falls back to now.
        let now = Utc::now();
        let recorded_at = if r.recorded_at > 0 {
            let sent = Utc
                .timestamp_opt(r.recorded_at, 0)
                .single()
                .ok_or_else(|| Status::invalid_argument("recorded_at out of range"))?;
            let floor = now - chrono::Duration::hours(24);
            let ceil = now + chrono::Duration::minutes(5);
            sent.clamp(floor, ceil)
        } else {
            now
        };

        queries::locations::insert_location(
            &self.pool,
            project_id,
            user_id,
            r.lat,
            r.lng,
            recorded_at,
        )
        .await
        .map_err(|e| {
            warn!(error = %e, "insert_location failed");
            Status::internal("failed to record location")
        })?;
        metrics::histogram!("atlas_geo_update_location_duration_ms")
            .record(started.elapsed().as_secs_f64() * 1000.0);

        // Fire-and-forget Kafka enqueue. The row is in Postgres regardless.
        self.producer.enqueue(&LocationUpdateEvent {
            // The consumer reads this long after the request is gone and
            // has no other way to learn the tenant.
            project_id: project_id.to_string(),
            user_id: r.user_id.clone(),
            lat: r.lat,
            lng: r.lng,
            recorded_at: recorded_at.timestamp(),
        });
        metrics::counter!("atlas_location_updates_total").increment(1);

        Ok(Response::new(LocationAck { ok: true }))
    }

    async fn get_nearby(
        &self,
        req: Request<NearbyRequest>,
    ) -> Result<Response<NearbyResponse>, Status> {
        let r = req.into_inner();
        let project_id = parse_project_id(&r.project_id)?;
        let requester = parse_user_id(&r.requester_user_id)?;
        let radius = clamp_radius(r.radius_m)?;
        let limit = clamp_limit(r.limit);
        // Cardinatity-bounded label: the caller can send any string, so
        // map to a fixed vocabulary before it reaches the metric. An
        // unbounded label is a memory leak with a nice name.
        let role = match r.role.as_str() {
            "driver" => "driver",
            "walker" => "walker",
            _ => "unknown",
        };

        let start = Instant::now();
        let rows = queries::locations::nearby(
            &self.pool, project_id, r.lat, r.lng, radius, requester, limit,
        )
        .await
        .map_err(|e| {
            warn!(error = %e, "nearby query failed");
            Status::internal("nearby query failed")
        })?;
        metrics::histogram!(
            "atlas_geo_nearby_query_duration_ms",
            "role" => role,
        )
        .record(start.elapsed().as_secs_f64() * 1000.0);

        let users = rows
            .into_iter()
            .map(|row| NearbyUser {
                user_id: row.user_id.to_string(),
                lat: row.lat,
                lng: row.lng,
                distance_m: row.distance_m,
                safety_score: row.safety_score,
                safety_vote_count: row.safety_vote_count,
            })
            .collect();

        Ok(Response::new(NearbyResponse { users }))
    }

    async fn score_route(
        &self,
        req: Request<RouteScoreRequest>,
    ) -> Result<Response<RouteScoreResponse>, Status> {
        let r = req.into_inner();
        let project_id = parse_project_id(&r.project_id)?;
        if r.candidates.is_empty() {
            return Err(Status::invalid_argument("at least one candidate required"));
        }
        if r.candidates.len() > MAX_ROUTE_CANDIDATES {
            return Err(Status::invalid_argument(format!(
                "at most {MAX_ROUTE_CANDIDATES} candidates per request"
            )));
        }
        // Validate each candidate has at least 2 points (a single point is
        // not a route).
        for c in &r.candidates {
            if c.points.len() < 2 {
                return Err(Status::invalid_argument(
                    "each route candidate needs at least 2 points",
                ));
            }
            if c.points.len() > MAX_ROUTE_POINTS {
                return Err(Status::invalid_argument(format!(
                    "at most {MAX_ROUTE_POINTS} points per route candidate"
                )));
            }
        }

        let start = Instant::now();
        let candidate_count_bucket = match r.candidates.len() {
            1 => "1",
            2..=3 => "2-3",
            4..=5 => "4-5",
            _ => "6+",
        };

        let mut scored = Vec::with_capacity(r.candidates.len());
        for c in r.candidates {
            let points: Vec<(f64, f64)> = c.points.iter().map(|p| (p.lat, p.lng)).collect();
            let result = queries::safety::score_route_line(&self.pool, project_id, &points)
                .await
                .map_err(|e| {
                    warn!(error = %e, "score_route query failed");
                    Status::internal("route scoring failed")
                })?;
            scored.push(ScoredRoute {
                route_id: c.route_id,
                score: result.score,
                vote_count: result.vote_count,
            });
        }
        metrics::histogram!(
            "atlas_geo_score_route_duration_ms",
            "candidate_count" => candidate_count_bucket,
        )
        .record(start.elapsed().as_secs_f64() * 1000.0);

        // Best = highest safety score. Stable: first candidate wins on
        // ties, which is what makes the answer reproducible when no route
        // has any votes and every score is the neutral 1500.
        let best = scored
            .iter()
            .max_by(|a, b| {
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("non-empty after validation");

        Ok(Response::new(RouteScoreResponse {
            best_route_id: best.route_id.clone(),
            score: best.score,
            all_scores: scored,
        }))
    }

    async fn trigger_geofence_check(
        &self,
        req: Request<GeofenceCheckRequest>,
    ) -> Result<Response<GeofenceCheckResponse>, Status> {
        let r = req.into_inner();
        let project_id = parse_project_id(&r.project_id)?;
        let user_id = parse_user_id(&r.user_id)?;
        let start = Instant::now();
        let ids =
            queries::geofences::check_membership(&self.pool, project_id, user_id, r.lat, r.lng)
                .await
                .map_err(|e| {
                    warn!(error = %e, "geofence membership check failed");
                    Status::internal("geofence check failed")
                })?;
        metrics::histogram!("atlas_geo_geofence_check_duration_ms")
            .record(start.elapsed().as_secs_f64() * 1000.0);

        Ok(Response::new(GeofenceCheckResponse {
            triggered: !ids.is_empty(),
            geofence_ids: ids.into_iter().map(|id| id.to_string()).collect(),
        }))
    }

    async fn create_geofence(
        &self,
        req: Request<CreateGeofenceRequest>,
    ) -> Result<Response<Geofence>, Status> {
        let started = Instant::now();
        let r = req.into_inner();
        let project_id = parse_project_id(&r.project_id)?;
        let user_id = parse_user_id(&r.user_id)?;
        if !r.radius_m.is_finite() || r.radius_m <= 0.0 || r.radius_m > MAX_RADIUS_M {
            return Err(Status::invalid_argument(
                "radius_m must be > 0 and <= 50000",
            ));
        }
        let row = queries::geofences::create(
            &self.pool,
            project_id,
            user_id,
            &r.label,
            r.center_lat,
            r.center_lng,
            r.radius_m,
        )
        .await
        .map_err(|e| {
            warn!(error = %e, "create_geofence failed");
            // FK violation = unknown user_id; everything else is internal.
            if let sqlx::Error::Database(dbe) = &e {
                if dbe.constraint().is_some() {
                    return Status::failed_precondition("user_id does not exist");
                }
            }
            Status::internal("create_geofence failed")
        })?;

        info!(geofence_id = %row.id, user_id = %row.user_id, "geofence created");
        metrics::histogram!("atlas_geo_create_geofence_duration_ms")
            .record(started.elapsed().as_secs_f64() * 1000.0);
        Ok(Response::new(to_proto(row)))
    }

    async fn list_geofences(
        &self,
        req: Request<ListGeofencesRequest>,
    ) -> Result<Response<ListGeofencesResponse>, Status> {
        let started = Instant::now();
        let r = req.into_inner();
        let project_id = parse_project_id(&r.project_id)?;
        let user_id = parse_user_id(&r.user_id)?;
        let rows = queries::geofences::list(&self.pool, project_id, user_id, r.active_only)
            .await
            .map_err(|e| {
                warn!(error = %e, "list_geofences failed");
                Status::internal("list_geofences failed")
            })?;
        metrics::histogram!("atlas_geo_list_geofences_duration_ms")
            .record(started.elapsed().as_secs_f64() * 1000.0);
        Ok(Response::new(ListGeofencesResponse {
            geofences: rows.into_iter().map(to_proto).collect(),
        }))
    }

    async fn delete_geofence(
        &self,
        req: Request<DeleteGeofenceRequest>,
    ) -> Result<Response<DeleteGeofenceResponse>, Status> {
        let started = Instant::now();
        let r = req.into_inner();
        let project_id = parse_project_id(&r.project_id)?;
        let user_id = parse_user_id(&r.user_id)?;
        let geofence_id = Uuid::parse_str(&r.geofence_id)
            .map_err(|_| Status::invalid_argument("geofence_id is not a valid UUID"))?;
        let deleted = queries::geofences::deactivate(&self.pool, project_id, user_id, geofence_id)
            .await
            .map_err(|e| {
                warn!(error = %e, "delete_geofence failed");
                Status::internal("delete_geofence failed")
            })?;
        metrics::histogram!("atlas_geo_delete_geofence_duration_ms")
            .record(started.elapsed().as_secs_f64() * 1000.0);
        Ok(Response::new(DeleteGeofenceResponse { deleted }))
    }

    async fn cast_safety_vote(
        &self,
        req: Request<SafetyVoteRequest>,
    ) -> Result<Response<SafetyVoteResponse>, Status> {
        let started = Instant::now();
        let r = req.into_inner();
        let project_id = parse_project_id(&r.project_id)?;
        let user_id = parse_user_id(&r.user_id)?;

        // VERDICT_UNKNOWN is proto3's zero value, so it is what an unset
        // field arrives as. Counting it as either verdict would let a
        // client that forgot the field silently move scores.
        let verdict = match Verdict::try_from(r.verdict) {
            Ok(Verdict::Safe) => queries::safety::Verdict::Safe,
            Ok(Verdict::Unsafe) => queries::safety::Verdict::Unsafe,
            _ => return Err(Status::invalid_argument("verdict must be SAFE or UNSAFE")),
        };

        if !r.lat.is_finite() || !r.lng.is_finite() {
            return Err(Status::invalid_argument("lat and lng must be finite"));
        }

        let score =
            queries::safety::cast_vote(&self.pool, project_id, user_id, r.lat, r.lng, verdict)
                .await
                .map_err(|e| {
                    warn!(error = %e, "cast_safety_vote failed");
                    // An unknown user_id is a foreign-key violation, which
                    // is the caller's mistake rather than ours.
                    if let sqlx::Error::Database(dbe) = &e {
                        if dbe.constraint().is_some() {
                            return Status::failed_precondition("user_id does not exist");
                        }
                    }
                    Status::internal("cast_safety_vote failed")
                })?;

        metrics::counter!(
            "atlas_geo_safety_votes_total",
            "verdict" => match verdict {
                queries::safety::Verdict::Safe => "safe",
                queries::safety::Verdict::Unsafe => "unsafe",
            },
        )
        .increment(1);
        metrics::histogram!("atlas_geo_cast_safety_vote_duration_ms")
            .record(started.elapsed().as_secs_f64() * 1000.0);

        Ok(Response::new(SafetyVoteResponse {
            safety_score: score.score,
            vote_count: score.vote_count,
        }))
    }
}

fn to_proto(row: queries::geofences::GeofenceRow) -> Geofence {
    Geofence {
        id: row.id.to_string(),
        user_id: row.user_id.to_string(),
        label: row.label,
        center_lat: row.center_lat,
        center_lng: row.center_lng,
        radius_m: row.radius_m,
        active: row.active,
        created_at: row.created_at.timestamp(),
    }
}

#[cfg(test)]
mod tests {
    //! Pure unit tests for input validation helpers. No DB or Kafka.
    //! These run on every `cargo test` (no docker-compose required).
    use super::*;

    #[test]
    fn parse_user_id_accepts_uuid() {
        let id = parse_user_id("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(id.to_string(), "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn parse_user_id_rejects_garbage() {
        assert!(parse_user_id("not-a-uuid").is_err());
        assert!(parse_user_id("").is_err());
    }

    #[test]
    fn clamp_radius_rejects_nonpositive_and_nan() {
        assert!(clamp_radius(0.0).is_err());
        assert!(clamp_radius(-1.0).is_err());
        assert!(clamp_radius(f64::NAN).is_err());
        assert!(clamp_radius(f64::INFINITY).is_err());
    }

    #[test]
    fn clamp_radius_caps_at_max() {
        assert_eq!(clamp_radius(100.0).unwrap(), 100.0);
        assert_eq!(clamp_radius(MAX_RADIUS_M).unwrap(), MAX_RADIUS_M);
        assert_eq!(clamp_radius(MAX_RADIUS_M + 1.0).unwrap(), MAX_RADIUS_M);
        assert_eq!(clamp_radius(999_999.0).unwrap(), MAX_RADIUS_M);
    }

    #[test]
    fn clamp_limit_zero_becomes_default() {
        assert_eq!(clamp_limit(0), DEFAULT_NEARBY_LIMIT as i64);
    }

    #[test]
    fn clamp_limit_caps_at_max() {
        assert_eq!(clamp_limit(50), 50);
        assert_eq!(clamp_limit(MAX_NEARBY_LIMIT), MAX_NEARBY_LIMIT as i64);
        assert_eq!(clamp_limit(MAX_NEARBY_LIMIT + 1), MAX_NEARBY_LIMIT as i64);
        assert_eq!(clamp_limit(u32::MAX), MAX_NEARBY_LIMIT as i64);
    }
}

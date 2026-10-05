//! Wire types.
//!
//! Field names match the JSON exactly, so there is no mapping layer to
//! drift. Where the API's shape would be misleading in Rust — a 0 that
//! means "never" — the type says what it means instead.

use serde::{Deserialize, Serialize};

// --- auth -------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterResult {
    pub user_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    pub token: String,
    /// Unix seconds.
    pub expires_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    pub user_id: String,
    pub session_id: String,
    /// `(0, 0)` when no position was supplied at login — the wire has no
    /// nulls here.
    pub last_lat: f64,
    pub last_lng: f64,
    pub issued_at: i64,
    pub expires_at: i64,
    pub email: String,
    /// Unix seconds, or `None` when the address has never been confirmed.
    ///
    /// Gate features on `is_some()`. A timestamp rather than a bool
    /// because "when" is the question support conversations ask, and a
    /// bool cannot be widened into one later without having already lost
    /// the answer.
    pub email_verified_at: Option<i64>,
    pub created_at: i64,
}

// --- geo --------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Ack {
    pub ok: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NearbyUser {
    pub user_id: String,
    pub lat: f64,
    pub lng: f64,
    pub distance_m: f64,
    /// Safety score for the place this user is standing: 1000..2000,
    /// neutral 1500.
    ///
    /// Read it together with [`NearbyUser::safety_vote_count`]. 1500 from
    /// nobody voting and 1500 from a hundred evenly split voters are
    /// different facts, and a UI that renders them identically is claiming
    /// knowledge it does not have.
    pub safety_score: f64,
    pub safety_vote_count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Geofence {
    pub id: String,
    pub user_id: String,
    /// Empty string when unlabelled.
    pub label: String,
    pub center_lat: f64,
    pub center_lng: f64,
    pub radius_m: f64,
    pub active: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GeofenceCheck {
    pub triggered: bool,
    pub geofence_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LatLng {
    pub lat: f64,
    pub lng: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouteCandidate {
    pub route_id: String,
    pub points: Vec<LatLng>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScoredRoute {
    pub route_id: String,
    pub score: f64,
    pub vote_count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouteScore {
    pub best_route_id: String,
    pub score: f64,
    pub all_scores: Vec<ScoredRoute>,
}

/// One user's judgement about one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Safe,
    Unsafe,
}

impl Verdict {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Verdict::Safe => "safe",
            Verdict::Unsafe => "unsafe",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SafetyVote {
    pub safety_score: f64,
    pub vote_count: i64,
}

// --- payments ---------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Wallet {
    pub balance_cents: i64,
    pub currency: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Deposit {
    pub transaction_id: String,
    pub status: String,
    pub balance_cents: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Transaction {
    pub transaction_id: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OnboardingUrl {
    pub onboarding_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PayoutAccountStatus {
    pub exists: bool,
    pub payouts_enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Deleted {
    pub deleted: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserId {
    pub user_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct NearbyEnvelope {
    pub users: Vec<NearbyUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GeofenceEnvelope {
    pub geofences: Vec<Geofence>,
}

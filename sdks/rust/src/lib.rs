//! Rust client for the Atlas gateway.
//!
//! ```no_run
//! use atlas_sdk::{AtlasClient, ClientOptions};
//!
//! # async fn example() -> atlas_sdk::Result<()> {
//! let atlas = AtlasClient::new(ClientOptions {
//!     base_url: "https://api.atlas.dev".into(),
//!     project_key: std::env::var("ATLAS_KEY").unwrap(),
//!     ..Default::default()
//! })?;
//!
//! atlas.auth().login("rider@example.com", "hunter2!").await?;  // token stored
//! let users = atlas.geo().nearby(51.5074, -0.1278, 500.0).await?;
//! let wallet = atlas.payments().wallet().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Two credentials
//!
//! Every call carries both, and they answer different questions. The
//! `project_key` says which application is calling — it is yours, it stays
//! on your server, and it never changes between users. The bearer token
//! says which of your users is calling; [`AuthApi::login`] obtains it and
//! stores it on the client. Neither substitutes for the other.
//!
//! **The project key is a server-side secret.** Anyone holding it can act
//! on your whole project, so keep this client on your backend.
//!
//! # Identity
//!
//! No method takes a user id or a project id. The gateway derives both
//! from the credentials above, and its request bodies have no fields for
//! them — that absence is what stops one caller acting as another. An SDK
//! that accepted a user id would imply a capability the API does not have.

mod error;
mod http;
mod types;

pub use error::{Error, ErrorCode, Result};
pub use types::*;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use http::{Http, Request};

/// How the client talks to Atlas.
///
/// `Debug` is implemented by hand so the project key does not appear in
/// logs; the rest of the configuration is printed normally.
#[derive(Clone)]
pub struct ClientOptions {
    /// Gateway origin, e.g. `https://api.atlas.dev`. The `/v1` prefix is
    /// added for you.
    pub base_url: String,
    /// Your project key, `atl_live_…`. Required.
    pub project_key: String,
    /// An existing bearer token, to resume a session.
    pub token: Option<String>,
    /// Per-request timeout. Defaults to 10s, matching the gateway's own
    /// upstream deadline — a client timeout shorter than the server's
    /// turns slow-but-successful calls into errors the server never sees.
    pub timeout: Duration,
    /// Retries for safe requests only. Defaults to 2.
    pub max_retries: u32,
}

impl std::fmt::Debug for ClientOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientOptions")
            .field("base_url", &self.base_url)
            .field("project_key", &"<redacted>")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            project_key: String::new(),
            token: None,
            timeout: Duration::from_secs(10),
            max_retries: 2,
        }
    }
}

/// Client for the Atlas gateway. Cheap to clone; clones share a token.
#[derive(Clone)]
pub struct AtlasClient {
    // Debug is implemented by hand below rather than derived: deriving it
    // would print the project key into any log line that formats a client.
    http: Arc<Http>,
    token: Arc<RwLock<Option<String>>>,
}

impl std::fmt::Debug for AtlasClient {
    /// Deliberately says nothing about credentials.
    ///
    /// A derived Debug would print the project key and the bearer token
    /// the first time anyone logs a client, and both are secrets that end
    /// up in aggregators nobody audits.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtlasClient")
            .field("authenticated", &self.token().is_some())
            .finish_non_exhaustive()
    }
}

impl AtlasClient {
    pub fn new(options: ClientOptions) -> Result<Self> {
        if options.project_key.is_empty() {
            // Fail here rather than letting every call come back 401. A
            // missing key is a configuration mistake, and the error should
            // point at the line that has to change.
            return Err(Error::InvalidRequest(
                "project_key is required (atl_live_… / atl_test_… / atl_dev_…)".into(),
            ));
        }
        if options.base_url.is_empty() {
            return Err(Error::InvalidRequest("base_url is required".into()));
        }

        let token = Arc::new(RwLock::new(options.token));
        let http = Http::new(
            options.base_url,
            options.project_key,
            options.timeout,
            options.max_retries,
            Arc::clone(&token),
        )?;
        Ok(Self {
            http: Arc::new(http),
            token,
        })
    }

    pub fn auth(&self) -> AuthApi<'_> {
        AuthApi { client: self }
    }

    pub fn geo(&self) -> GeoApi<'_> {
        GeoApi { client: self }
    }

    pub fn payments(&self) -> PaymentsApi<'_> {
        PaymentsApi { client: self }
    }

    /// The current bearer token, if any.
    pub fn token(&self) -> Option<String> {
        self.token.read().expect("token lock poisoned").clone()
    }

    /// Set or clear the token. `login` and `logout` do this for you.
    pub fn set_token(&self, token: Option<String>) {
        *self.token.write().expect("token lock poisoned") = token;
    }
}

// --- auth -------------------------------------------------------------------

pub struct AuthApi<'a> {
    client: &'a AtlasClient,
}

impl AuthApi<'_> {
    /// Create a user in the calling project.
    ///
    /// The same address in two projects is two different people, so this
    /// conflicts only within one project.
    pub async fn register(&self, email: &str, password: &str) -> Result<RegisterResult> {
        self.client
            .http
            .send(
                Request::post("/v1/auth/register")
                    .anonymous()
                    .json(serde_json::json!({ "email": email, "password": password })),
            )
            .await
    }

    /// Exchange credentials for a token, which is stored on the client.
    pub async fn login(&self, email: &str, password: &str) -> Result<Session> {
        let session: Session = self
            .client
            .http
            .send(
                Request::post("/v1/auth/login")
                    .anonymous()
                    .json(serde_json::json!({ "email": email, "password": password })),
            )
            .await?;
        self.client.set_token(Some(session.token.clone()));
        Ok(session)
    }

    /// As [`login`](Self::login), with a position stamped into the token's
    /// claims.
    pub async fn login_at(
        &self,
        email: &str,
        password: &str,
        lat: f64,
        lng: f64,
    ) -> Result<Session> {
        let session: Session = self
            .client
            .http
            .send(Request::post("/v1/auth/login").anonymous().json(
                serde_json::json!({ "email": email, "password": password, "lat": lat, "lng": lng }),
            ))
            .await?;
        self.client.set_token(Some(session.token.clone()));
        Ok(session)
    }

    /// Revoke the current session and forget the token.
    pub async fn logout(&self) -> Result<()> {
        let result: Result<serde_json::Value> = self
            .client
            .http
            .send(Request::post("/v1/auth/logout"))
            .await;
        // Cleared regardless of the outcome: the token is either revoked
        // or was already invalid, and keeping it helps nobody.
        self.client.set_token(None);
        result.map(|_| ())
    }

    /// The current session and the caller's profile.
    pub async fn me(&self) -> Result<Me> {
        self.client.http.send(Request::get("/v1/auth/me")).await
    }

    /// Ask Atlas to mail this address a password reset link.
    ///
    /// Succeeds the same way whether or not the address has an account —
    /// the server deliberately does not say, since an endpoint that did
    /// would let anyone test a list of addresses for which have accounts.
    /// Do not build a UI that claims the address was found.
    pub async fn request_password_reset(&self, email: &str) -> Result<()> {
        self.client
            .http
            .send::<serde_json::Value>(
                Request::post("/v1/auth/password-reset")
                    .anonymous()
                    .json(serde_json::json!({ "email": email })),
            )
            .await
            .map(|_| ())
    }

    /// Redeem a reset token and set a new password.
    ///
    /// Needs no session: whoever holds the emailed token is, for this one
    /// call, the account's owner. Succeeding revokes every session the
    /// user had — including this client's, so the stored token is cleared
    /// and you must log in again.
    pub async fn reset_password(&self, token: &str, new_password: &str) -> Result<UserId> {
        let result: UserId = self
            .client
            .http
            .send(
                Request::post("/v1/auth/password-reset/confirm")
                    .anonymous()
                    .json(serde_json::json!({ "token": token, "new_password": new_password })),
            )
            .await?;
        self.client.set_token(None);
        Ok(result)
    }

    /// Ask Atlas to mail this address a verification link. Also silent
    /// about whether the address exists.
    pub async fn request_email_verification(&self, email: &str) -> Result<()> {
        self.client
            .http
            .send::<serde_json::Value>(
                Request::post("/v1/auth/email/verify")
                    .anonymous()
                    .json(serde_json::json!({ "email": email })),
            )
            .await
            .map(|_| ())
    }

    /// Redeem a verification token.
    ///
    /// Unlike a reset this leaves sessions alone: confirming an address is
    /// not evidence that anything leaked.
    pub async fn verify_email(&self, token: &str) -> Result<UserId> {
        self.client
            .http
            .send(
                Request::post("/v1/auth/email/verify/confirm")
                    .anonymous()
                    .json(serde_json::json!({ "token": token })),
            )
            .await
    }
}

// --- geo --------------------------------------------------------------------

pub struct GeoApi<'a> {
    client: &'a AtlasClient,
}

impl GeoApi<'_> {
    /// Record the caller's position.
    ///
    /// [`recorded_at`] is unix seconds for when the fix was taken, not
    /// sent. The server clamps it to its retention window; without it the
    /// server stamps arrival time, which for batched or offline uploads
    /// would start the 24h retention clock late.
    pub async fn update_location(
        &self,
        lat: f64,
        lng: f64,
        recorded_at: Option<i64>,
    ) -> Result<Ack> {
        let mut body = serde_json::json!({ "lat": lat, "lng": lng });
        if let Some(at) = recorded_at {
            body["recorded_at"] = serde_json::Value::from(at);
        }
        self.client
            .http
            .send(Request::post("/v1/geo/locations").json(body))
            .await
    }

    /// Users within `radius_m` metres, nearest first.
    ///
    /// Scoped to the calling project: it never returns another customer's
    /// users, even standing on the same coordinates.
    pub async fn nearby(&self, lat: f64, lng: f64, radius_m: f64) -> Result<Vec<NearbyUser>> {
        let envelope: NearbyEnvelope = self
            .client
            .http
            .send(
                Request::get("/v1/geo/nearby")
                    .query("lat", lat)
                    .query("lng", lng)
                    .query("radius_m", radius_m),
            )
            .await?;
        Ok(envelope.users)
    }

    /// As [`nearby`](Self::nearby), with an application-defined role
    /// filter and a result cap (1..=100).
    pub async fn nearby_filtered(
        &self,
        lat: f64,
        lng: f64,
        radius_m: f64,
        role: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<NearbyUser>> {
        let envelope: NearbyEnvelope = self
            .client
            .http
            .send(
                Request::get("/v1/geo/nearby")
                    .query("lat", lat)
                    .query("lng", lng)
                    .query("radius_m", radius_m)
                    .maybe_query("role", role)
                    .maybe_query("limit", limit),
            )
            .await?;
        Ok(envelope.users)
    }

    /// Rank route candidates by the safety votes along them.
    pub async fn score_route(&self, candidates: Vec<RouteCandidate>) -> Result<RouteScore> {
        if candidates.is_empty() {
            return Err(Error::InvalidRequest(
                "at least one route candidate is required".into(),
            ));
        }
        self.client
            .http
            .send(
                Request::post("/v1/geo/routes/score")
                    .json(serde_json::json!({ "candidates": candidates })),
            )
            .await
    }

    pub async fn create_geofence(
        &self,
        label: &str,
        center_lat: f64,
        center_lng: f64,
        radius_m: f64,
    ) -> Result<Geofence> {
        self.client
            .http
            .send(Request::post("/v1/geo/geofences").json(serde_json::json!({
                "label": label,
                "center_lat": center_lat,
                "center_lng": center_lng,
                "radius_m": radius_m,
            })))
            .await
    }

    pub async fn list_geofences(&self, active_only: bool) -> Result<Vec<Geofence>> {
        let envelope: GeofenceEnvelope = self
            .client
            .http
            .send(Request::get("/v1/geo/geofences").query("active_only", active_only))
            .await?;
        Ok(envelope.geofences)
    }

    /// Deactivate one of the caller's geofences.
    ///
    /// A fence belonging to someone else is a [`ErrorCode::NotFound`], the
    /// same answer as an id that never existed.
    pub async fn delete_geofence(&self, id: &str) -> Result<Deleted> {
        self.client
            .http
            .send(Request::delete(format!("/v1/geo/geofences/{id}")))
            .await
    }

    pub async fn check_geofences(&self, lat: f64, lng: f64) -> Result<GeofenceCheck> {
        self.client
            .http
            .send(
                Request::post("/v1/geo/geofences/check")
                    .json(serde_json::json!({ "lat": lat, "lng": lng })),
            )
            .await
    }

    /// Record the caller's judgement about a place.
    ///
    /// Voting again in the same area replaces your previous verdict rather
    /// than adding to it: one user is one voter however often they vote.
    pub async fn cast_safety_vote(
        &self,
        lat: f64,
        lng: f64,
        verdict: Verdict,
    ) -> Result<SafetyVote> {
        self.client
            .http
            .send(
                Request::post("/v1/geo/safety/votes").json(serde_json::json!({
                    "lat": lat,
                    "lng": lng,
                    "verdict": verdict.as_str(),
                })),
            )
            .await
    }
}

// --- payments ---------------------------------------------------------------

pub struct PaymentsApi<'a> {
    client: &'a AtlasClient,
}

impl PaymentsApi<'_> {
    pub async fn wallet(&self) -> Result<Wallet> {
        self.client
            .http
            .send(Request::get("/v1/payments/wallet"))
            .await
    }

    /// Add funds to the caller's wallet.
    ///
    /// The idempotency key is generated when omitted, and it is what makes
    /// this POST safe for the transport to retry.
    pub async fn deposit(
        &self,
        amount_cents: i64,
        idempotency_key: Option<&str>,
    ) -> Result<Deposit> {
        let key = idempotency_key
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        self.client
            .http
            .send(
                Request::post("/v1/payments/deposits")
                    .replayable()
                    .json(serde_json::json!({
                        "amount_cents": amount_cents,
                        "idempotency_key": key,
                    })),
            )
            .await
    }

    /// Move funds from the caller to another user.
    ///
    /// Creates a PENDING transaction. The money moves later, when your
    /// application publishes the ride lifecycle event that settles it —
    /// there is deliberately no settle endpoint, because the gateway
    /// cannot verify the caller owns a transaction.
    pub async fn create_transaction(
        &self,
        to_user_id: &str,
        amount_cents: i64,
        ride_id: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> Result<Transaction> {
        let key = idempotency_key
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mut body = serde_json::json!({
            "to_user_id": to_user_id,
            "amount_cents": amount_cents,
            "idempotency_key": key,
        });
        if let Some(ride) = ride_id {
            body["ride_id"] = serde_json::Value::String(ride.to_string());
        }
        self.client
            .http
            .send(
                Request::post("/v1/payments/transactions")
                    .replayable()
                    .json(body),
            )
            .await
    }

    /// Start payout onboarding for the CALLER's own account.
    ///
    /// The returned URL is single-use and short-lived: redirect the user
    /// immediately. The account being onboarded is the token subject's,
    /// so a caller can only ever set up payouts for themselves.
    pub async fn start_payout_onboarding(&self, return_url: &str) -> Result<OnboardingUrl> {
        self.client
            .http
            .send(
                Request::post("/v1/payments/connected-accounts").json(serde_json::json!({
                    "return_url": return_url,
                })),
            )
            .await
    }

    /// The caller's own payout-account status. `payouts_enabled` is what
    /// gates whether a transaction naming this user as payee can succeed.
    pub async fn payout_account(&self) -> Result<PayoutAccountStatus> {
        self.client
            .http
            .send(Request::get("/v1/payments/connected-accounts"))
            .await
    }
}

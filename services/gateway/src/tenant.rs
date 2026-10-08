//! Tenant resolution: which project does this request belong to?
//!
//! # Two credentials, two questions
//!
//! Atlas requests carry two independent identities and it is important not
//! to conflate them:
//!
//!   * `Authorization: Bearer <jwt>` — WHO the end user is. Issued by
//!     auth-service to a person using the developer's app. Handled by
//!     [`crate::extract::AuthUser`].
//!   * `X-Atlas-Key: atl_live_…` — WHICH CUSTOMER'S APPLICATION is asking.
//!     Issued by the control plane to a developer, and held server-side by
//!     their backend. Handled here.
//!
//! A user token says nothing about the project, and a project key says
//! nothing about the user. Both are required on every `/v1` route, which
//! is why this is a layer rather than a per-handler extractor: a new route
//! added to any of the `/v1` sub-routers is covered without anyone
//! remembering to cover it. Forgetting a tenant check is not the kind of
//! mistake that shows up in testing — it shows up as one customer reading
//! another's data.
//!
//! # Why this reads Postgres directly
//!
//! The obvious alternative is a `ResolveKey` RPC on the control plane. It
//! was rejected because it would put the control-plane PROCESS on the
//! critical path of every data-plane request: a control-plane deploy or
//! crash would take the whole API down with it. Postgres is already a hard
//! dependency of every backend, so reading `control.api_keys` here adds no
//! availability edge that does not already exist. The cost is that the
//! gateway knows one table in the control schema, which is a narrower and
//! more visible coupling than a new runtime dependency.
//!
//! The query is read-only and touches a single unique index.
//!
//! # Caching
//!
//! Results are cached in-process for a short TTL, mirroring the 30s
//! auth-service keeps for token validation. Negative results are cached
//! too, and that is not an optimisation: without it, a flood of invalid
//! keys becomes a flood of Postgres queries, and the cheapest possible
//! request for an attacker to send would be the most expensive one for us
//! to answer.
//!
//! The TTL is the revocation window. `atlas keys revoke` takes effect
//! within `PROJECT_CACHE_TTL_SECONDS` rather than instantly, which is the
//! same bargain the token cache already makes.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ApiError;

/// The header carrying the project key. Named after the platform rather
/// than reusing `Authorization`, which already carries the user token.
pub const KEY_HEADER: &str = "x-atlas-key";

/// The customer whose application is making this request.
#[derive(Debug, Clone)]
pub struct Tenant {
    pub project_id: Uuid,
    pub account_id: Uuid,
    /// `development` | `staging` | `production`, from the project row.
    /// Backends do not branch on this yet; it is here so that when they
    /// do, the value comes from the key rather than from the caller.
    pub environment: String,
    /// Which key was presented, for audit logging. Never the key itself.
    pub key_id: Uuid,
    /// Display prefix, e.g. `atl_live_9f3c`. Safe to log.
    pub key_prefix: String,
}

#[derive(Clone)]
enum Cached {
    Resolved(Tenant),
    /// The key did not resolve. The reason is deliberately not kept: every
    /// failure returns one message, so there is nothing to distinguish.
    Rejected,
}

struct Entry {
    value: Cached,
    stored_at: Instant,
}

/// Digest-keyed cache in front of `control.api_keys`.
pub struct ProjectCache {
    entries: RwLock<HashMap<String, Entry>>,
    ttl: Duration,
    negative_ttl: Duration,
}

impl ProjectCache {
    pub fn new(ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            entries: RwLock::new(HashMap::new()),
            // Shorter than the positive TTL. A key that does not exist is
            // unlikely to start existing, but a key that was rejected
            // because the project was mid-creation should not stay
            // rejected for the full window.
            negative_ttl: ttl / 3,
            ttl,
        })
    }

    fn ttl_for(&self, value: &Cached) -> Duration {
        match value {
            Cached::Resolved(_) => self.ttl,
            Cached::Rejected => self.negative_ttl,
        }
    }

    fn get(&self, digest: &str) -> Option<Cached> {
        let entries = self.entries.read().ok()?;
        let entry = entries.get(digest)?;
        if entry.stored_at.elapsed() < self.ttl_for(&entry.value) {
            Some(entry.value.clone())
        } else {
            None
        }
    }

    fn put(&self, digest: String, value: Cached) {
        if let Ok(mut entries) = self.entries.write() {
            entries.insert(
                digest,
                Entry {
                    value,
                    stored_at: Instant::now(),
                },
            );
        }
    }

    /// Drop expired entries. Without this the map grows with every
    /// distinct key ever presented, which for the negative half means an
    /// attacker chooses how much memory the gateway uses.
    pub fn sweep(&self) {
        if let Ok(mut entries) = self.entries.write() {
            entries.retain(|_, e| e.stored_at.elapsed() < self.ttl_for(&e.value));
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.read().map(|e| e.len()).unwrap_or(0)
    }

    /// Sweep on a timer for the lifetime of the process.
    pub fn spawn_gc(self: &Arc<Self>) {
        let cache = Arc::clone(self);
        let period = cache.ttl.max(Duration::from_secs(30));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            loop {
                ticker.tick().await;
                cache.sweep();
            }
        });
    }
}

/// Pull the project key out of the headers, rejecting anything that is not
/// shaped like one before it can cost a database round trip.
fn presented_key(headers: &HeaderMap) -> Result<&str, ApiError> {
    let raw = headers
        .get(KEY_HEADER)
        .ok_or_else(|| ApiError::Unauthorized(format!("missing {KEY_HEADER} header")))?
        .to_str()
        .map_err(|_| ApiError::Unauthorized(format!("{KEY_HEADER} is not valid UTF-8")))?;

    if !atlas_keys::looks_like_key(raw) {
        return Err(ApiError::Unauthorized("malformed project key".to_string()));
    }
    Ok(raw)
}

/// Resolve a presented key to its project, consulting the cache first.
pub async fn resolve(pool: &PgPool, cache: &ProjectCache, key: &str) -> Result<Tenant, ApiError> {
    let digest = atlas_keys::hash(key);

    if let Some(hit) = cache.get(&digest) {
        metrics::counter!("atlas_gateway_project_cache_total", "result" => "hit").increment(1);
        return match hit {
            Cached::Resolved(t) => Ok(t),
            Cached::Rejected => Err(rejected()),
        };
    }
    metrics::counter!("atlas_gateway_project_cache_total", "result" => "miss").increment(1);

    // One indexed lookup. `status` and `expires_at` are checked in SQL so
    // a revoked key never becomes a Tenant value in the first place.
    // Scope is checked here too (D4): the data plane serves `data` keys;
    // `admin` keys are for control-plane mutations and never carry user
    // traffic.
    let row = sqlx::query_as::<_, (Uuid, Uuid, Uuid, String, String)>(
        r#"
        SELECT k.id, k.account_id, p.id, p.environment, k.key_prefix
        FROM control.api_keys k
        -- An INNER join on a non-null project_id is what excludes
        -- account-scoped keys: those exist so `atlas deploy` can create a
        -- project that does not exist yet, and they name no project, so
        -- there is no tenant for them to be. They belong on the control
        -- plane and nowhere near the data plane.
        JOIN control.projects p ON p.id = k.project_id
        WHERE k.key_hash = $1
          AND k.status = 'active'
          AND k.scope = 'data'
          AND (k.expires_at IS NULL OR k.expires_at > NOW())
        "#,
    )
    .bind(&digest)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        // A database failure is not an authentication failure, and must
        // not be cached as one — otherwise a brief Postgres blip would
        // lock every caller out for the negative TTL.
        tracing::error!(error = %e, "project key lookup failed");
        ApiError::upstream(
            "control",
            tonic::Status::unavailable("project lookup unavailable"),
        )
    })?;

    match row {
        Some((key_id, account_id, project_id, environment, key_prefix)) => {
            let tenant = Tenant {
                project_id,
                account_id,
                environment,
                key_id,
                key_prefix,
            };
            cache.put(digest, Cached::Resolved(tenant.clone()));
            Ok(tenant)
        }
        None => {
            cache.put(digest, Cached::Rejected);
            Err(rejected())
        }
    }
}

/// One message for "no such key", "revoked", "expired", and
/// "account-scoped key used on the data plane" alike. Distinguishing them
/// would tell someone holding a random string whether it happened to name
/// a real key.
fn rejected() -> ApiError {
    ApiError::Unauthorized("invalid project key".to_string())
}

/// Middleware over every `/v1` route.
///
/// Applied to the routers as a whole rather than to individual handlers so
/// that a route added later cannot be tenant-unaware by omission.
pub async fn layer(
    State(state): State<crate::state::AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let key = match presented_key(request.headers()) {
        Ok(key) => key.to_string(),
        Err(e) => return e.into_response(),
    };

    let tenant = match resolve(&state.pool, &state.projects, &key).await {
        Ok(tenant) => tenant,
        Err(e) => return e.into_response(),
    };

    // Handlers read this through the `Tenant` extractor. It is inserted
    // here and nowhere else, so it can only have come from a resolved key.
    request.extensions_mut().insert(tenant);
    next.run(request).await
}

/// Handler-facing view of the resolved tenant.
///
/// Reads what [`layer`] inserted. The `Internal` arm is unreachable while
/// every `/v1` router carries the layer — it exists so that wiring a route
/// without it fails as a 500 with a loud log rather than silently serving
/// an unscoped request.
#[axum::async_trait]
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Tenant {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Tenant>().cloned().ok_or_else(|| {
            tracing::error!("tenant extractor used on a route without the tenant layer");
            ApiError::upstream("gateway", tonic::Status::internal("tenant context missing"))
        })
    }
}

/// Middleware that installs a fixed tenant without consulting anything.
///
/// This exists so router-level tests can exercise routing, user
/// authentication, and rate limiting without standing up Postgres — those
/// tests are about what the gateway decides before it talks to anything,
/// and making them database-backed would trade their whole point for
/// coverage they do not provide.
///
/// It is never reachable in production: [`crate::routes::router`], the
/// only constructor `main.rs` calls, always installs [`layer`]. The test
/// `production_router_requires_a_project_key` asserts exactly that.
pub async fn fixed_layer(
    State(tenant): State<Tenant>,
    mut request: Request,
    next: Next,
) -> Response {
    request.extensions_mut().insert(tenant);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(KEY_HEADER, HeaderValue::from_str(value).unwrap());
        h
    }

    fn tenant() -> Tenant {
        Tenant {
            project_id: Uuid::new_v4(),
            account_id: Uuid::new_v4(),
            environment: "development".to_string(),
            key_id: Uuid::new_v4(),
            key_prefix: "atl_dev_abcd".to_string(),
        }
    }

    #[test]
    fn a_missing_header_is_rejected_without_a_lookup() {
        let err = presented_key(&HeaderMap::new()).unwrap_err();
        assert!(matches!(err, ApiError::Unauthorized(_)));
    }

    /// The shape check exists so junk cannot cost a query. If it ever
    /// stops rejecting these, every malformed request becomes a database
    /// round trip.
    #[test]
    fn junk_is_rejected_before_the_database() {
        for junk in [
            "",
            "hunter2",
            "Bearer atl_live_0123456789abcdef0123456789abcdef",
            // Right scheme, too short to be a real key.
            "atl_live_abc",
            // A well-formed key from some other issuer.
            "zzz_live_0123456789abcdef0123456789abcdef",
        ] {
            assert!(
                presented_key(&headers_with(junk)).is_err(),
                "{junk:?} must not reach the database"
            );
        }
    }

    #[test]
    fn a_well_formed_key_passes_the_shape_check() {
        let key = atlas_keys::generate("production").plaintext;
        assert_eq!(presented_key(&headers_with(&key)).unwrap(), key);
    }

    #[test]
    fn a_resolved_key_is_served_from_cache_within_the_ttl() {
        let cache = ProjectCache::new(Duration::from_secs(60));
        let t = tenant();
        cache.put("digest".to_string(), Cached::Resolved(t.clone()));

        match cache.get("digest") {
            Some(Cached::Resolved(hit)) => assert_eq!(hit.project_id, t.project_id),
            _ => panic!("expected a cached hit"),
        }
    }

    /// Negative caching is the DoS protection, so it gets its own test:
    /// a rejected key must not be re-queried on every request.
    #[test]
    fn a_rejected_key_is_also_cached() {
        let cache = ProjectCache::new(Duration::from_secs(60));
        cache.put("digest".to_string(), Cached::Rejected);
        assert!(matches!(cache.get("digest"), Some(Cached::Rejected)));
    }

    /// Rejections expire sooner than resolutions, so a key rejected during
    /// a race with project creation recovers quickly.
    #[test]
    fn rejections_expire_sooner_than_resolutions() {
        let cache = ProjectCache::new(Duration::from_secs(30));
        assert!(cache.ttl_for(&Cached::Rejected) < cache.ttl_for(&Cached::Resolved(tenant())));
    }

    #[test]
    fn an_expired_entry_is_a_miss() {
        let cache = ProjectCache::new(Duration::from_millis(1));
        cache.put("digest".to_string(), Cached::Resolved(tenant()));
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.get("digest").is_none());
    }

    /// Without a sweep, the negative half of the cache lets whoever is
    /// sending bad keys decide how much memory this process uses.
    #[test]
    fn sweeping_drops_expired_entries() {
        let cache = ProjectCache::new(Duration::from_millis(1));
        for i in 0..100 {
            cache.put(format!("digest-{i}"), Cached::Rejected);
        }
        assert_eq!(cache.len(), 100);
        std::thread::sleep(Duration::from_millis(5));
        cache.sweep();
        assert_eq!(cache.len(), 0, "expired entries must not accumulate");
    }

    #[test]
    fn sweeping_keeps_live_entries() {
        let cache = ProjectCache::new(Duration::from_secs(60));
        cache.put("live".to_string(), Cached::Resolved(tenant()));
        cache.sweep();
        assert_eq!(cache.len(), 1);
    }
}

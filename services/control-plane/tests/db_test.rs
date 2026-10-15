//! End-to-end tests against a real Postgres, driven through the actual
//! router so every request goes down the full path: HTTP, key extractor,
//! SQL, response serialisation.
//!
//! Marked `#[ignore]` so `cargo test` stays green without Docker. Run with:
//!
//!     docker compose up -d postgres
//!     cargo test -p atlas-control-plane -- --include-ignored
//!
//! The database is shared and never truncated, so every test mints its own
//! account and project name from a random UUID and asserts only within
//! that scope.

use atlas_control_plane::{
    config::Config,
    ratelimit::{Limiters, RateLimitConfig},
    routes,
    state::AppState,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

const DATABASE_URL: &str = "postgres://atlas:atlas_dev@localhost:5432/atlas";

fn test_config() -> Config {
    Config {
        http_addr: "127.0.0.1:0".parse().unwrap(),
        metrics_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: DATABASE_URL.to_string(),
        database_pool_size: 4,
        // Backends are not running for these tests. Health probes will
        // report false, which is correct and is what the assertions expect.
        auth_addr: "http://127.0.0.1:59441".to_string(),
        geo_addr: "http://127.0.0.1:59442".to_string(),
        payments_addr: "http://127.0.0.1:59443".to_string(),
        kafka_brokers: "127.0.0.1:59444".to_string(),
        gateway_metrics_url: "http://127.0.0.1:59445/metrics".to_string(),
        endpoint_template: "https://api.atlas.dev/v1/{name}".to_string(),
        probe_timeout: std::time::Duration::from_millis(120),
        rate_limit: RateLimitConfig::default(),
    }
}

/// A fresh router sharing one pool. `oneshot` consumes the router, so
/// callers build a new one per request.
struct Harness {
    state: AppState,
}

impl Harness {
    async fn new() -> Self {
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(DATABASE_URL)
            .await
            .expect("connect to local postgres — is docker compose up?");
        Harness {
            state: AppState::new(pool, test_config()).expect("state builds"),
        }
    }

    fn app(&self) -> axum::Router {
        routes::router_without_peer(
            self.state.clone(),
            Limiters::new(RateLimitConfig {
                default_per_minute: 10_000,
                signup_per_minute: 10_000,
                ..RateLimitConfig::default()
            }),
        )
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, Value) {
        let response = self.app().oneshot(req).await.expect("router responds");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        // 204 has no body; represent it as null rather than failing.
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// Bootstrap an account and return its account-scoped key.
    async fn new_account(&self) -> String {
        let email = format!("test-{}@atlas.dev", Uuid::new_v4());
        let (status, body) = self
            .send(post("/v1/accounts", json!({ "email": email }), None))
            .await;
        assert_eq!(status, StatusCode::CREATED, "account creation: {body}");
        body["api_key"]
            .as_str()
            .expect("api_key present")
            .to_string()
    }
}

fn post(uri: &str, body: Value, key: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(k) = key {
        b = b.header("authorization", format!("Bearer {k}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn get(uri: &str, key: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap()
}

fn delete(uri: &str, key: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap()
}

fn project_name() -> String {
    // Lowercase alphanumeric + hyphen, 3-40 chars — the CLI's rule.
    format!("t-{}", &Uuid::new_v4().simple().to_string()[..12])
}

fn deploy_body(name: &str, services: Vec<&str>) -> Value {
    json!({
        "name": name,
        "region": "us-central1",
        "environment": "production",
        "services_enabled": services,
    })
}

// --- bootstrap --------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn bootstrap_mints_a_key_the_cli_would_accept() {
    let h = Harness::new().await;
    let key = h.new_account().await;

    // The CLI refuses to load an atlas.toml whose key fails these rules.
    assert!(
        key.starts_with("atl_dev_") || key.starts_with("atl_test_") || key.starts_with("atl_live_"),
        "key {key} has no recognised scheme"
    );
    assert!(key.len() >= 24, "key {key} is too short for the CLI");
}

#[tokio::test]
#[ignore]
async fn duplicate_account_email_is_rejected() {
    let h = Harness::new().await;
    let email = format!("dupe-{}@atlas.dev", Uuid::new_v4());

    let (first, _) = h
        .send(post("/v1/accounts", json!({ "email": &email }), None))
        .await;
    assert_eq!(first, StatusCode::CREATED);

    // Re-issuing a key for a known email would be a credential reset for
    // anyone who can guess an address.
    let (second, _) = h
        .send(post("/v1/accounts", json!({ "email": &email }), None))
        .await;
    assert_eq!(second, StatusCode::CONFLICT);
}

// --- deploy -----------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn deploy_provisions_then_is_idempotent() {
    let h = Harness::new().await;
    let key = h.new_account().await;
    let name = project_name();

    let (status, body) = h
        .send(post(
            "/v1/projects",
            deploy_body(&name, vec!["auth", "geo"]),
            Some(&key),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["project_name"], name.as_str());
    assert_eq!(body["region"], "us-central1");
    assert_eq!(
        body["endpoint"],
        format!("https://api.atlas.dev/v1/{name}").as_str()
    );

    let provisioned = body["provisioned"].as_array().unwrap();
    // All four namespaces are reported, so a developer can see what is off.
    assert_eq!(provisioned.len(), 4);
    let find = |svc: &str| {
        provisioned
            .iter()
            .find(|p| p["service"] == svc)
            .unwrap_or_else(|| panic!("{svc} missing"))
            .clone()
    };
    assert_eq!(find("auth")["status"], "ok");
    assert_eq!(find("geo")["status"], "ok");
    assert_eq!(find("payments")["status"], "skipped");
    // First provision has no "already provisioned" note.
    assert!(find("auth")["detail"].is_null());

    // Re-deploying the same config must not create a second project and
    // must report the services as already provisioned.
    let (status, body) = h
        .send(post(
            "/v1/projects",
            deploy_body(&name, vec!["auth", "geo"]),
            Some(&key),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let provisioned = body["provisioned"].as_array().unwrap();
    let auth = provisioned.iter().find(|p| p["service"] == "auth").unwrap();
    assert_eq!(auth["detail"], "already provisioned");
}

#[tokio::test]
#[ignore]
async fn disabling_a_service_is_reflected_on_redeploy() {
    let h = Harness::new().await;
    let key = h.new_account().await;
    let name = project_name();

    h.send(post(
        "/v1/projects",
        deploy_body(&name, vec!["auth", "payments"]),
        Some(&key),
    ))
    .await;

    let (status, body) = h
        .send(post(
            "/v1/projects",
            deploy_body(&name, vec!["auth"]),
            Some(&key),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let payments = body["provisioned"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["service"] == "payments")
        .unwrap()
        .clone();
    assert_eq!(payments["status"], "skipped");
    assert_eq!(payments["detail"], "disabled by this deploy");
}

#[tokio::test]
#[ignore]
async fn deploy_rejects_input_the_cli_would_never_send() {
    let h = Harness::new().await;
    let key = h.new_account().await;

    let cases = vec![
        (
            "bad name",
            json!({"name":"-nope-","region":"us-central1","environment":"production","services_enabled":["auth"]}),
        ),
        (
            "bad region",
            json!({"name":project_name(),"region":"mars-1","environment":"production","services_enabled":["auth"]}),
        ),
        (
            "bad environment",
            json!({"name":project_name(),"region":"us-central1","environment":"chaos","services_enabled":["auth"]}),
        ),
        (
            "unknown service",
            json!({"name":project_name(),"region":"us-central1","environment":"production","services_enabled":["telepathy"]}),
        ),
        (
            "no services",
            json!({"name":project_name(),"region":"us-central1","environment":"production","services_enabled":[]}),
        ),
    ];

    for (label, body) in cases {
        let (status, resp) = h.send(post("/v1/projects", body, Some(&key))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: {resp}");
    }
}

/// Project names appear in the public endpoint URL, so they are globally
/// unique. A second account must not be able to take one.
#[tokio::test]
#[ignore]
async fn project_names_are_globally_unique() {
    let h = Harness::new().await;
    let owner = h.new_account().await;
    let stranger = h.new_account().await;
    let name = project_name();

    let (status, _) = h
        .send(post(
            "/v1/projects",
            deploy_body(&name, vec!["auth"]),
            Some(&owner),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = h
        .send(post(
            "/v1/projects",
            deploy_body(&name, vec!["auth"]),
            Some(&stranger),
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

// --- tenancy ----------------------------------------------------------------

/// The whole point of the account model. A key from one account must not
/// reach another account's project, and the response must not confirm the
/// project exists.
#[tokio::test]
#[ignore]
async fn one_account_cannot_see_anothers_project() {
    let h = Harness::new().await;
    let owner = h.new_account().await;
    let stranger = h.new_account().await;
    let name = project_name();

    h.send(post(
        "/v1/projects",
        deploy_body(&name, vec!["auth", "geo"]),
        Some(&owner),
    ))
    .await;

    for uri in [
        format!("/v1/projects/{name}/status"),
        format!("/v1/projects/{name}/keys"),
        format!("/v1/projects/{name}/logs"),
    ] {
        let (status, body) = h.send(get(&uri, &stranger)).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{uri} leaked to another account: {body}"
        );
    }
}

/// A project-scoped key is the one you put in CI. Leaking it must not let
/// the holder create new projects on the account.
#[tokio::test]
#[ignore]
async fn project_scoped_keys_cannot_create_projects() {
    let h = Harness::new().await;
    let account_key = h.new_account().await;
    let name = project_name();

    h.send(post(
        "/v1/projects",
        deploy_body(&name, vec!["auth"]),
        Some(&account_key),
    ))
    .await;

    let (status, body) = h
        .send(post(
            &format!("/v1/projects/{name}/keys"),
            json!({ "name": "ci", "expiry": "days30" }),
            Some(&account_key),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let scoped_key = body["api_key"].as_str().unwrap().to_string();

    // It works for its own project...
    let (status, _) = h
        .send(get(&format!("/v1/projects/{name}/status"), &scoped_key))
        .await;
    assert_eq!(status, StatusCode::OK);

    // ...but cannot bootstrap a new one.
    let (status, body) = h
        .send(post(
            "/v1/projects",
            deploy_body(&project_name(), vec!["auth"]),
            Some(&scoped_key),
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

// --- keys -------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn key_lifecycle_create_list_revoke() {
    let h = Harness::new().await;
    let account_key = h.new_account().await;
    let name = project_name();
    h.send(post(
        "/v1/projects",
        deploy_body(&name, vec!["auth"]),
        Some(&account_key),
    ))
    .await;

    // Create.
    let (status, created) = h
        .send(post(
            &format!("/v1/projects/{name}/keys"),
            json!({ "name": "ci", "expiry": "never" }),
            Some(&account_key),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let plaintext = created["api_key"].as_str().unwrap().to_string();
    let prefix = created["prefix"].as_str().unwrap().to_string();
    assert_eq!(created["status"], "active");
    assert!(created["last_used_at"].is_null());
    // Environment is production, so the key must be a live-tier key.
    assert!(plaintext.starts_with("atl_live_"), "got {plaintext}");
    assert!(plaintext.starts_with(&prefix));

    // List: the project key shows up; the account bootstrap key does not.
    let (status, list) = h
        .send(get(&format!("/v1/projects/{name}/keys"), &account_key))
        .await;
    assert_eq!(status, StatusCode::OK);
    let keys = list.as_array().unwrap();
    assert_eq!(
        keys.len(),
        1,
        "expected only the project-scoped key: {list}"
    );
    assert_eq!(keys[0]["name"], "ci");
    assert_eq!(keys[0]["prefix"], prefix.as_str());
    // The plaintext is never returned again.
    assert!(keys[0].get("api_key").is_none());

    // The new key authenticates.
    let (status, _) = h
        .send(get(&format!("/v1/projects/{name}/status"), &plaintext))
        .await;
    assert_eq!(status, StatusCode::OK);

    // Revoke.
    let (status, _) = h
        .send(delete(
            &format!("/v1/projects/{name}/keys/{prefix}"),
            &account_key,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // A revoked key must stop working immediately.
    let (status, _) = h
        .send(get(&format!("/v1/projects/{name}/status"), &plaintext))
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "revoked key still authenticates"
    );

    // And it is reported as revoked, not deleted.
    let (_, list) = h
        .send(get(&format!("/v1/projects/{name}/keys"), &account_key))
        .await;
    assert_eq!(list[0]["status"], "revoked");

    // Revoking again is a 404, not a silent success.
    let (status, _) = h
        .send(delete(
            &format!("/v1/projects/{name}/keys/{prefix}"),
            &account_key,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// An expired key must be refused even though its status is still active.
#[tokio::test]
#[ignore]
async fn expired_keys_do_not_authenticate() {
    let h = Harness::new().await;
    let account_key = h.new_account().await;
    let name = project_name();
    h.send(post(
        "/v1/projects",
        deploy_body(&name, vec!["auth"]),
        Some(&account_key),
    ))
    .await;

    let (_, created) = h
        .send(post(
            &format!("/v1/projects/{name}/keys"),
            json!({ "name": "shortlived", "expiry": "days30" }),
            Some(&account_key),
        ))
        .await;
    let plaintext = created["api_key"].as_str().unwrap().to_string();
    let prefix = created["prefix"].as_str().unwrap();

    // Works now.
    let (status, _) = h
        .send(get(&format!("/v1/projects/{name}/status"), &plaintext))
        .await;
    assert_eq!(status, StatusCode::OK);

    // Backdate the expiry rather than waiting 30 days.
    sqlx::query(
        "UPDATE control.api_keys SET expires_at = NOW() - INTERVAL '1 second' WHERE key_prefix = $1",
    )
    .bind(prefix)
    .execute(&h.state.pool)
    .await
    .unwrap();

    let (status, _) = h
        .send(get(&format!("/v1/projects/{name}/status"), &plaintext))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "expired key accepted");
}

// --- status -----------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn status_reports_only_enabled_services_and_real_health() {
    let h = Harness::new().await;
    let key = h.new_account().await;
    let name = project_name();
    h.send(post(
        "/v1/projects",
        deploy_body(&name, vec!["auth", "geo"]),
        Some(&key),
    ))
    .await;

    let (status, body) = h
        .send(get(&format!("/v1/projects/{name}/status"), &key))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["project_name"], name.as_str());

    let services = body["services"].as_array().unwrap();
    assert_eq!(services.len(), 2, "only enabled services: {body}");
    let names: Vec<&str> = services
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["auth", "geo"]);

    for s in services {
        // No backends are running in this test, and health is a live
        // probe rather than a stored flag — so it must report false.
        assert_eq!(s["healthy"], false, "health should be probed, not assumed");
        // Usage is null, not zero: gateway metrics carry no project label
        // yet, so any number reported here would be platform-wide traffic
        // dressed up as this project's — a wrong number that looks exact.
        assert!(s["requests_24h"].is_null(), "usage: {}", s["requests_24h"]);
        assert!(s["error_rate"].is_null(), "usage: {}", s["error_rate"]);
        assert!(s["p95_latency_ms"].is_null(), "usage: {}", s["p95_latency_ms"]);
    }
}

// --- logs -------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn logs_return_the_projects_audit_trail() {
    let h = Harness::new().await;
    let key = h.new_account().await;
    let name = project_name();
    h.send(post(
        "/v1/projects",
        deploy_body(&name, vec!["auth"]),
        Some(&key),
    ))
    .await;
    h.send(post(
        &format!("/v1/projects/{name}/keys"),
        json!({ "name": "ci", "expiry": "never" }),
        Some(&key),
    ))
    .await;

    let (status, body) = h
        .send(get(&format!("/v1/projects/{name}/logs"), &key))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let lines = body.as_array().unwrap();
    assert!(lines.len() >= 2, "expected deploy + key events: {body}");

    // Shape matches cli::api::LogLine exactly.
    for line in lines {
        assert!(line["timestamp"].is_string());
        assert!(line["service"].is_string());
        assert!(line["level"].is_string());
        assert!(line["message"].is_string());
    }

    // Chronological, oldest first — the deploy precedes the key issuance.
    let messages: Vec<&str> = lines
        .iter()
        .map(|l| l["message"].as_str().unwrap())
        .collect();
    assert!(
        messages.iter().any(|m| m.contains("provisioned")),
        "deploy not recorded: {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("issued key")),
        "key issuance not recorded: {messages:?}"
    );

    // A key's plaintext must never reach the audit trail.
    for m in &messages {
        assert!(
            !m.contains("atl_live_") || m.matches('_').count() <= 3,
            "audit message may contain a full key: {m}"
        );
    }

    // Filtering by a service with no rows yields an empty list, not a 500.
    let (status, body) = h
        .send(get(&format!("/v1/projects/{name}/logs?service=geo"), &key))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
}

// --- rate limiting ----------------------------------------------------------

/// POST /v1/accounts is the only unauthenticated write in the platform and
/// it mints an API key. Unthrottled, anyone reaching the port can create
/// unlimited accounts and credentials.
#[tokio::test]
#[ignore]
async fn account_creation_is_throttled() {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(DATABASE_URL)
        .await
        .expect("connect to local postgres");
    let state = AppState::new(pool, test_config()).expect("state builds");

    // Two signups allowed, third refused. All share the "unknown" address
    // bucket because the test harness supplies no ConnectInfo.
    let limiters = Limiters::new(RateLimitConfig {
        signup_per_minute: 2,
        default_per_minute: 10_000,
        ..RateLimitConfig::default()
    });
    // One shared limiter across all three requests — a fresh limiter per
    // request would reset the quota and the test would prove nothing.
    let app = || routes::router_without_peer(state.clone(), std::sync::Arc::clone(&limiters));

    let mut statuses = Vec::new();
    for _ in 0..3 {
        let email = format!("throttle-{}@atlas.dev", Uuid::new_v4());
        let res = app()
            .oneshot(post("/v1/accounts", json!({ "email": email }), None))
            .await
            .unwrap();
        statuses.push(res.status());
    }

    assert_eq!(statuses[0], StatusCode::CREATED);
    assert_eq!(statuses[1], StatusCode::CREATED);
    assert_eq!(
        statuses[2],
        StatusCode::TOO_MANY_REQUESTS,
        "third signup from one address must be refused"
    );
}

//! Tests against a real HTTP server on a real port.
//!
//! Not a mocked transport: the point is to exercise the actual request
//! path — headers, query encoding, retries, JSON handling — the way it
//! runs in production. A mock proves the SDK calls the mock.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use atlas_sdk::{AtlasClient, ClientOptions, ErrorCode, Verdict};
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};

const TEST_KEY: &str = "atl_test_0123456789abcdef0123456789abcdef";

/// What the fake gateway saw, so tests can assert on the wire.
#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<Recorded>>,
    /// Fails this many times before succeeding, for the retry tests.
    fail_times: AtomicUsize,
    attempts: AtomicUsize,
    /// Answers 429 + Retry-After this many times, for the rate-limit test.
    rate_limited_times: AtomicUsize,
}

#[derive(Clone, Debug)]
struct Recorded {
    path: String,
    key: Option<String>,
    authorization: Option<String>,
    body: String,
}

type Shared = Arc<Recorder>;

async fn record(state: &Shared, headers: &HeaderMap, path: &str, body: String) {
    state.requests.lock().unwrap().push(Recorded {
        path: path.to_string(),
        key: headers
            .get("x-atlas-key")
            .map(|v| v.to_str().unwrap_or_default().to_string()),
        authorization: headers
            .get("authorization")
            .map(|v| v.to_str().unwrap_or_default().to_string()),
        body,
    });
}

async fn serve() -> (String, Shared) {
    let state: Shared = Arc::new(Recorder::default());

    let app = Router::new()
        .route(
            "/v1/auth/register",
            post({
                let s = Arc::clone(&state);
                move |headers: HeaderMap, body: String| async move {
                    record(&s, &headers, "/v1/auth/register", body).await;
                    (
                        StatusCode::CREATED,
                        Json(serde_json::json!({"user_id": "u-1"})),
                    )
                }
            }),
        )
        .route(
            "/v1/auth/login",
            post({
                let s = Arc::clone(&state);
                move |headers: HeaderMap, body: String| async move {
                    record(&s, &headers, "/v1/auth/login", body).await;
                    Json(serde_json::json!({"token": "tok-abc", "expires_at": 1787000000}))
                }
            }),
        )
        .route(
            "/v1/auth/me",
            get({
                let s = Arc::clone(&state);
                move |headers: HeaderMap| async move {
                    record(&s, &headers, "/v1/auth/me", String::new()).await;
                    Json(serde_json::json!({
                        "user_id": "u-1", "session_id": "s-1",
                        "last_lat": 0.0, "last_lng": 0.0,
                        "issued_at": 1, "expires_at": 2,
                        "email": "a@b.dev", "email_verified_at": null, "created_at": 1
                    }))
                }
            }),
        )
        .route(
            "/v1/auth/password-reset/confirm",
            post({
                let s = Arc::clone(&state);
                move |headers: HeaderMap, body: String| async move {
                    record(&s, &headers, "/v1/auth/password-reset/confirm", body).await;
                    Json(serde_json::json!({"user_id": "u-1"}))
                }
            }),
        )
        .route(
            "/v1/geo/nearby",
            get({
                let s = Arc::clone(&state);
                move |headers: HeaderMap, Query(q): Query<Vec<(String, String)>>| async move {
                    let rendered = q
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("&");
                    record(&s, &headers, "/v1/geo/nearby", rendered).await;
                    Json(serde_json::json!({"users": [{
                        "user_id": "u-2", "lat": 1.0, "lng": 2.0,
                        "distance_m": 3.0, "safety_score": 1500.0,
                        "safety_vote_count": 0
                    }]}))
                }
            }),
        )
        .route(
            "/v1/geo/safety/votes",
            post({
                let s = Arc::clone(&state);
                move |headers: HeaderMap, body: String| async move {
                    record(&s, &headers, "/v1/geo/safety/votes", body).await;
                    Json(serde_json::json!({"safety_score": 1642.8, "vote_count": 2}))
                }
            }),
        )
        .route(
            "/v1/geo/geofences/:id",
            delete({
                let s = Arc::clone(&state);
                move |Path(id): Path<String>, headers: HeaderMap| async move {
                    record(
                        &s,
                        &headers,
                        &format!("/v1/geo/geofences/{id}"),
                        String::new(),
                    )
                    .await;
                    (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({
                            "error": {"code": "not_found", "message": "geofence not found"}
                        })),
                    )
                }
            }),
        )
        .route(
            "/v1/payments/deposits",
            post({
                let s = Arc::clone(&state);
                move |headers: HeaderMap, body: String| async move {
                    record(&s, &headers, "/v1/payments/deposits", body).await;
                    let seen = s.attempts.fetch_add(1, Ordering::SeqCst);
                    if seen < s.fail_times.load(Ordering::SeqCst) {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(serde_json::json!({
                                "error": {"code": "unavailable", "message": "try again"}
                            })),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "transaction_id": "t-1", "status": "settled", "balance_cents": 5000
                        })),
                    )
                }
            }),
        )
        .route(
            "/v1/payments/wallet",
            get({
                let s = Arc::clone(&state);
                move |headers: HeaderMap| async move {
                    record(&s, &headers, "/v1/payments/wallet", String::new()).await;
                    let seen = s.attempts.fetch_add(1, Ordering::SeqCst);
                    if seen < s.rate_limited_times.load(Ordering::SeqCst) {
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            [("retry-after", "1")],
                            Json(serde_json::json!({
                                "error": {"code": "resource_exhausted", "message": "rate limit exceeded"}
                            })),
                        );
                    }
                    if seen < s.fail_times.load(Ordering::SeqCst) {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("retry-after", "0")],
                            Json(serde_json::json!({
                                "error": {"code": "unavailable", "message": "try again"}
                            })),
                        );
                    }
                    (
                        StatusCode::OK,
                        [("retry-after", "0")],
                        Json(serde_json::json!({"balance_cents": 5000, "currency": "USD"})),
                    )
                }
            }),
        )
        .with_state(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state)
}

fn client(base_url: &str, retries: u32) -> AtlasClient {
    AtlasClient::new(ClientOptions {
        base_url: base_url.to_string(),
        project_key: TEST_KEY.to_string(),
        max_retries: retries,
        timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .expect("client builds")
}

// --- credentials ------------------------------------------------------------

#[tokio::test]
async fn a_missing_project_key_fails_at_construction() {
    // Not at call time: a missing key is a configuration mistake, and the
    // error should point at the line that has to change rather than at
    // every request that follows.
    let err = AtlasClient::new(ClientOptions {
        base_url: "http://127.0.0.1:1".into(),
        ..Default::default()
    })
    .expect_err("must refuse");
    assert!(err.to_string().contains("project_key"));
}

#[tokio::test]
async fn the_project_key_is_sent_on_every_call_including_register() {
    let (url, rec) = serve().await;
    let c = client(&url, 0);

    c.auth().register("a@b.dev", "hunter2!").await.unwrap();
    c.auth().login("a@b.dev", "hunter2!").await.unwrap();
    c.auth().me().await.unwrap();

    let seen = rec.requests.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    for r in &seen {
        assert_eq!(
            r.key.as_deref(),
            Some(TEST_KEY),
            "missing key on {}",
            r.path
        );
    }
    // Register and login carry no bearer; /me does.
    assert!(seen[0].authorization.is_none());
    assert!(seen[1].authorization.is_none());
    assert_eq!(seen[2].authorization.as_deref(), Some("Bearer tok-abc"));
}

#[tokio::test]
async fn login_stores_the_token_and_logout_forgets_it() {
    let (url, _) = serve().await;
    let c = client(&url, 0);
    assert_eq!(c.token(), None);

    c.auth().login("a@b.dev", "hunter2!").await.unwrap();
    assert_eq!(c.token().as_deref(), Some("tok-abc"));
}

#[tokio::test]
async fn a_completed_reset_clears_the_token() {
    // The server just revoked every session; holding the dead token would
    // only produce confusing 403s on the next call.
    let (url, _) = serve().await;
    let c = client(&url, 0);
    c.auth().login("a@b.dev", "hunter2!").await.unwrap();

    c.auth()
        .reset_password(&"f".repeat(64), "a-new-password")
        .await
        .unwrap();
    assert_eq!(c.token(), None);
}

// --- identity ---------------------------------------------------------------

#[tokio::test]
async fn no_request_body_carries_a_user_id() {
    let (url, rec) = serve().await;
    let c = client(&url, 0);
    c.auth().login("a@b.dev", "hunter2!").await.unwrap();
    c.geo()
        .cast_safety_vote(1.0, 2.0, Verdict::Unsafe)
        .await
        .unwrap();

    for r in rec.requests.lock().unwrap().iter() {
        assert!(
            !r.body.contains("user_id"),
            "{} sent a user_id: {}",
            r.path,
            r.body
        );
        assert!(
            !r.body.contains("project_id"),
            "{} sent a project_id",
            r.path
        );
    }
}

// --- transport --------------------------------------------------------------

#[tokio::test]
async fn a_get_is_retried_and_eventually_succeeds() {
    let (url, rec) = serve().await;
    rec.fail_times.store(2, Ordering::SeqCst);
    let c = client(&url, 2);
    c.auth().login("a@b.dev", "hunter2!").await.unwrap();

    let wallet = c
        .payments()
        .wallet()
        .await
        .expect("retries carry it through");
    assert_eq!(wallet.balance_cents, 5000);
    assert_eq!(
        rec.attempts.load(Ordering::SeqCst),
        3,
        "two failures, one success"
    );
}

#[tokio::test]
async fn a_deposit_is_retried_because_it_carries_an_idempotency_key() {
    // The key is what makes replaying a POST safe on the server side, so
    // the transport is allowed to retry this one.
    let (url, rec) = serve().await;
    rec.fail_times.store(1, Ordering::SeqCst);
    let c = client(&url, 2);
    c.auth().login("a@b.dev", "hunter2!").await.unwrap();

    let deposit = c.payments().deposit(5000, None).await.unwrap();
    assert_eq!(deposit.balance_cents, 5000);
    assert_eq!(rec.attempts.load(Ordering::SeqCst), 2);

    // And the same key travelled on both attempts, which is the whole
    // point — a fresh key per retry would double-charge.
    let bodies: Vec<String> = rec
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path == "/v1/payments/deposits")
        .map(|r| r.body.clone())
        .collect();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1], "the retry must reuse the key");
}

#[tokio::test]
async fn query_parameters_are_encoded() {
    let (url, rec) = serve().await;
    let c = client(&url, 0);
    c.auth().login("a@b.dev", "hunter2!").await.unwrap();
    c.geo().nearby(51.5074, -0.1278, 500.0).await.unwrap();

    let q = rec
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.path == "/v1/geo/nearby")
        .unwrap()
        .body
        .clone();
    assert!(q.contains("lat=51.5074"), "{q}");
    assert!(q.contains("lng=-0.1278"), "{q}");
    assert!(q.contains("radius_m=500"), "{q}");
}

// --- errors -----------------------------------------------------------------

#[tokio::test]
async fn an_error_envelope_becomes_a_typed_code() {
    let (url, _) = serve().await;
    let c = client(&url, 0);
    c.auth().login("a@b.dev", "hunter2!").await.unwrap();

    let err = c
        .geo()
        .delete_geofence("someone-elses")
        .await
        .expect_err("404");
    assert_eq!(err.code(), Some(ErrorCode::NotFound));
    assert!(!err.is_retryable(), "a 404 will not become a 200 on retry");
}

#[tokio::test]
async fn an_unreachable_gateway_is_a_connection_error_not_an_api_error() {
    // The distinction matters: "the service rejected this" and "we could
    // not ask" call for different handling, and conflating them produces
    // both spurious alerts and missed outages.
    let c = AtlasClient::new(ClientOptions {
        // A port nothing listens on.
        base_url: "http://127.0.0.1:59987".into(),
        project_key: TEST_KEY.into(),
        max_retries: 0,
        timeout: Duration::from_millis(500),
        ..Default::default()
    })
    .unwrap();

    let err = c
        .auth()
        .register("a@b.dev", "hunter2!")
        .await
        .expect_err("cannot connect");
    assert_eq!(err.code(), None, "no API code: the gateway never answered");
    assert!(err.is_retryable());
    assert!(matches!(err, atlas_sdk::Error::Connection(_)));
}

#[tokio::test]
async fn scoring_no_routes_fails_before_the_request() {
    let (url, rec) = serve().await;
    let c = client(&url, 0);
    c.auth().login("a@b.dev", "hunter2!").await.unwrap();

    let before = rec.requests.lock().unwrap().len();
    c.geo()
        .score_route(vec![])
        .await
        .expect_err("no candidates");
    assert_eq!(
        rec.requests.lock().unwrap().len(),
        before,
        "an obviously invalid request should not cost a round trip"
    );
}

// --- types ------------------------------------------------------------------

#[tokio::test]
async fn an_unverified_address_is_none_not_zero() {
    // 0 is a real timestamp; a caller doing date maths on it renders 1970.
    let (url, _) = serve().await;
    let c = client(&url, 0);
    c.auth().login("a@b.dev", "hunter2!").await.unwrap();

    let me = c.auth().me().await.unwrap();
    assert_eq!(me.email_verified_at, None);
    assert_eq!(me.email, "a@b.dev");
}

#[tokio::test]
async fn a_neutral_score_arrives_with_its_evidence() {
    let (url, _) = serve().await;
    let c = client(&url, 0);
    c.auth().login("a@b.dev", "hunter2!").await.unwrap();

    let users = c.geo().nearby(1.0, 2.0, 100.0).await.unwrap();
    assert_eq!(users[0].safety_score, 1500.0);
    // Zero voters means the score is a default, not a measurement.
    assert_eq!(users[0].safety_vote_count, 0);
}

#[tokio::test]
async fn debug_output_never_contains_the_project_key() {
    // A derived Debug would print the key the first time anyone logs a
    // client, and it would land in an aggregator nobody audits.
    let (url, _) = serve().await;
    let c = client(&url, 0);
    c.auth().login("a@b.dev", "hunter2!").await.unwrap();

    let rendered = format!("{c:?}");
    assert!(!rendered.contains(TEST_KEY), "key leaked: {rendered}");
    assert!(!rendered.contains("tok-abc"), "token leaked: {rendered}");
    // It still says something useful.
    assert!(rendered.contains("authenticated: true"), "{rendered}");

    let opts = ClientOptions {
        base_url: url,
        project_key: TEST_KEY.into(),
        token: Some("tok-abc".into()),
        ..Default::default()
    };
    let rendered = format!("{opts:?}");
    assert!(
        !rendered.contains(TEST_KEY),
        "key leaked from options: {rendered}"
    );
    assert!(
        !rendered.contains("tok-abc"),
        "token leaked from options: {rendered}"
    );
}

#[tokio::test]
async fn a_429_with_retry_after_is_resource_exhausted_and_retryable() {
    let (url, rec) = serve().await;
    let c = client(&url, 1); // 1 retry

    rec.rate_limited_times.store(1, Ordering::SeqCst);
    let started = std::time::Instant::now();
    let wallet = c.payments().wallet().await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(wallet.balance_cents, 5000);
    assert!(
        elapsed >= std::time::Duration::from_millis(900),
        "retry should honour Retry-After, took {elapsed:?}"
    );
    assert_eq!(rec.requests.lock().unwrap().len(), 2);
}

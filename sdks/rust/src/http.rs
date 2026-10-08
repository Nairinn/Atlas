//! The transport: headers, retries, timeouts.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::error::{api_error, Error, Result};

pub(crate) const KEY_HEADER: &str = "X-Atlas-Key";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
    Delete,
}

impl Method {
    /// Whether a request may be replayed after a failure.
    ///
    /// GET and DELETE are idempotent by definition. POST is not, and this
    /// SDK will not retry one — with a single exception the caller opts
    /// into by supplying an idempotency key, which is what makes the
    /// replay safe on the server side.
    fn idempotent(self) -> bool {
        matches!(self, Method::Get | Method::Delete)
    }

    fn as_reqwest(self) -> reqwest::Method {
        match self {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Delete => reqwest::Method::DELETE,
        }
    }
}

pub(crate) struct Request {
    pub method: Method,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<serde_json::Value>,
    /// Set when the body carries an idempotency key, which makes a POST
    /// safe to replay.
    pub idempotent_post: bool,
    pub authenticated: bool,
}

impl Request {
    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::Get, path)
    }

    pub fn post(path: impl Into<String>) -> Self {
        Self::new(Method::Post, path)
    }

    pub fn delete(path: impl Into<String>) -> Self {
        Self::new(Method::Delete, path)
    }

    fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            query: Vec::new(),
            body: None,
            idempotent_post: false,
            // Most routes need one. The three that do not opt out
            // explicitly, so forgetting is a 401 rather than an
            // accidentally anonymous request.
            authenticated: true,
        }
    }

    pub fn anonymous(mut self) -> Self {
        self.authenticated = false;
        self
    }

    pub fn json(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }

    pub fn replayable(mut self) -> Self {
        self.idempotent_post = true;
        self
    }

    pub fn query(mut self, key: &str, value: impl ToString) -> Self {
        self.query.push((key.to_string(), value.to_string()));
        self
    }

    pub fn maybe_query(self, key: &str, value: Option<impl ToString>) -> Self {
        match value {
            Some(v) => self.query(key, v),
            None => self,
        }
    }
}

pub(crate) struct Http {
    client: reqwest::Client,
    base_url: String,
    project_key: String,
    max_retries: u32,
    token: Arc<RwLock<Option<String>>>,
}

impl Http {
    pub fn new(
        base_url: String,
        project_key: String,
        timeout: Duration,
        max_retries: u32,
        token: Arc<RwLock<Option<String>>>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| Error::InvalidRequest(format!("could not build HTTP client: {e}")))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            project_key,
            max_retries,
            token,
        })
    }

    pub async fn send<T: DeserializeOwned>(&self, req: Request) -> Result<T> {
        let body = self.send_raw(req).await?;
        // An empty 2xx body is legal for endpoints that return nothing.
        // `serde_json::from_str::<()>("")` fails, so null stands in.
        let text = if body.trim().is_empty() {
            "null"
        } else {
            &body
        };
        serde_json::from_str(text).map_err(|e| Error::Decode(e.to_string()))
    }

    async fn send_raw(&self, req: Request) -> Result<String> {
        let url = format!("{}{}", self.base_url, req.path);
        let attempts = if req.method.idempotent() || req.idempotent_post {
            self.max_retries + 1
        } else {
            1
        };

        let mut last: Option<Error> = None;
        let mut retry_after_ms: Option<u64> = None;
        for attempt in 0..attempts {
            if attempt > 0 {
                // Exponential backoff with jitter. Without jitter, every
                // client that failed together retries together, and the
                // recovering service is hit by a synchronised wave.
                // A Retry-After the server sent (429) overrides the
                // guess, capped so a hostile value cannot stall the caller.
                let base = 100u64 << (attempt - 1);
                let jitter = fastrand_u64() % (base / 2 + 1);
                let wait = (retry_after_ms.unwrap_or(0)).max(base) + jitter;
                tokio::time::sleep(Duration::from_millis(wait)).await;
                retry_after_ms = None;
            }

            let mut builder = self
                .client
                .request(req.method.as_reqwest(), &url)
                // Sent on every request, including register and login:
                // creating a user means creating them in a project.
                .header(KEY_HEADER, &self.project_key)
                .query(&req.query);

            if req.authenticated {
                if let Some(token) = self.token.read().expect("token lock poisoned").clone() {
                    builder = builder.bearer_auth(token);
                }
            }
            if let Some(ref body) = req.body {
                builder = builder.json(body);
            }

            match builder.send().await {
                Ok(response) => {
                    let status = response.status();
                    let hint = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_retry_after_ms);
                    let text = response.text().await.unwrap_or_default();
                    if status.is_success() {
                        return Ok(text);
                    }
                    retry_after_ms = hint;
                    let err = api_error(status.as_u16(), &text);
                    if !err.is_retryable() || attempt + 1 == attempts {
                        return Err(err);
                    }
                    last = Some(err);
                }
                Err(e) => {
                    let err = if e.is_timeout() {
                        Error::Connection(format!("timed out after {:?}", e.url()))
                    } else {
                        Error::Connection(e.to_string())
                    };
                    if attempt + 1 == attempts {
                        return Err(err);
                    }
                    last = Some(err);
                }
            }
        }
        Err(last.unwrap_or_else(|| Error::Connection("no attempt was made".into())))
    }
}

/// A tiny xorshift, to avoid a dependency for jitter.
///
/// Not cryptographic and does not need to be: it decorrelates retry
/// timing, and nothing about that is a secret.
fn fastrand_u64() -> u64 {
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|s| {
        let mut x = s.get();
        if x == 0 {
            x = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x2545_F491_4F6C_DD1D)
                | 1;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}

/// The longest a server hint may delay a retry.
const MAX_RETRY_AFTER_MS: u64 = 30_000;

/// Seconds form only, capped at 30s so a hostile or buggy value cannot
/// stall a caller indefinitely. The HTTP-date form is ignored: Atlas's
/// own limiters send seconds, and a misread hint degrades to the normal
/// backoff rather than a wrong wait.
fn parse_retry_after_ms(header: &str) -> Option<u64> {
    let secs = header.trim().parse::<u64>().ok()?;
    Some(secs.saturating_mul(1000).min(MAX_RETRY_AFTER_MS))
}

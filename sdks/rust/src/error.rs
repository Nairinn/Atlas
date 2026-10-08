//! Errors, and the distinction between "the server said no" and "we never
//! reached the server".

use std::fmt;

/// The stable `code` from the gateway's error envelope.
///
/// Branch on this, never on the message: messages are written for humans
/// and change without warning, while these are part of the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidArgument,
    Unauthenticated,
    PermissionDenied,
    NotFound,
    AlreadyExists,
    FailedPrecondition,
    /// The 429 the gateway's own limiter emits (`resource_exhausted`).
    ResourceExhausted,
    /// The upstream took longer than its deadline.
    DeadlineExceeded,
    Unavailable,
    Internal,
    /// A code this SDK version does not know.
    ///
    /// Present so a gateway that adds a code does not turn into a parse
    /// failure here — an SDK that refuses to deserialize an unfamiliar
    /// error is strictly worse at reporting errors than one that passes it
    /// through.
    Unknown,
}

impl ErrorCode {
    fn from_wire(s: &str) -> Self {
        match s {
            "invalid_argument" => Self::InvalidArgument,
            "unauthenticated" => Self::Unauthenticated,
            "permission_denied" => Self::PermissionDenied,
            "not_found" => Self::NotFound,
            "already_exists" => Self::AlreadyExists,
            "failed_precondition" => Self::FailedPrecondition,
            "resource_exhausted" => Self::ResourceExhausted,
            "deadline_exceeded" => Self::DeadlineExceeded,
            "unavailable" => Self::Unavailable,
            "internal" => Self::Internal,
            _ => Self::Unknown,
        }
    }
}

/// Anything that can go wrong calling Atlas.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The gateway answered with an error envelope.
    #[error("atlas {code:?} ({status}): {message}")]
    Api {
        code: ErrorCode,
        message: String,
        status: u16,
    },

    /// The request never produced a response: DNS, TCP, TLS, or timeout.
    ///
    /// Deliberately separate from [`Error::Api`]. A caller retrying or
    /// alerting needs to distinguish "the service rejected this" from "we
    /// could not ask" — the first is about the request, the second about
    /// the network, and treating them alike produces both spurious alerts
    /// and missed outages.
    #[error("could not reach atlas: {0}")]
    Connection(String),

    /// A 2xx whose body was not what this SDK expected.
    #[error("unexpected response from atlas: {0}")]
    Decode(String),

    /// Caught before anything was sent.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

impl Error {
    /// True when retrying the identical request might succeed.
    ///
    /// The set is the retry contract across all three SDKs:
    /// `unavailable`, `deadline_exceeded`, `resource_exhausted` — the
    /// transient upstream states — plus plain network failures. A 500
    /// `internal` is deliberately absent: it means the gateway itself is
    /// broken in a way a retry cannot fix, and retrying it hides the bug.
    ///
    /// Note this says nothing about whether it is SAFE to retry — that
    /// depends on the request's idempotency, which the transport decides.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Connection(_) => true,
            Error::Api { code, .. } => matches!(
                code,
                ErrorCode::Unavailable | ErrorCode::DeadlineExceeded | ErrorCode::ResourceExhausted
            ),
            _ => false,
        }
    }

    /// The stable code, when the failure came from the gateway.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Error::Api { code, .. } => Some(*code),
            _ => None,
        }
    }
}

pub(crate) fn api_error(status: u16, body: &str) -> Error {
    #[derive(serde::Deserialize)]
    struct Envelope {
        error: Inner,
    }
    #[derive(serde::Deserialize)]
    struct Inner {
        code: String,
        message: String,
    }

    match serde_json::from_str::<Envelope>(body) {
        Ok(e) => Error::Api {
            code: ErrorCode::from_wire(&e.error.code),
            message: e.error.message,
            status,
        },
        // A non-JSON error body means something between the caller and the
        // gateway answered — a proxy, a load balancer, an ingress 502. The
        // status is the only reliable signal, so it is preserved rather
        // than being flattened into a decode failure. 502/504 from a proxy
        // are unavailable for the same reason 503 is.
        Err(_) => Error::Api {
            code: match status {
                401 => ErrorCode::Unauthenticated,
                403 => ErrorCode::PermissionDenied,
                404 => ErrorCode::NotFound,
                429 => ErrorCode::ResourceExhausted,
                502..=504 => ErrorCode::Unavailable,
                _ => ErrorCode::Unknown,
            },
            message: if body.is_empty() {
                format!("HTTP {status}")
            } else {
                body.chars().take(200).collect()
            },
            status,
        },
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

pub type Result<T> = std::result::Result<T, Error>;

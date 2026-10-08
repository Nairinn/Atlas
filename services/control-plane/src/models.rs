//! Wire types.
//!
//! Every struct here mirrors one in `cli/src/api.rs`. Field names and JSON
//! shapes must match exactly — the CLI deserialises these with serde and a
//! rename would surface as a confusing parse error rather than a 4xx. The
//! integration test in `tests/cli_contract_test.rs` pins the field names.

use serde::{Deserialize, Serialize};

// --- accounts (bootstrap; no CLI counterpart yet) ---------------------------

#[derive(Debug, Deserialize)]
pub struct CreateAccountRequest {
    pub email: String,
}

/// The only response that ever carries a key in plaintext.
#[derive(Debug, Serialize)]
pub struct CreateAccountResponse {
    pub account_id: String,
    pub api_key: String,
    pub prefix: String,
    /// Spelled out because the key cannot be recovered later.
    pub warning: String,
}

// --- deploy -----------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DeployRequest {
    pub name: String,
    pub region: String,
    pub environment: String,
    pub services_enabled: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DeployResponse {
    pub project_name: String,
    pub region: String,
    pub provisioned: Vec<ProvisionedService>,
    pub endpoint: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct ProvisionedService {
    pub service: String,
    /// "ok" | "skipped" | "failed"
    pub status: String,
    pub detail: Option<String>,
}

// --- status -----------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub project_name: String,
    pub services: Vec<ServiceStatus>,
    /// "platform": the usage numbers are scraped from the gateway's
    /// process-wide counters, which do not carry a project label — every
    /// tenant's traffic is in them. Null until they do.
    pub scope: String,
}

#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub healthy: bool,
    /// Usage fields are null until gateway metrics carry a project
    /// label: the platform-wide counter mixes every tenant's traffic,
    /// and reporting it as this project's would be a wrong number that
    /// looks exact.
    pub p95_latency_ms: Option<u32>,
    pub requests_24h: Option<u64>,
    pub error_rate: Option<f64>,
}

// --- logs -------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct LogLine {
    /// RFC 3339, matching the CLI's mock fixtures.
    pub timestamp: String,
    pub service: String,
    /// "info" | "warn" | "error"
    pub level: String,
    pub message: String,
}

// --- keys -------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ApiKeyView {
    pub scope: String,
    pub name: String,
    pub prefix: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    /// "active" | "revoked"
    pub status: String,
}

/// `POST /projects/:name/keys`. The response is an [`ApiKeyView`] with the
/// plaintext attached, since this is the one moment it can be shown.
#[derive(Debug, Deserialize)]
pub struct CreateKeyRequest {
    pub name: String,
    pub expiry: KeyExpiry,
    /// 'data' (default, gateway only) or 'admin' (also control-plane
    /// mutations). Absent means data: the smaller grant.
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyExpiry {
    Never,
    Days30,
    Days90,
    Days365,
}

impl KeyExpiry {
    pub fn days(self) -> Option<i64> {
        match self {
            KeyExpiry::Never => None,
            KeyExpiry::Days30 => Some(30),
            KeyExpiry::Days90 => Some(90),
            KeyExpiry::Days365 => Some(365),
        }
    }
}

/// Created-key response: the view plus the plaintext, flattened so the CLI
/// can deserialise it straight into its `ApiKey`.
#[derive(Debug, Serialize)]
pub struct CreatedKeyResponse {
    #[serde(flatten)]
    pub key: ApiKeyView,
    /// Present only here, never on list.
    pub api_key: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLI serialises `KeyExpiry` with `rename_all = "snake_case"`, so
    /// it sends `"days30"`. Deserialising must accept exactly that.
    #[test]
    fn key_expiry_matches_the_cli_wire_format() {
        let parse = |s: &str| serde_json::from_str::<KeyExpiry>(s).unwrap();
        assert_eq!(parse("\"never\""), KeyExpiry::Never);
        assert_eq!(parse("\"days30\""), KeyExpiry::Days30);
        assert_eq!(parse("\"days90\""), KeyExpiry::Days90);
        assert_eq!(parse("\"days365\""), KeyExpiry::Days365);
    }

    #[test]
    fn expiry_days_are_right() {
        assert_eq!(KeyExpiry::Never.days(), None);
        assert_eq!(KeyExpiry::Days30.days(), Some(30));
        assert_eq!(KeyExpiry::Days365.days(), Some(365));
    }

    /// The CLI posts this exact body from `DeployRequest`.
    #[test]
    fn deploy_request_parses_the_cli_body() {
        let body = r#"{
            "name": "my-mobility-app",
            "region": "us-central1",
            "environment": "production",
            "services_enabled": ["auth", "geo", "payments", "events"]
        }"#;
        let req: DeployRequest = serde_json::from_str(body).unwrap();
        assert_eq!(req.name, "my-mobility-app");
        assert_eq!(req.services_enabled.len(), 4);
    }

    /// A created key must deserialise into the CLI's `ApiKey`, which has
    /// no `api_key` field and would reject an unflattened nesting.
    #[test]
    fn created_key_flattens_into_the_cli_shape() {
        let resp = CreatedKeyResponse {
            key: ApiKeyView {
                name: "ci".into(),
                prefix: "atl_live_ef01".into(),
                created_at: "2026-04-20T11:02:18Z".into(),
                last_used_at: None,
                status: "active".into(),
                scope: "data".into(),
            },
            api_key: "atl_live_ef01deadbeef".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["name"], "ci");
        assert_eq!(json["prefix"], "atl_live_ef01");
        assert_eq!(json["status"], "active");
        assert!(json["last_used_at"].is_null());
        assert_eq!(json["api_key"], "atl_live_ef01deadbeef");
    }
}

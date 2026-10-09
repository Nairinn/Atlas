//! `atlas.toml` configuration parser.
//!
//! A developer drops `atlas.toml` in their project root to configure which
//! Atlas platform services they want. This module parses and validates that
//! file. Unknown keys are hard errors (typo protection), with one
//! deliberate exception: the removed per-service tuning sections
//! (`[auth]`, `[geo]`, `[payments]`, `[events]`) are accepted for one
//! release with a deprecation warning, so an existing file keeps working
//! while its owner notices the removal.
//!
//! Why those sections went: nothing downstream ever read them. The deploy
//! request posts the service toggles and nothing else — the per-service
//! values validated fields no platform component consumed.

use serde::Deserialize;
use std::fs;
use std::path::Path;
use thiserror::Error;

/// Regions where Atlas projects can be provisioned.
pub const KNOWN_REGIONS: &[&str] = &["us-central1", "eu-west1", "ap-southeast1"];

/// API key prefixes. Each maps to a deployment tier.
const KEY_PREFIXES: &[&str] = &["atl_live_", "atl_test_", "atl_dev_"];

/// Sections that used to be parsed and are now ignored with a warning.
const DEPRECATED_SECTIONS: &[&str] = &["auth", "geo", "payments", "events"];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid TOML in {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },

    #[error("validation failed: {0}")]
    Validation(String),
}

/// Top-level config. Everything uses `deny_unknown_fields` so typos like
/// `[paymetns]` surface as an error instead of silently being ignored.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct AtlasConfig {
    pub project: Project,
    pub services: Services,
    /// The deprecated per-service sections actually found in the file, so
    /// `atlas validate` can warn rather than silently swallowing them.
    #[serde(skip)]
    pub deprecated_sections: Vec<&'static str>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub name: String,
    pub api_key: String,
    pub environment: Environment,
    pub region: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Development,
    Staging,
    Production,
}

impl Environment {
    pub fn as_str(&self) -> &'static str {
        match self {
            Environment::Development => "development",
            Environment::Staging => "staging",
            Environment::Production => "production",
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Services {
    #[serde(default)]
    pub auth: bool,
    #[serde(default)]
    pub geo: bool,
    #[serde(default)]
    pub payments: bool,
    #[serde(default)]
    pub events: bool,
}

// --- Loading + validation --------------------------------------------------

impl AtlasConfig {
    /// Read and validate `atlas.toml` from a path.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path_str = path.as_ref().display().to_string();
        let raw = fs::read_to_string(&path).map_err(|source| ConfigError::Io {
            path: path_str.clone(),
            source,
        })?;
        Self::parse_str(&raw, &path_str)
    }

    /// Parse a config from a string. Public for unit tests.
    ///
    /// Deprecated sections are commented out before the TOML parse — one
    /// release of tolerance — and recorded so callers can warn. The strip
    /// runs from a removed section's header to the NEXT header (or EOF),
    /// because the fields inside belong to the removed section; leaving
    /// them behind would dump them into whatever table was open above.
    pub fn parse_str(raw: &str, path_label: &str) -> Result<Self, ConfigError> {
        let mut found: Vec<&'static str> = Vec::new();
        let mut stripped_lines: Vec<String> = Vec::new();
        let mut suppressing = false;
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                suppressing = DEPRECATED_SECTIONS
                    .iter()
                    .any(|s| trimmed == format!("[{s}]"));
                if let Some(name) = DEPRECATED_SECTIONS
                    .iter()
                    .find(|s| trimmed == format!("[{s}]"))
                {
                    if !found.contains(name) {
                        found.push(name);
                    }
                }
                if suppressing {
                    stripped_lines.push(format!("# deprecated, ignored: {trimmed}"));
                    continue;
                }
            }
            if suppressing {
                stripped_lines.push(format!("# deprecated, ignored: {trimmed}"));
            } else {
                stripped_lines.push(line.to_string());
            }
        }
        let stripped = stripped_lines.join("\n");

        let mut cfg: AtlasConfig =
            toml::from_str(&stripped).map_err(|source| ConfigError::Parse {
                path: path_label.to_string(),
                source,
            })?;
        cfg.deprecated_sections = found;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Semantic validation that can't be expressed in the serde schema.
    fn validate(&self) -> Result<(), ConfigError> {
        // --- project ---
        if self.project.name.trim().is_empty() {
            return Err(ConfigError::Validation(
                "project.name must not be empty".into(),
            ));
        }
        if !is_valid_project_name(&self.project.name) {
            return Err(ConfigError::Validation(format!(
                "project.name '{}' is invalid: must be 3-40 chars, lowercase alphanumeric and hyphens, \
                 and not start or end with a hyphen",
                self.project.name
            )));
        }
        if !KEY_PREFIXES
            .iter()
            .any(|p| self.project.api_key.starts_with(p))
        {
            return Err(ConfigError::Validation(format!(
                "project.api_key must start with one of {:?}",
                KEY_PREFIXES
            )));
        }
        if self.project.api_key.len() < 24 {
            return Err(ConfigError::Validation(
                "project.api_key looks too short to be valid".into(),
            ));
        }
        if !KNOWN_REGIONS.contains(&self.project.region.as_str()) {
            return Err(ConfigError::Validation(format!(
                "project.region '{}' is not a known Atlas region. Known: {:?}",
                self.project.region, KNOWN_REGIONS
            )));
        }

        // --- services consistency ---
        if !(self.services.auth
            || self.services.geo
            || self.services.payments
            || self.services.events)
        {
            return Err(ConfigError::Validation(
                "[services] must enable at least one of auth, geo, payments, events".into(),
            ));
        }

        Ok(())
    }

    /// Mask the API key for display. Keeps the prefix and last 4 chars.
    pub fn masked_api_key(&self) -> String {
        mask_key(&self.project.api_key)
    }
}

pub fn mask_key(key: &str) -> String {
    // Find the prefix portion (everything up to and including the last `_`)
    let prefix_end = key.rfind('_').map(|i| i + 1).unwrap_or(0);
    let (prefix, secret) = key.split_at(prefix_end);
    if secret.len() <= 4 {
        return format!("{}{}", prefix, "*".repeat(secret.len()));
    }
    let tail = &secret[secret.len() - 4..];
    format!("{}{}{}", prefix, "*".repeat(secret.len() - 4), tail)
}

fn is_valid_project_name(name: &str) -> bool {
    let chars: Vec<char> = name.chars().collect();
    let len = chars.len();
    if !(3..=40).contains(&len) {
        return false;
    }
    if chars[0] == '-' || chars[len - 1] == '-' {
        return false;
    }
    chars
        .iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
}

// --- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[project]
name        = "my-mobility-app"
api_key     = "atl_live_abcdef0123456789abcdef0123456789"
environment = "production"
region      = "us-central1"

[services]
auth     = true
geo      = true
payments = true
events   = true
"#;

    fn parse(toml_str: &str) -> Result<AtlasConfig, ConfigError> {
        AtlasConfig::parse_str(toml_str, "<test>")
    }

    #[test]
    fn full_valid_config_parses() {
        let cfg = parse(MINIMAL).expect("should parse");
        assert_eq!(cfg.project.name, "my-mobility-app");
        assert_eq!(cfg.project.environment, Environment::Production);
        assert!(cfg.services.auth && cfg.services.geo);
        assert!(cfg.deprecated_sections.is_empty());
    }

    #[test]
    fn minimal_valid_config() {
        let cfg = parse(
            r#"
[project]
name        = "tiny"
api_key     = "atl_dev_xxxxxxxxxxxxxxxxxxxxxxxx"
environment = "development"
region      = "us-central1"

[services]
auth = true
"#,
        )
        .expect("should parse minimal config");
        assert!(cfg.services.auth);
        assert!(!cfg.services.geo);
    }

    #[test]
    fn deprecated_sections_are_accepted_with_a_warning() {
        let cfg = parse(
            r#"
[project]
name        = "app"
api_key     = "atl_dev_xxxxxxxxxxxxxxxxxxxxxxxx"
environment = "development"
region      = "us-central1"

[services]
auth = true
geo = true

[auth]
token_expiry_seconds = 3600
geo_claims           = true

[geo]
default_radius_m   = 500
location_ttl_hours = 24
"#,
        )
        .expect("a removed section must not break an existing file");
        assert!(cfg.services.auth);
        // Both found sections are reported, in file order.
        assert_eq!(cfg.deprecated_sections, vec!["auth", "geo"]);
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        let err = parse(
            r#"
[project]
name        = "app"
api_key     = "atl_dev_xxxxxxxxxxxxxxxxxxxxxxxx"
environment = "development"
region      = "us-central1"

[services]
auth = true

[oops]
foo = "bar"
"#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn unknown_field_in_section_is_rejected() {
        let err = parse(
            r#"
[project]
name        = "app"
api_key     = "atl_dev_xxxxxxxxxxxxxxxxxxxxxxxx"
environment = "development"
region      = "us-central1"
typo_field  = "boom"

[services]
auth = true
"#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn missing_required_project_field_is_rejected() {
        let err = parse(
            r#"
[project]
name        = "app"
environment = "development"
region      = "us-central1"

[services]
auth = true
"#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn invalid_environment_is_rejected() {
        let err = parse(
            r#"
[project]
name        = "app"
api_key     = "atl_dev_xxxxxxxxxxxxxxxxxxxxxxxx"
environment = "prod"
region      = "us-central1"

[services]
auth = true
"#,
        )
        .unwrap_err();
        // serde rejects the enum variant
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn bad_api_key_prefix_is_rejected() {
        let err = parse(
            r#"
[project]
name        = "app"
api_key     = "wrongprefix_abcdefghijklmnopqrstuvwx"
environment = "development"
region      = "us-central1"

[services]
auth = true
"#,
        )
        .unwrap_err();
        match err {
            ConfigError::Validation(msg) => assert!(msg.contains("api_key")),
            _ => panic!("expected Validation, got {err:?}"),
        }
    }

    #[test]
    fn short_api_key_is_rejected() {
        let err = parse(
            r#"
[project]
name        = "app"
api_key     = "atl_dev_short"
environment = "development"
region      = "us-central1"

[services]
auth = true
"#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn unknown_region_is_rejected() {
        let err = parse(
            r#"
[project]
name        = "app"
api_key     = "atl_dev_xxxxxxxxxxxxxxxxxxxxxxxx"
environment = "development"
region      = "mars-1"

[services]
auth = true
"#,
        )
        .unwrap_err();
        match err {
            ConfigError::Validation(msg) => assert!(msg.contains("region")),
            _ => panic!("expected Validation"),
        }
    }

    #[test]
    fn no_services_enabled_is_rejected() {
        let err = parse(
            r#"
[project]
name        = "app"
api_key     = "atl_dev_xxxxxxxxxxxxxxxxxxxxxxxx"
environment = "development"
region      = "us-central1"

[services]
"#,
        )
        .unwrap_err();
        match err {
            ConfigError::Validation(msg) => assert!(msg.contains("at least one")),
            _ => panic!("expected Validation"),
        }
    }

    #[test]
    fn invalid_project_name_is_rejected() {
        for bad in ["-leading", "trailing-", "UPPER", "with space", "ok!", "ab"] {
            let toml = format!(
                r#"
[project]
name        = "{bad}"
api_key     = "atl_dev_xxxxxxxxxxxxxxxxxxxxxxxx"
environment = "development"
region      = "us-central1"

[services]
auth = true
"#
            );
            let err = parse(&toml).unwrap_err();
            assert!(
                matches!(err, ConfigError::Validation(_)),
                "name {bad:?} should be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn key_masking() {
        assert_eq!(
            mask_key("atl_live_abcdef0123456789abcdef"),
            "atl_live_******************cdef"
        );
        assert_eq!(mask_key("atl_dev_abc"), "atl_dev_***");
    }
}

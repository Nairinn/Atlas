//! The Atlas API key format: minting, display prefixes, and digests.
//!
//! This lives in its own crate because two services must agree on it
//! exactly: the control plane MINTS keys, and the gateway RESOLVES them on
//! every data-plane request to decide which project a caller belongs to.
//! A second copy of `hash` or `looks_like_key` that drifted by one
//! character would not fail loudly — it would quietly reject every valid
//! key, or worse, accept a shape the issuer never mints.
//!
//! # Format
//!
//! `atl_{tier}_{32 hex chars}` — e.g. `atl_live_9f3c1e...`. The tier comes
//! from the project's environment, and the three schemes are exactly the
//! ones `cli/src/config.rs` accepts in `KEY_PREFIXES`; a key this module
//! mints must pass `atlas validate` when pasted into `atlas.toml`, which
//! also means it must be at least 24 characters.
//!
//! # Storage
//!
//! Only the SHA-256 digest is persisted. The plaintext is returned once,
//! at creation, and cannot be recovered afterwards.
//!
//! SHA-256 rather than bcrypt/Argon2 is deliberate and is the opposite of
//! the choice auth-service makes for passwords. A password is low-entropy
//! and guessable, so it needs a deliberately slow KDF. An API key here is
//! 128 bits of CSPRNG output — there is no dictionary to run against it,
//! and brute force is not a threat model a slow hash improves. What a slow
//! KDF *would* do is add its cost to every authenticated request, since
//! the key is presented on each one.

use sha2::{Digest, Sha256};

/// Schemes accepted by the CLI, keyed by project environment.
pub const SCHEME_LIVE: &str = "atl_live_";
pub const SCHEME_TEST: &str = "atl_test_";
pub const SCHEME_DEV: &str = "atl_dev_";

/// How many secret characters appear in the display prefix. Matches the
/// `atl_live_abcd` shape the CLI renders.
const PREFIX_SECRET_CHARS: usize = 4;

/// A freshly minted key. The plaintext exists only in this struct and in
/// the single response that returns it.
#[derive(Debug, Clone)]
pub struct GeneratedKey {
    pub plaintext: String,
    pub prefix: String,
    pub hash: String,
}

/// Map a project environment onto a key scheme.
///
/// Unknown environments fall back to the dev scheme: the database CHECK
/// constraint already restricts the column to the three known values, so
/// this branch is unreachable in practice, and defaulting to the least
/// privileged-looking tier is the safe direction to be wrong in.
pub fn scheme_for(environment: &str) -> &'static str {
    match environment {
        "production" => SCHEME_LIVE,
        "staging" => SCHEME_TEST,
        _ => SCHEME_DEV,
    }
}

/// Mint a key for the given environment.
///
/// The secret is 16 bytes drawn straight from the OS CSPRNG, hex-encoded
/// to 32 characters = 128 bits. A UUID's v4 layout would reserve 6 bits
/// for version/variant fields, so deriving entropy from UUIDs loses
/// those bits; reading the CSPRNG directly keeps all 128 and is shorter
/// to reason about.
///
/// Prefix collisions within a project are handled by the caller (the
/// control plane re-rolls against the project's existing active keys)
/// and backstopped by a unique index; see migration 0110.
pub fn generate(environment: &str) -> GeneratedKey {
    let scheme = scheme_for(environment);
    let bytes = rand_bytes();
    let secret: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let plaintext = format!("{scheme}{secret}");
    GeneratedKey {
        prefix: prefix_of(&plaintext),
        hash: hash(&plaintext),
        plaintext,
    }
}

/// 16 bytes from the OS CSPRNG. `getrandom` is what `uuid::Uuid::new_v4`
/// uses internally; calling it directly drops the UUID framing.
fn rand_bytes() -> [u8; 16] {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable");
    buf
}

/// SHA-256, lowercase hex. This is what the `control.api_keys.key_hash`
/// unique index is built on, so every authenticated request is a single
/// indexed lookup.
pub fn hash(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    // `{:02x}` per byte rather than a hex crate — one fewer dependency for
    // three lines of formatting.
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Display prefix: the scheme plus the first four secret characters.
///
/// Splits on the last underscore, matching `cli::config::mask_key`, so a
/// prefix printed by `atlas keys list` lines up with the masked key shown
/// by `atlas validate`.
pub fn prefix_of(key: &str) -> String {
    let split = key.rfind('_').map(|i| i + 1).unwrap_or(0);
    let (scheme, secret) = key.split_at(split);
    let shown: String = secret.chars().take(PREFIX_SECRET_CHARS).collect();
    format!("{scheme}{shown}")
}

/// Whether a string is shaped like an Atlas key. Cheap pre-filter so a
/// malformed bearer token is rejected without a database round trip.
pub fn looks_like_key(candidate: &str) -> bool {
    [SCHEME_LIVE, SCHEME_TEST, SCHEME_DEV]
        .iter()
        .any(|s| candidate.starts_with(s))
        && candidate.len() >= 24
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The CLI refuses to load an `atlas.toml` whose key does not match
    /// `KEY_PREFIXES` and is not at least 24 characters. A key we mint has
    /// to survive that, or `atlas deploy` fails on the config we just told
    /// the developer to write.
    #[test]
    fn generated_keys_satisfy_cli_validation() {
        for env in ["production", "staging", "development"] {
            let k = generate(env);
            assert!(
                looks_like_key(&k.plaintext),
                "{env} key {} not recognised",
                k.plaintext
            );
            assert!(
                k.plaintext.len() >= 24,
                "{env} key is {} chars, CLI requires >= 24",
                k.plaintext.len()
            );
        }
    }

    #[test]
    fn scheme_follows_environment() {
        assert_eq!(scheme_for("production"), SCHEME_LIVE);
        assert_eq!(scheme_for("staging"), SCHEME_TEST);
        assert_eq!(scheme_for("development"), SCHEME_DEV);
        // Unknown values must not silently mint a live key.
        assert_eq!(scheme_for("whatever"), SCHEME_DEV);
    }

    #[test]
    fn prefix_is_scheme_plus_four() {
        assert_eq!(prefix_of("atl_live_abcdef0123"), "atl_live_abcd");
        assert_eq!(prefix_of("atl_dev_9f3c1e"), "atl_dev_9f3c");
        // Shorter-than-four secrets do not panic.
        assert_eq!(prefix_of("atl_test_ab"), "atl_test_ab");
        // No underscore at all: the whole string counts as secret, so it
        // truncates to four rather than echoing the input back. Callers
        // are gated by `looks_like_key`, so this is unreachable in
        // practice — but erring toward revealing less is the right
        // direction for a function whose job is to not print secrets.
        assert_eq!(prefix_of("nounderscore"), "noun");
    }

    #[test]
    fn prefix_never_reveals_the_whole_secret() {
        let k = generate("production");
        assert!(k.plaintext.starts_with(&k.prefix));
        assert!(
            k.prefix.len() < k.plaintext.len(),
            "prefix must be a strict prefix of the key"
        );
    }

    #[test]
    fn hash_is_stable_and_is_not_the_key() {
        // Known-answer test, so a change of algorithm cannot pass silently
        // and invalidate every stored key.
        assert_eq!(hash("atl_live_0123456789abcdef0123456789abcdef"), {
            let d = Sha256::digest(b"atl_live_0123456789abcdef0123456789abcdef");
            d.iter().map(|b| format!("{b:02x}")).collect::<String>()
        });
        assert_eq!(hash("a").len(), 64);
        assert_ne!(hash("atl_live_x"), "atl_live_x");
        assert_ne!(hash("atl_live_x"), hash("atl_live_y"));
    }

    /// Two keys colliding would let one project authenticate as another.
    #[test]
    fn generated_keys_are_unique() {
        let n = 1_000;
        let keys: HashSet<String> = (0..n).map(|_| generate("production").plaintext).collect();
        assert_eq!(keys.len(), n, "duplicate key generated");
    }

    #[test]
    fn looks_like_key_rejects_junk() {
        assert!(!looks_like_key(""));
        assert!(!looks_like_key("Bearer atl_live_abc"));
        // A well-formed key belonging to some other scheme. The prefix is
        // deliberately not a real vendor's (Stripe's `sk_live_`, say) —
        // secret scanners flag those even in test fixtures, and rightly so.
        assert!(!looks_like_key("zzz_live_0123456789abcdef0123456789"));
        // Right scheme but too short for the CLI's own rule.
        assert!(!looks_like_key("atl_live_abc"));
    }
}

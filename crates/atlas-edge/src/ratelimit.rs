//! The rate-limit core shared by the gateway and the control plane.
//!
//! # What this does and does not protect
//!
//! This is **per-replica**, in-process limiting. With three gateway pods a
//! configured 60/min is effectively 180/min globally, because a client's
//! requests are spread across replicas by the load balancer. That is
//! deliberate rather than overlooked: a globally exact limit needs shared
//! state (Redis) on the hot path of every request, which is a latency and
//! an availability cost this does not yet justify.
//!
//! Treat it as the layer that stops one client from exhausting one
//! replica, sitting underneath the ingress rate limit which is the real
//! global cap.
//!
//! # What each service supplies
//!
//! The vocabulary of buckets (default vs credential vs signup) differs per
//! service, so [`Buckets`] is generic over a service-defined profile:
//! the service decides which profile a request belongs to and what the
//! 429 body looks like; this module owns the GCRA machinery, the
//! per-address anti-rotation ceiling, and the key derivation.

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use sha2::{Digest, Sha256};

type Keyed = RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock>;

/// Per-minute quotas for one service's limiter profiles.
#[derive(Debug, Clone)]
pub struct Quotas<P> {
    /// Requests per minute for ordinary authenticated traffic.
    pub default_per_minute: u32,
    /// The anti-rotation cap: per-address requests per minute applied to
    /// EVERY non-exempt request, so a client minting a fresh bearer string
    /// per request cannot get a fresh token bucket each time. Set far
    /// above the per-key quota so carrier-grade NAT users never feel it.
    pub ip_ceiling_per_minute: u32,
    /// How many proxies sit in front of this service. See [`client_key`].
    pub trusted_proxy_hops: usize,
    pub enabled: bool,
    /// The service's strict-quota profiles (credential endpoints on the
    /// gateway, signup on the control plane) with their per-minute rate.
    pub strict: Vec<(P, u32)>,
}

impl<P> Default for Quotas<P> {
    fn default() -> Self {
        Self {
            default_per_minute: 600,
            ip_ceiling_per_minute: 6000,
            trusted_proxy_hops: 0,
            enabled: true,
            strict: Vec::new(),
        }
    }
}

pub struct Limiters<P: QuotaProfile> {
    default: Keyed,
    ip_ceiling: Keyed,
    strict: Vec<(P, Keyed)>,
    quotas: Quotas<P>,
}

impl<P: QuotaProfile> Limiters<P> {
    pub fn new(quotas: Quotas<P>) -> Arc<Self> {
        let quota = |per_minute: u32| {
            let n = NonZeroU32::new(per_minute.max(1)).expect("nonzero");
            // Burst equal to the per-minute rate: GCRA refills
            // continuously, so this allows a short spike then settles to
            // the steady rate, which matches how a real client opening a
            // screen behaves.
            Quota::per_minute(n).allow_burst(n)
        };
        Arc::new(Self {
            default: RateLimiter::keyed(quota(quotas.default_per_minute)),
            ip_ceiling: RateLimiter::keyed(quota(quotas.ip_ceiling_per_minute)),
            strict: quotas
                .strict
                .iter()
                .map(|(p, rate)| (p.clone(), RateLimiter::keyed(quota(*rate))))
                .collect(),
            quotas,
        })
    }

    pub fn quotas(&self) -> &Quotas<P> {
        &self.quotas
    }

    /// The per-address ceiling verdict for a request. Checked before any
    /// primary bucket: it is what actually caps a token-rotation attack,
    /// which otherwise mints a fresh primary bucket per request.
    pub fn check_ip_ceiling(&self, key: &String) -> Result<(), Duration> {
        self.ip_ceiling
            .check_key(key)
            .map_err(|neg| neg.wait_time_from(DefaultClock::default().now()))
    }

    /// The primary-bucket verdict: the strict profile's bucket when the
    /// request matches one, otherwise the default bucket.
    pub fn check_primary(&self, profile: Option<&P>, key: &String) -> Result<(), Duration> {
        let limiter = profile
            .and_then(|p| self.strict.iter().find(|(sp, _)| sp == p))
            .map(|(_, l)| l)
            .unwrap_or(&self.default);
        limiter
            .check_key(key)
            .map_err(|neg| neg.wait_time_from(DefaultClock::default().now()))
    }

    /// Drop buckets for keys that have not been seen recently.
    ///
    /// Without this the key map grows for every distinct IP or token
    /// forever, which turns the rate limiter itself into a memory
    /// exhaustion vector — the opposite of its job.
    pub fn retain_recent(&self) {
        self.default.retain_recent();
        self.ip_ceiling.retain_recent();
        for (_, l) in &self.strict {
            l.retain_recent();
        }
    }

    /// Spawn the periodic sweep. Cheap: it walks only live keys.
    pub fn spawn_gc(self: &Arc<Self>) {
        let limiters = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                limiters.retain_recent();
            }
        });
    }
}

/// A service's strict-quota profile: which requests get their own bucket
/// (credential endpoints on the gateway, account creation on the control
/// plane). The service both names the profiles and classifies paths into
/// them; this crate stays agnostic.
pub trait QuotaProfile: Clone + PartialEq + Send + Sync + 'static {}

impl<T> QuotaProfile for T where T: Clone + PartialEq + Send + Sync + 'static {}

/// Build the limiter key for a request.
///
/// # Why the client address is not just `X-Forwarded-For[0]`
///
/// `X-Forwarded-For` is client-supplied. Anyone can send
/// `X-Forwarded-For: 1.2.3.4` and, if the first entry is trusted, get a
/// fresh rate-limit bucket per forged value — which defeats the limiter
/// entirely and is a worse position than not having one, because it
/// looks protected.
///
/// The only safe read is to count back from the right by the number of
/// proxies you actually run: each appends the address it saw, so the
/// (hops+1)-th from the end is the last address a trusted proxy observed.
/// `trusted_proxy_hops = 0` means no proxy is trusted and the socket
/// peer address is used, which is correct for direct exposure and tests.
pub fn client_key(
    xff: Option<&str>,
    peer: Option<SocketAddr>,
    trusted_proxy_hops: usize,
) -> String {
    if trusted_proxy_hops > 0 {
        if let Some(xff) = xff {
            let hops: Vec<&str> = xff
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            // hops.len() - trusted_proxy_hops is the index of the address
            // the outermost trusted proxy saw. A shorter list than
            // expected means the request did not traverse the proxies we
            // think it did, so fall through to the peer address rather
            // than trusting it.
            if let Some(idx) = hops.len().checked_sub(trusted_proxy_hops) {
                if let Some(addr) = hops.get(idx) {
                    if let Ok(ip) = addr.parse::<IpAddr>() {
                        return format!("ip:{ip}");
                    }
                }
            }
        }
    }
    match peer {
        Some(addr) => format!("ip:{}", addr.ip()),
        // Only reachable when ConnectInfo is absent, which in practice
        // means a test harness driving the router directly.
        None => "ip:unknown".to_string(),
    }
}

/// Key authenticated requests by a digest of the bearer token.
///
/// Hashed rather than used raw so a live credential is not sitting in a
/// map key in memory, and truncated because 16 hex characters is ample
/// to keep buckets distinct. Returns None when no parseable bearer is
/// present; the caller falls back to the address key.
pub fn token_key(bearer: Option<&str>) -> Option<String> {
    let token = bearer?;
    let digest = Sha256::digest(token.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    Some(format!("tok:{hex}"))
}

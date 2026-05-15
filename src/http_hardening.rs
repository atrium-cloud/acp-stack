//! HTTP hardening: CORS, WebSocket origin, trusted-proxy client IP, and the
//! in-process auth-failure IP block.
//!
//! Layout:
//!   * [`client_ip`] resolves the effective client IP given the socket peer,
//!     the configured `[security.http]` block, and the request headers. If
//!     `trust_proxy_headers` is true and the peer matches an entry in
//!     `trusted_proxies`, parses `X-Forwarded-For` / `Forwarded`. Otherwise
//!     returns the socket peer.
//!   * [`build_cors_layer`] builds a `CorsLayer` from `allowed_origins`. Returns
//!     `None` when no origins are configured (same-origin behavior).
//!   * [`origin_allowed`] checks a `Origin` header value against the configured
//!     allowlist. Used by the WebSocket upgrade handler.
//!   * [`AuthFailureBlocker`] tracks per-IP auth-failure counts in a
//!     time-windowed map. When the count crosses `auth_failures_per_minute`,
//!     the IP is blocked for `auth_block_duration`. Used by the authenticate
//!     middleware to short-circuit blocked IPs before they reach key
//!     comparison.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use http::HeaderMap;
use sha2::{Digest, Sha256};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::config::SecurityHttpConfig;

/// Resolve the effective client IP for a request given the socket peer.
/// Returns the socket peer unless proxy headers are explicitly trusted AND
/// the peer is in `trusted_proxies`. The leftmost entry of `X-Forwarded-For`
/// is preferred when both headers are present (matches NGINX defaults).
pub fn client_ip(headers: &HeaderMap, peer: IpAddr, sec: &SecurityHttpConfig) -> IpAddr {
    if !sec.trust_proxy_headers {
        return peer;
    }
    let peer_trusted = sec
        .trusted_proxies
        .iter()
        .filter_map(|raw| raw.parse::<IpAddr>().ok())
        .any(|trusted| trusted == peer);
    if !peer_trusted {
        return peer;
    }
    if let Some(value) = headers.get("x-forwarded-for") {
        if let Ok(text) = value.to_str() {
            if let Some(first) = text.split(',').next() {
                if let Ok(ip) = first.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }
    }
    if let Some(value) = headers.get("forwarded") {
        if let Ok(text) = value.to_str() {
            // Match a single `for=<ip>` segment. Real RFC 7239 parsing is more
            // permissive (quoted, with port, multiple segments); the leftmost
            // unquoted host is sufficient for the common case.
            for part in text.split(';') {
                let part = part.trim();
                if let Some(rest) = part.strip_prefix("for=") {
                    let rest = rest.trim().trim_matches('"');
                    if let Ok(ip) = rest.parse::<IpAddr>() {
                        return ip;
                    }
                }
            }
        }
    }
    peer
}

/// Build a `tower_http::cors::CorsLayer` from the configured origins. Returns
/// `None` when no origins are configured so the router stays free of an
/// unnecessary layer in single-origin deployments. The wildcard self-check
/// (`http.wildcard_origin_public_bind`) already warns about `*` on public binds.
pub fn build_cors_layer(sec: &SecurityHttpConfig) -> Option<CorsLayer> {
    if sec.allowed_origins.is_empty() {
        return None;
    }
    // Wildcard ("*") demands AllowOrigin::any() and forbids credentials. The
    // self-check already warns about this on public binds; honor what the
    // operator configured here without panicking.
    let has_wildcard = sec.allowed_origins.iter().any(|origin| origin == "*");
    let layer = CorsLayer::new()
        .allow_headers([http::header::AUTHORIZATION, http::header::CONTENT_TYPE])
        .allow_methods([
            http::Method::GET,
            http::Method::POST,
            http::Method::PUT,
            http::Method::DELETE,
            http::Method::OPTIONS,
        ]);
    let layer = if has_wildcard {
        layer.allow_origin(AllowOrigin::any())
    } else {
        let parsed: Vec<http::HeaderValue> = sec
            .allowed_origins
            .iter()
            .filter_map(|origin| origin.parse().ok())
            .collect();
        if parsed.is_empty() {
            return None;
        }
        layer
            .allow_origin(AllowOrigin::list(parsed))
            .allow_credentials(true)
    };
    Some(layer)
}

/// Check whether an `Origin` header value matches the configured allowlist.
/// Returns true when the allowlist is empty (no origin policy enforced),
/// when no `Origin` header was provided (CLI/local clients), when any entry
/// in the allowlist is `"*"` (wildcard, matching the CORS layer's behavior),
/// or when the origin literally matches one of the configured entries.
pub fn origin_allowed(origin: Option<&str>, sec: &SecurityHttpConfig) -> bool {
    if sec.allowed_origins.is_empty() {
        return true;
    }
    let Some(origin) = origin else {
        return true;
    };
    sec.allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed == origin)
}

/// In-process per-IP auth-failure counter + temporary block. Used by the
/// authenticate middleware to:
///
///   * Decrement a count and (re)set the block when the auth_failures table
///     accepts a new row.
///   * Refuse new requests from a blocked IP before the bearer-token compare
///     runs, so brute-force attempts cost the attacker zero local CPU.
///
/// The block is purely advisory — a daemon restart clears it. Persistent IP
/// blocks belong in a reverse proxy layer.
pub struct AuthFailureBlocker {
    failures_per_minute: u64,
    block_duration: Duration,
    state: DashMap<IpAddr, BlockState>,
}

#[derive(Debug, Clone, Copy)]
struct BlockState {
    window_start: Instant,
    count: u64,
    blocked_until: Option<Instant>,
}

impl Default for BlockState {
    fn default() -> Self {
        Self {
            window_start: Instant::now(),
            count: 0,
            blocked_until: None,
        }
    }
}

impl AuthFailureBlocker {
    pub fn from_config(sec: &SecurityHttpConfig) -> Self {
        let block_duration = crate::config::parse_duration_string(&sec.auth_block_duration)
            .unwrap_or(Duration::from_secs(15 * 60));
        Self {
            failures_per_minute: sec.auth_failures_per_minute,
            block_duration,
            state: DashMap::new(),
        }
    }

    /// Returns `Some(until)` if the IP is currently blocked and the request
    /// should be rejected with `auth.ip_blocked`. Returns None otherwise.
    /// Side effect: clears an elapsed `blocked_until` and resets the failure
    /// counter so a fresh brute-force burst can re-trip the block (otherwise
    /// the IP would be permanently unblockable once its first block elapsed).
    pub fn check(&self, ip: IpAddr) -> Option<Instant> {
        let now = Instant::now();
        let blocked_until = {
            let entry = self.state.get(&ip)?;
            entry.blocked_until
        };
        let until = blocked_until?;
        if until <= now {
            if let Some(mut entry) = self.state.get_mut(&ip) {
                entry.blocked_until = None;
                entry.window_start = now;
                entry.count = 0;
            }
            return None;
        }
        Some(until)
    }

    /// Record a new auth failure for `ip`. If the per-minute count crosses
    /// the configured threshold, set a block for `block_duration` and return
    /// `true` so the caller can emit a `security.ip_block_applied` event.
    pub fn record_failure(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut entry = self.state.entry(ip).or_default();
        // A stale blocked_until that already elapsed must not gate the
        // re-trip logic below. Clear it here so the brute-force-after-cooldown
        // attacker gets blocked again instead of getting a permanent pass.
        if let Some(until) = entry.blocked_until {
            if until <= now {
                entry.blocked_until = None;
                entry.window_start = now;
                entry.count = 0;
            }
        }
        if now.saturating_duration_since(entry.window_start) >= Duration::from_secs(60) {
            entry.window_start = now;
            entry.count = 0;
        }
        entry.count += 1;
        if entry.count >= self.failures_per_minute && entry.blocked_until.is_none() {
            entry.blocked_until = Some(now + self.block_duration);
            return true;
        }
        false
    }

    /// Reset the counter for an IP (used when a successful auth lands).
    pub fn record_success(&self, ip: IpAddr) {
        if let Some(mut entry) = self.state.get_mut(&ip) {
            entry.count = 0;
            entry.blocked_until = None;
        }
    }

    pub fn block_duration(&self) -> Duration {
        self.block_duration
    }
}

/// Scope label for a rate-limit rejection. Threaded into the durable
/// `security.rate_limited` event payload so operators can tell whether a
/// burst is coming from a single key, a single IP, or unauthenticated noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitScope {
    PerIp,
    PerKey,
    Unauthenticated,
}

impl RateLimitScope {
    pub fn as_str(self) -> &'static str {
        match self {
            RateLimitScope::PerIp => "per_ip",
            RateLimitScope::PerKey => "per_key",
            RateLimitScope::Unauthenticated => "unauthenticated",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

/// In-process rate limiter. Three independent token buckets:
///
/// * `per_ip` — ticked on every request, keyed by the resolved client IP
///   (trusted-proxy-aware). Capacity `burst`, refill
///   `rate_limit_per_minute / 60` tokens/sec.
/// * `per_key` — ticked on requests that successfully match an API key,
///   keyed by an opaque sha256 fingerprint (first 16 hex chars) of the
///   bearer token. The raw key is never stored. Same capacity/refill as
///   per_ip.
/// * `unauthenticated` — ticked on requests that fail bearer parse/match,
///   keyed by client IP, with a stricter capacity/refill (1/4 of the
///   authenticated tier). Defense-in-depth against unauthenticated floods
///   below the auth-failure-block threshold.
///
/// State is in-memory only; a daemon restart clears all buckets. Persistent
/// rate-limit state belongs at the reverse-proxy layer.
pub struct RateLimiter {
    auth_capacity: f64,
    auth_refill_per_sec: f64,
    unauth_capacity: f64,
    unauth_refill_per_sec: f64,
    per_ip: DashMap<IpAddr, TokenBucket>,
    per_key: DashMap<String, TokenBucket>,
    unauth: DashMap<IpAddr, TokenBucket>,
}

impl RateLimiter {
    pub fn from_config(sec: &SecurityHttpConfig) -> Self {
        // `burst` is the bucket capacity; `rate_limit_per_minute` controls
        // the steady-state refill rate. Both are required > 0 by config
        // validation; clamping to 1 is purely defense-in-depth.
        let auth_capacity = sec.burst.max(1) as f64;
        let auth_refill_per_sec = (sec.rate_limit_per_minute.max(1) as f64) / 60.0;
        // Unauthenticated tier is 1/4 of the authenticated tier. Conservative
        // — high-traffic legitimate clients must be authenticated anyway.
        // Ceiling division so a configured `burst = 4` still allows a 1-token
        // unauth bucket.
        let unauth_capacity = (sec.burst.div_ceil(4)).max(1) as f64;
        let unauth_refill_per_sec = (sec.rate_limit_per_minute.div_ceil(4).max(1) as f64) / 60.0;
        Self {
            auth_capacity,
            auth_refill_per_sec,
            unauth_capacity,
            unauth_refill_per_sec,
            per_ip: DashMap::new(),
            per_key: DashMap::new(),
            unauth: DashMap::new(),
        }
    }

    pub fn check_per_ip(&self, ip: IpAddr) -> Result<(), RateLimitScope> {
        if try_acquire(
            &self.per_ip,
            ip,
            self.auth_capacity,
            self.auth_refill_per_sec,
        ) {
            Ok(())
        } else {
            Err(RateLimitScope::PerIp)
        }
    }

    pub fn check_per_key(&self, fingerprint: &str) -> Result<(), RateLimitScope> {
        if try_acquire(
            &self.per_key,
            fingerprint.to_owned(),
            self.auth_capacity,
            self.auth_refill_per_sec,
        ) {
            Ok(())
        } else {
            Err(RateLimitScope::PerKey)
        }
    }

    pub fn check_unauthenticated(&self, ip: IpAddr) -> Result<(), RateLimitScope> {
        if try_acquire(
            &self.unauth,
            ip,
            self.unauth_capacity,
            self.unauth_refill_per_sec,
        ) {
            Ok(())
        } else {
            Err(RateLimitScope::Unauthenticated)
        }
    }
}

fn try_acquire<K>(map: &DashMap<K, TokenBucket>, key: K, capacity: f64, refill_per_sec: f64) -> bool
where
    K: std::hash::Hash + Eq + Clone,
{
    let now = Instant::now();
    let mut entry = map.entry(key).or_insert(TokenBucket {
        tokens: capacity,
        last_refill: now,
    });
    let elapsed = now.duration_since(entry.last_refill).as_secs_f64();
    entry.tokens = (entry.tokens + elapsed * refill_per_sec).min(capacity);
    entry.last_refill = now;
    if entry.tokens >= 1.0 {
        entry.tokens -= 1.0;
        true
    } else {
        false
    }
}

/// Opaque fingerprint of an API key value. Returns the first 16 hex
/// characters of `sha256(key_bytes)`. Used as the per-key rate-limiter map
/// key and in `security.rate_limited` event payloads so operators can
/// identify which bearer was throttled without ever logging the key itself.
pub fn key_fingerprint(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn make_sec(
        trust: bool,
        proxies: &[&str],
        origins: &[&str],
        failures: u64,
    ) -> SecurityHttpConfig {
        SecurityHttpConfig {
            max_request_bytes: 1024,
            rate_limit_per_minute: 100,
            burst: 30,
            auth_failures_per_minute: failures,
            auth_block_duration: "1s".to_owned(),
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
            trust_proxy_headers: trust,
            trusted_proxies: proxies.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn client_ip_returns_peer_when_proxy_headers_untrusted() {
        let sec = make_sec(false, &[], &[], 10);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(client_ip(&headers, peer, &sec), peer);
    }

    #[test]
    fn client_ip_honors_xff_when_peer_is_trusted() {
        let sec = make_sec(true, &["127.0.0.1"], &[], 10);
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.7, 10.0.0.1"),
        );
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(client_ip(&headers, peer, &sec).to_string(), "203.0.113.7");
    }

    #[test]
    fn client_ip_ignores_xff_when_peer_not_trusted() {
        let sec = make_sec(true, &["10.0.0.1"], &[], 10);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(client_ip(&headers, peer, &sec), peer);
    }

    #[test]
    fn origin_allowed_returns_true_with_empty_list() {
        let sec = make_sec(false, &[], &[], 10);
        assert!(origin_allowed(Some("https://x"), &sec));
        assert!(origin_allowed(None, &sec));
    }

    #[test]
    fn origin_allowed_returns_false_on_missing_match() {
        let sec = make_sec(false, &[], &["https://allowed"], 10);
        assert!(origin_allowed(Some("https://allowed"), &sec));
        assert!(!origin_allowed(Some("https://blocked"), &sec));
    }

    #[test]
    fn origin_allowed_wildcard_accepts_any() {
        let sec = make_sec(false, &[], &["*"], 10);
        assert!(origin_allowed(Some("https://browser.example"), &sec));
        assert!(origin_allowed(Some("http://127.0.0.1:5173"), &sec));
        assert!(origin_allowed(None, &sec));
    }

    #[test]
    fn origin_allowed_wildcard_alongside_literal_still_accepts_any() {
        let sec = make_sec(false, &[], &["*", "https://allowed"], 10);
        assert!(origin_allowed(Some("https://other"), &sec));
    }

    #[test]
    fn auth_failure_blocker_blocks_after_threshold() {
        let sec = make_sec(false, &[], &[], 3);
        let blocker = AuthFailureBlocker::from_config(&sec);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(!blocker.record_failure(ip));
        assert!(!blocker.record_failure(ip));
        assert!(
            blocker.record_failure(ip),
            "third failure should trip block"
        );
        assert!(blocker.check(ip).is_some());
    }

    #[test]
    fn record_success_clears_counter() {
        let sec = make_sec(false, &[], &[], 3);
        let blocker = AuthFailureBlocker::from_config(&sec);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        blocker.record_failure(ip);
        blocker.record_failure(ip);
        blocker.record_success(ip);
        assert!(blocker.check(ip).is_none());
    }

    #[test]
    fn key_fingerprint_is_deterministic_and_does_not_expose_key() {
        let f1 = key_fingerprint("acps_session_secret_token");
        let f2 = key_fingerprint("acps_session_secret_token");
        let f3 = key_fingerprint("different");
        assert_eq!(f1, f2);
        assert_ne!(f1, f3);
        // Same length, hex-only, and does not contain the raw key.
        assert_eq!(f1.len(), 16);
        assert!(f1.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!f1.contains("session"));
        assert!(!f1.contains("acps"));
    }

    #[test]
    fn rate_limiter_rejects_after_burst_exhausted() {
        // burst=2, rate_limit_per_minute=60 (1/sec refill). First 2 succeed,
        // 3rd fails before the bucket can refill enough.
        let sec = SecurityHttpConfig {
            max_request_bytes: 1024,
            rate_limit_per_minute: 60,
            burst: 2,
            auth_failures_per_minute: 10,
            auth_block_duration: "15m".to_owned(),
            allowed_origins: vec![],
            trust_proxy_headers: false,
            trusted_proxies: vec![],
        };
        let limiter = RateLimiter::from_config(&sec);
        let ip: IpAddr = "10.0.0.5".parse().unwrap();
        assert!(limiter.check_per_ip(ip).is_ok());
        assert!(limiter.check_per_ip(ip).is_ok());
        assert_eq!(limiter.check_per_ip(ip), Err(RateLimitScope::PerIp));
    }

    #[test]
    fn rate_limiter_per_key_independent_of_per_ip() {
        let sec = SecurityHttpConfig {
            max_request_bytes: 1024,
            rate_limit_per_minute: 60,
            burst: 1,
            auth_failures_per_minute: 10,
            auth_block_duration: "15m".to_owned(),
            allowed_origins: vec![],
            trust_proxy_headers: false,
            trusted_proxies: vec![],
        };
        let limiter = RateLimiter::from_config(&sec);
        assert!(limiter.check_per_key("aaaa").is_ok());
        assert_eq!(limiter.check_per_key("aaaa"), Err(RateLimitScope::PerKey));
        // A different fingerprint has its own bucket.
        assert!(limiter.check_per_key("bbbb").is_ok());
    }

    #[test]
    fn rate_limiter_unauthenticated_uses_stricter_limit() {
        // burst=8, so unauth capacity = ceil(8/4) = 2.
        let sec = SecurityHttpConfig {
            max_request_bytes: 1024,
            rate_limit_per_minute: 60,
            burst: 8,
            auth_failures_per_minute: 10,
            auth_block_duration: "15m".to_owned(),
            allowed_origins: vec![],
            trust_proxy_headers: false,
            trusted_proxies: vec![],
        };
        let limiter = RateLimiter::from_config(&sec);
        let ip: IpAddr = "10.0.0.6".parse().unwrap();
        assert!(limiter.check_unauthenticated(ip).is_ok());
        assert!(limiter.check_unauthenticated(ip).is_ok());
        assert_eq!(
            limiter.check_unauthenticated(ip),
            Err(RateLimitScope::Unauthenticated),
        );
    }

    #[test]
    fn ip_is_re_blockable_after_initial_block_expires() {
        // 1-second block: the test sleeps just past the expiry, then
        // attempts to brute-force again. The blocker must re-trip rather
        // than allow unlimited subsequent attempts. Regression for the bug
        // where `record_failure` checked `blocked_until.is_none()` even on
        // already-elapsed blocks.
        let sec = make_sec(false, &[], &[], 2);
        let blocker = AuthFailureBlocker::from_config(&sec);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        blocker.record_failure(ip);
        assert!(blocker.record_failure(ip), "second failure should trip");
        assert!(blocker.check(ip).is_some());

        std::thread::sleep(Duration::from_millis(1100));
        assert!(blocker.check(ip).is_none(), "block must elapse");

        blocker.record_failure(ip);
        assert!(
            blocker.record_failure(ip),
            "second post-cooldown failure must re-trip the block"
        );
        assert!(blocker.check(ip).is_some());
    }
}

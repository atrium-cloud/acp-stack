//! Process-wide per-domain pacing for outbound HTTP requests: hosted computers
//! share egress IPs, so unauthenticated per-IP quotas are a near-exhausted shared
//! resource. Callers opt in via [`acquire`]/[`report_rate_limited`]; hosts without
//! a policy entry pass through untouched.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::error::{Result, StackError};

// CONSTANTS
const GITHUB_API_HOST: &str = "api.github.com";
/// Minimum spacing between our own requests to `api.github.com`.
const GITHUB_API_MIN_INTERVAL: Duration = Duration::from_secs(10);
/// Cooldown used when a rate-limit response carries no usable reset header.
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(5 * 60);
/// Upper bound on a header-supplied cooldown: reset headers come from the network,
/// and an absurd value must not arm a permanent circuit or overflow `Instant` math.
const MAX_COOLDOWN: Duration = Duration::from_secs(60 * 60);
/// Hard cap on any single wait; beyond this a typed error is returned instead,
/// since a longer sleep is indistinguishable from a hang.
const MAX_WAIT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Copy)]
struct DomainPolicy {
    min_interval: Duration,
}

fn policy_for(host: &str) -> Option<DomainPolicy> {
    match host {
        GITHUB_API_HOST => Some(DomainPolicy {
            min_interval: GITHUB_API_MIN_INTERVAL,
        }),
        _ => None,
    }
}

#[derive(Debug, Default)]
struct DomainState {
    next_allowed_at: Option<Instant>,
    circuit_open_until: Option<Instant>,
}

static DOMAIN_STATES: Mutex<BTreeMap<String, DomainState>> = Mutex::new(BTreeMap::new());

enum Reservation {
    Ready,
    Wait(Duration),
}

fn reserve_slot(state: &mut DomainState, policy: DomainPolicy, now: Instant) -> Reservation {
    if let Some(until) = state.circuit_open_until {
        if until > now {
            return Reservation::Wait(until - now);
        }
        state.circuit_open_until = None;
    }
    if let Some(at) = state.next_allowed_at
        && at > now
    {
        return Reservation::Wait(at - now);
    }
    state.next_allowed_at = Some(now + policy.min_interval);
    Reservation::Ready
}

fn open_circuit(state: &mut DomainState, now: Instant, cooldown: Duration) {
    let until = now + cooldown;
    // Never shorten an already-open circuit: concurrent threads may report
    // the same exhaustion with differing header precision.
    if state
        .circuit_open_until
        .is_none_or(|current| until > current)
    {
        state.circuit_open_until = Some(until);
    }
}

/// Block until `host` may be contacted per its policy, then reserve the slot;
/// errors only when the required wait exceeds [`MAX_WAIT`].
pub fn acquire(host: &str) -> Result<()> {
    let Some(policy) = policy_for(host) else {
        return Ok(());
    };
    loop {
        let now = Instant::now();
        let wait = {
            let mut states = DOMAIN_STATES.lock().unwrap_or_else(PoisonError::into_inner);
            let state = states.entry(host.to_owned()).or_default();
            match reserve_slot(state, policy, now) {
                Reservation::Ready => return Ok(()),
                Reservation::Wait(wait) => wait,
            }
        };
        if wait > MAX_WAIT {
            return Err(StackError::DomainRateLimited {
                domain: host.to_owned(),
                retry_after_secs: wait.as_secs(),
            });
        }
        tracing::info!(
            domain = host,
            wait_secs = wait.as_secs(),
            "waiting for per-domain rate limit",
        );
        std::thread::sleep(wait);
    }
}

/// Record that `host` answered with a rate-limit response, opening its circuit
/// until `retry_after` or [`DEFAULT_COOLDOWN`] elapses.
pub fn report_rate_limited(host: &str, retry_after: Option<Duration>) {
    if policy_for(host).is_none() {
        return;
    }
    let cooldown = retry_after.unwrap_or(DEFAULT_COOLDOWN).min(MAX_COOLDOWN);
    let now = Instant::now();
    let mut states = DOMAIN_STATES.lock().unwrap_or_else(PoisonError::into_inner);
    let state = states.entry(host.to_owned()).or_default();
    open_circuit(state, now, cooldown);
    tracing::warn!(
        domain = host,
        cooldown_secs = cooldown.as_secs(),
        "domain reported rate limiting; opening circuit",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> DomainPolicy {
        DomainPolicy {
            min_interval: Duration::from_secs(10),
        }
    }

    #[test]
    fn unknown_host_has_no_policy() {
        assert!(policy_for("registry.npmjs.org").is_none());
        assert!(policy_for(GITHUB_API_HOST).is_some());
    }

    #[test]
    fn first_reservation_is_ready_and_spaces_the_next() {
        let mut state = DomainState::default();
        let now = Instant::now();
        assert!(matches!(
            reserve_slot(&mut state, policy(), now),
            Reservation::Ready
        ));
        match reserve_slot(&mut state, policy(), now + Duration::from_secs(3)) {
            Reservation::Wait(wait) => assert_eq!(wait, Duration::from_secs(7)),
            Reservation::Ready => panic!("second immediate reservation must wait"),
        }
    }

    #[test]
    fn reservation_ready_again_after_interval() {
        let mut state = DomainState::default();
        let now = Instant::now();
        assert!(matches!(
            reserve_slot(&mut state, policy(), now),
            Reservation::Ready
        ));
        assert!(matches!(
            reserve_slot(&mut state, policy(), now + Duration::from_secs(10)),
            Reservation::Ready
        ));
    }

    #[test]
    fn open_circuit_blocks_until_cooldown_elapses() {
        let mut state = DomainState::default();
        let now = Instant::now();
        open_circuit(&mut state, now, Duration::from_secs(300));
        match reserve_slot(&mut state, policy(), now + Duration::from_secs(60)) {
            Reservation::Wait(wait) => assert_eq!(wait, Duration::from_secs(240)),
            Reservation::Ready => panic!("open circuit must block"),
        }
        assert!(matches!(
            reserve_slot(&mut state, policy(), now + Duration::from_secs(300)),
            Reservation::Ready
        ));
        assert!(state.circuit_open_until.is_none());
    }

    #[test]
    fn open_circuit_never_shortens_an_existing_cooldown() {
        let mut state = DomainState::default();
        let now = Instant::now();
        open_circuit(&mut state, now, Duration::from_secs(300));
        open_circuit(&mut state, now, Duration::from_secs(30));
        match reserve_slot(&mut state, policy(), now) {
            Reservation::Wait(wait) => assert_eq!(wait, Duration::from_secs(300)),
            Reservation::Ready => panic!("circuit must stay open"),
        }
    }

    #[test]
    fn acquire_passes_unknown_hosts_through() {
        acquire("registry.npmjs.org").expect("no policy, no wait");
    }

    #[test]
    fn report_rate_limited_ignores_hosts_without_policy() {
        report_rate_limited("registry.npmjs.org", Some(Duration::from_secs(3600)));
        acquire("registry.npmjs.org").expect("no circuit for unmanaged hosts");
    }

    #[test]
    fn header_cooldowns_are_clamped_to_max() {
        let mut state = DomainState::default();
        let now = Instant::now();
        open_circuit(
            &mut state,
            now,
            Duration::from_secs(u64::MAX).min(MAX_COOLDOWN),
        );
        match reserve_slot(&mut state, policy(), now) {
            Reservation::Wait(wait) => assert_eq!(wait, MAX_COOLDOWN),
            Reservation::Ready => panic!("clamped circuit must still block"),
        }
    }

    /// Clear a host's process-global state so an armed circuit cannot leak
    /// order-dependent failures into other tests.
    fn reset_host(host: &str) {
        DOMAIN_STATES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(host);
    }

    #[test]
    fn wait_beyond_cap_is_a_typed_error_not_a_sleep() {
        report_rate_limited(GITHUB_API_HOST, Some(Duration::from_secs(u64::MAX)));
        let result = acquire(GITHUB_API_HOST);
        reset_host(GITHUB_API_HOST);
        match result {
            Err(StackError::DomainRateLimited { domain, .. }) => {
                assert_eq!(domain, GITHUB_API_HOST);
            }
            other => panic!("expected DomainRateLimited, got {other:?}"),
        }
    }
}

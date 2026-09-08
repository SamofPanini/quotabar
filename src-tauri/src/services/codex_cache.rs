use crate::domain::account::{AccountCacheKey, RouteKey};
use crate::domain::models::CodexRateLimits;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AuthFileStamp {
    pub(super) len: u64,
    pub(super) modified: SystemTime,
}
struct Cached {
    key: AccountCacheKey,
    stamp: AuthFileStamp,
    sequence: u64,
    limits: CodexRateLimits,
}
#[derive(Default)]
struct RouteState {
    cached: Option<Cached>,
    known_auth_failure: u64,
    unknown_auth_failure: u64,
}
#[derive(Default)]
struct Cache {
    routes: HashMap<RouteKey, RouteState>,
}
fn stale(limits: &CodexRateLimits, error: String) -> CodexRateLimits {
    let mut value = limits.clone();
    value.error = Some(error);
    value
}
impl Cache {
    fn retain(&self, key: Option<&AccountCacheKey>, error: String) -> CodexRateLimits {
        key.and_then(|key| {
            self.routes
                .get(key.route())
                .and_then(|state| state.cached.as_ref())
                .filter(|cached| &cached.key == key)
        })
        .map_or_else(
            || CodexRateLimits::disconnected(error.clone()),
            |cached| stale(&cached.limits, error.clone()),
        )
    }
    fn retain_stamp(
        &self,
        route: &RouteKey,
        stamp: Option<&AuthFileStamp>,
        error: String,
    ) -> CodexRateLimits {
        self.routes
            .get(route)
            .and_then(|state| state.cached.as_ref())
            .filter(|cached| stamp.is_some_and(|stamp| cached.stamp == *stamp))
            .map_or_else(
                || CodexRateLimits::disconnected(error.clone()),
                |cached| stale(&cached.limits, error.clone()),
            )
    }
    fn store(
        &mut self,
        key: AccountCacheKey,
        stamp: AuthFileStamp,
        sequence: u64,
        limits: CodexRateLimits,
    ) -> bool {
        let state = self.routes.entry(key.route().clone()).or_default();
        if state.known_auth_failure >= sequence
            || state.unknown_auth_failure >= sequence
            || state
                .cached
                .as_ref()
                .is_some_and(|cached| cached.sequence > sequence)
        {
            return false;
        }
        state.cached = Some(Cached {
            key,
            stamp,
            sequence,
            limits,
        });
        true
    }
    fn invalidate(&mut self, route: &RouteKey, key: Option<&AccountCacheKey>, sequence: u64) {
        let state = self.routes.entry(route.clone()).or_default();
        if key.is_some() {
            state.known_auth_failure = state.known_auth_failure.max(sequence);
        } else {
            state.unknown_auth_failure = state.unknown_auth_failure.max(sequence);
        }
        if state.cached.as_ref().is_some_and(|cached| {
            cached.sequence <= sequence && key.is_none_or(|key| cached.key == *key)
        }) {
            state.cached = None;
        }
    }
}
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
static NEXT: AtomicU64 = AtomicU64::new(1);
fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}
pub(super) fn retain_for_account(
    key: Option<&AccountCacheKey>,
    error: String,
) -> Result<CodexRateLimits, String> {
    cache()
        .lock()
        .map_err(|e| e.to_string())
        .map(|cache| cache.retain(key, error))
}
pub(super) fn retain_for_auth_stamp(
    route: &RouteKey,
    stamp: Option<&AuthFileStamp>,
    error: String,
) -> Result<CodexRateLimits, String> {
    cache()
        .lock()
        .map_err(|e| e.to_string())
        .map(|cache| cache.retain_stamp(route, stamp, error))
}
pub(super) fn store(
    key: AccountCacheKey,
    stamp: AuthFileStamp,
    sequence: u64,
    limits: CodexRateLimits,
) -> Result<bool, String> {
    Ok(cache()
        .lock()
        .map_err(|e| e.to_string())?
        .store(key, stamp, sequence, limits))
}
pub(super) fn invalidate(
    route: &RouteKey,
    key: Option<&AccountCacheKey>,
    sequence: u64,
) -> Result<(), String> {
    cache()
        .lock()
        .map_err(|e| e.to_string())?
        .invalidate(route, key, sequence);
    Ok(())
}
pub(super) fn next_request_sequence() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::{AuthFileStamp, Cache};
    use crate::domain::account::{CodexProfile, CodexProfileInput};
    use crate::domain::models::{CodexRateLimitWindow, CodexRateLimits};
    use std::fs;
    use std::time::{Duration, UNIX_EPOCH};
    fn profile(name: &str) -> CodexProfile {
        let path =
            std::env::temp_dir().join(format!("quotabar-cache-{name}-{}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        CodexProfile::from_input(CodexProfileInput {
            profile_id: format!("codex/{name}"),
            home: Some(path),
        })
        .unwrap()
    }
    fn limits() -> CodexRateLimits {
        CodexRateLimits {
            connected: true,
            plan_type: None,
            primary: Some(CodexRateLimitWindow {
                used_percent: 10.0,
                window_minutes: None,
                resets_at: None,
            }),
            secondary: None,
            credits: None,
            error: None,
        }
    }
    fn stamp(n: u64) -> AuthFileStamp {
        AuthFileStamp {
            len: n,
            modified: UNIX_EPOCH + Duration::from_secs(n),
        }
    }
    #[test]
    fn switched_account_transient_does_not_borrow_cache() {
        let mut cache = Cache::default();
        let p = profile("switch");
        let a = p.cache_key("a");
        assert!(cache.store(a, stamp(1), 1, limits()));
        assert!(!cache.retain(Some(&p.cache_key("b")), "x".into()).connected);
    }
    #[test]
    fn auth_stamp_mismatch_or_replacement_does_not_retain() {
        let mut cache = Cache::default();
        let p = profile("stamp");
        assert!(cache.store(p.cache_key("a"), stamp(1), 1, limits()));
        assert!(
            !cache
                .retain_stamp(p.route(), Some(&stamp(2)), "x".into())
                .connected
        );
    }
    #[test]
    fn unknown_auth_invalidation_is_route_local() {
        let mut cache = Cache::default();
        let a = profile("ua");
        let b = profile("ub");
        let ka = a.cache_key("x");
        let kb = b.cache_key("x");
        cache.store(ka.clone(), stamp(1), 1, limits());
        cache.store(kb.clone(), stamp(1), 1, limits());
        cache.invalidate(a.route(), None, 2);
        assert!(!cache.retain(Some(&ka), "x".into()).connected);
        assert!(cache.retain(Some(&kb), "x".into()).connected);
    }
    #[test]
    fn auth_failure_blocks_older_success() {
        let mut cache = Cache::default();
        let p = profile("block");
        let k = p.cache_key("a");
        cache.invalidate(p.route(), Some(&k), 2);
        assert!(!cache.store(k, stamp(1), 1, limits()));
    }
    #[test]
    fn older_auth_failure_cannot_clear_newer_success() {
        let mut cache = Cache::default();
        let p = profile("newer");
        let k = p.cache_key("a");
        cache.store(k.clone(), stamp(1), 3, limits());
        cache.invalidate(p.route(), Some(&k), 2);
        assert!(cache.retain(Some(&k), "x".into()).connected);
    }
    #[test]
    fn older_success_cannot_replace_newer_success() {
        let mut cache = Cache::default();
        let p = profile("order");
        let k = p.cache_key("a");
        assert!(cache.store(k.clone(), stamp(2), 2, limits()));
        assert!(!cache.store(k, stamp(1), 1, limits()));
    }
    #[test]
    fn other_route_is_unaffected_by_known_auth_failure() {
        let mut cache = Cache::default();
        let a = profile("ka");
        let b = profile("kb");
        let ka = a.cache_key("a");
        let kb = b.cache_key("b");
        cache.store(ka.clone(), stamp(1), 1, limits());
        cache.store(kb.clone(), stamp(1), 1, limits());
        cache.invalidate(a.route(), Some(&ka), 2);
        assert!(cache.retain(Some(&kb), "x".into()).connected);
    }
    #[test]
    fn duplicate_claims_across_routes_are_isolated() {
        let mut cache = Cache::default();
        let a = profile("da");
        let b = profile("db");
        let ka = a.cache_key("same");
        let kb = b.cache_key("same");
        cache.store(ka.clone(), stamp(1), 1, limits());
        cache.store(kb.clone(), stamp(1), 1, limits());
        cache.invalidate(a.route(), Some(&ka), 2);
        assert!(cache.retain(Some(&kb), "x".into()).connected);
    }
}

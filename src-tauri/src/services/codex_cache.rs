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
    known_auth_failures: HashMap<AccountCacheKey, u64>,
    unknown_auth_failure: Option<u64>,
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
        if state
            .known_auth_failures
            .get(&key)
            .is_some_and(|failure| *failure >= sequence)
            || state
                .unknown_auth_failure
                .is_some_and(|failure| failure >= sequence)
            || state
                .cached
                .as_ref()
                .is_some_and(|cached| cached.sequence > sequence)
        {
            return false;
        }
        // A newer verified success supersedes earlier authentication outcomes
        // for this credential route, but not a newer failure for another claim.
        state.known_auth_failures.remove(&key);
        state.unknown_auth_failure = None;
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
        if let Some(key) = key {
            state
                .known_auth_failures
                .entry(key.clone())
                .and_modify(|failure| *failure = (*failure).max(sequence))
                .or_insert(sequence);
        } else {
            state.unknown_auth_failure = Some(
                state
                    .unknown_auth_failure
                    .map_or(sequence, |failure| failure.max(sequence)),
            );
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
    fn transient_failure_preserves_only_the_same_accounts_limits() {
        let mut cache = Cache::default();
        let p = profile("switch");
        let a = p.cache_key("account-a");
        assert!(cache.store(a.clone(), stamp(512), 1, limits()));
        let error = "Network error: operation timed out".to_string();
        let same = cache.retain(Some(&a), error.clone());
        let switched = cache.retain(Some(&p.cache_key("account-b")), error.clone());
        let unknown = cache.retain(None, error);
        assert!(same.connected);
        assert_eq!(same.primary.unwrap().used_percent, 10.0);
        assert_eq!(same.error.as_deref(), Some("Network error: operation timed out"));
        assert!(!switched.connected);
        assert!(!unknown.connected);
    }
    #[test]
    fn transient_auth_read_preserves_only_the_same_file_stamp() {
        let mut cache = Cache::default();
        let p = profile("stamp");
        assert!(cache.store(p.cache_key("account-a"), stamp(512), 1, limits()));
        let error = "Failed to read auth.json: too many open files".to_string();
        let same = cache.retain_stamp(p.route(), Some(&stamp(512)), error.clone());
        let changed_len = cache.retain_stamp(
            p.route(),
            Some(&AuthFileStamp {
                len: 513,
                modified: UNIX_EPOCH + Duration::from_secs(512),
            }),
            error.clone(),
        );
        let changed_mtime = cache.retain_stamp(
            p.route(),
            Some(&AuthFileStamp {
                len: 512,
                modified: UNIX_EPOCH + Duration::from_secs(513),
            }),
            error.clone(),
        );
        let unknown = cache.retain_stamp(p.route(), None, error);
        assert!(same.connected);
        assert_eq!(
            same.error.as_deref(),
            Some("Failed to read auth.json: too many open files")
        );
        assert!(!changed_len.connected);
        assert!(!changed_mtime.connected);
        assert!(!unknown.connected);
    }
    #[test]
    fn unknown_account_authentication_failure_clears_older_cached_limits() {
        let mut cache = Cache::default();
        let p = profile("unknown");
        let key = p.cache_key("account-a");
        cache.store(key.clone(), stamp(512), 1, limits());
        cache.invalidate(p.route(), None, 2);
        let result = cache.retain(Some(&key), "Network error: operation timed out".into());
        assert!(!result.connected);
        assert!(result.primary.is_none());
    }
    #[test]
    fn authentication_failure_clears_same_account_after_auth_file_changes() {
        let mut cache = Cache::default();
        let p = profile("same");
        let k = p.cache_key("account-a");
        cache.store(k.clone(), stamp(512), 1, limits());
        cache.invalidate(p.route(), Some(&k), 2);
        assert!(!cache.retain(Some(&k), "timeout".into()).connected);
    }
    #[test]
    fn older_authentication_failure_does_not_clear_new_same_account_limits() {
        let mut cache = Cache::default();
        let p = profile("newer");
        let k = p.cache_key("account-a");
        cache.store(k.clone(), stamp(513), 3, limits());
        cache.invalidate(p.route(), Some(&k), 2);
        assert!(cache.retain(Some(&k), "x".into()).connected);
    }
    #[test]
    fn authentication_failure_blocks_an_older_success_from_repopulating_cache() {
        let mut cache = Cache::default();
        let p = profile("block");
        let k = p.cache_key("account-a");
        cache.store(k.clone(), stamp(512), 1, limits());
        cache.invalidate(p.route(), Some(&k), 3);
        assert!(!cache.store(k.clone(), stamp(512), 2, limits()));
        assert!(!cache.retain(Some(&k), "timeout".into()).connected);
    }
    #[test]
    fn older_success_does_not_replace_newer_cached_limits() {
        let mut cache = Cache::default();
        let p = profile("order");
        let k = p.cache_key("account-a");
        assert!(cache.store(k.clone(), stamp(513), 3, limits()));
        assert!(!cache.store(k, stamp(512), 2, limits()));
    }
    #[test]
    fn old_authentication_failure_does_not_clear_a_different_accounts_cache() {
        let mut cache = Cache::default();
        let profile = profile("same-route");
        let account_a = profile.cache_key("account-a");
        let account_b = profile.cache_key("account-b");
        cache.store(account_a.clone(), stamp(1), 3, limits());
        cache.invalidate(profile.route(), Some(&account_b), 2);
        assert!(cache.retain(Some(&account_a), "x".into()).connected);
    }
    #[test]
    fn known_auth_failure_for_one_claim_does_not_block_another_claim_on_that_route() {
        let mut cache = Cache::default();
        let profile = profile("same-route-ordering");
        let failed = profile.cache_key("account-a");
        let succeeding = profile.cache_key("account-b");
        cache.invalidate(profile.route(), Some(&failed), 3);
        assert!(cache.store(succeeding.clone(), stamp(512), 2, limits()));
        assert!(cache.retain(Some(&succeeding), "timeout".into()).connected);
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

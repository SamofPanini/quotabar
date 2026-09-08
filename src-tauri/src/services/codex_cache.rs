use crate::domain::account::AccountCacheKey;
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

struct CachedCodexRateLimits {
    auth_stamp: AuthFileStamp,
    request_sequence: u64,
    limits: CodexRateLimits,
}

/// Every map is keyed by profile + resolved account. The profile portion prevents a second home
/// that happens to resolve to the same ChatGPT account from borrowing another route's quota.
#[derive(Default)]
struct CodexRateLimitCache {
    cached: HashMap<AccountCacheKey, CachedCodexRateLimits>,
    auth_failures: HashMap<AccountCacheKey, u64>,
    unknown_auth_failures: HashMap<String, u64>,
}

impl CodexRateLimitCache {
    fn retain_for_account(&self, key: Option<&AccountCacheKey>, error: String) -> CodexRateLimits {
        key.and_then(|key| self.cached.get(key)).map_or_else(
            || CodexRateLimits::disconnected(error.clone()),
            |stale| stale_limits_with_error(&stale.limits, error.clone()),
        )
    }
    fn retain_for_auth_stamp(
        &self,
        profile_id: &str,
        stamp: Option<&AuthFileStamp>,
        error: String,
    ) -> CodexRateLimits {
        let Some(stamp) = stamp else {
            return CodexRateLimits::disconnected(error);
        };
        self.cached
            .iter()
            .find(|(key, cached)| key.profile_id() == profile_id && cached.auth_stamp == *stamp)
            .map_or_else(
                || CodexRateLimits::disconnected(error.clone()),
                |(_, stale)| stale_limits_with_error(&stale.limits, error.clone()),
            )
    }
    fn store(
        &mut self,
        key: AccountCacheKey,
        stamp: AuthFileStamp,
        sequence: u64,
        limits: CodexRateLimits,
    ) -> bool {
        if self
            .auth_failures
            .get(&key)
            .is_some_and(|failed| *failed >= sequence)
            || self
                .unknown_auth_failures
                .get(key.profile_id())
                .is_some_and(|failed| *failed >= sequence)
            || self
                .cached
                .get(&key)
                .is_some_and(|cached| cached.request_sequence > sequence)
        {
            return false;
        }
        self.auth_failures.remove(&key);
        self.unknown_auth_failures.remove(key.profile_id());
        self.cached.insert(
            key,
            CachedCodexRateLimits {
                auth_stamp: stamp,
                request_sequence: sequence,
                limits,
            },
        );
        true
    }
    fn invalidate(&mut self, key: Option<&AccountCacheKey>, profile_id: &str, sequence: u64) {
        match key {
            Some(key) => {
                self.auth_failures
                    .entry(key.clone())
                    .and_modify(|old| *old = (*old).max(sequence))
                    .or_insert(sequence);
            }
            None => {
                self.unknown_auth_failures
                    .entry(profile_id.to_string())
                    .and_modify(|old| *old = (*old).max(sequence))
                    .or_insert(sequence);
            }
        }
        self.cached.retain(|cached_key, cached| {
            !(cached.request_sequence <= sequence
                && key.map_or(cached_key.profile_id() == profile_id, |key| {
                    cached_key == key
                }))
        });
    }
}

fn stale_limits_with_error(limits: &CodexRateLimits, error: String) -> CodexRateLimits {
    let mut result = limits.clone();
    result.error = Some(error);
    result
}
static LAST_GOOD_LIMITS: OnceLock<Mutex<CodexRateLimitCache>> = OnceLock::new();
static NEXT_REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);
fn cache() -> &'static Mutex<CodexRateLimitCache> {
    LAST_GOOD_LIMITS.get_or_init(|| Mutex::new(CodexRateLimitCache::default()))
}
pub(super) fn retain_for_account(
    key: Option<&AccountCacheKey>,
    error: String,
) -> Result<CodexRateLimits, String> {
    cache()
        .lock()
        .map_err(|e| format!("last-good cache lock poisoned: {e}"))
        .map(|cache| cache.retain_for_account(key, error))
}
pub(super) fn retain_for_auth_stamp(
    profile_id: &str,
    stamp: Option<&AuthFileStamp>,
    error: String,
) -> Result<CodexRateLimits, String> {
    cache()
        .lock()
        .map_err(|e| format!("last-good cache lock poisoned: {e}"))
        .map(|cache| cache.retain_for_auth_stamp(profile_id, stamp, error))
}
pub(super) fn store(
    key: AccountCacheKey,
    stamp: AuthFileStamp,
    sequence: u64,
    limits: CodexRateLimits,
) -> Result<bool, String> {
    Ok(cache()
        .lock()
        .map_err(|e| format!("last-good cache lock poisoned: {e}"))?
        .store(key, stamp, sequence, limits))
}
pub(super) fn invalidate(
    key: Option<&AccountCacheKey>,
    profile_id: &str,
    sequence: u64,
) -> Result<(), String> {
    cache()
        .lock()
        .map_err(|e| format!("last-good cache lock poisoned: {e}"))?
        .invalidate(key, profile_id, sequence);
    Ok(())
}
pub(super) fn next_request_sequence() -> u64 {
    NEXT_REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::{AuthFileStamp, CodexRateLimitCache};
    use crate::domain::account::{codex_account, CodexProfile, CodexProfileInput};
    use crate::domain::models::{CodexRateLimitWindow, CodexRateLimits};
    use std::time::{Duration, UNIX_EPOCH};
    fn limits(used: f64) -> CodexRateLimits {
        CodexRateLimits {
            connected: true,
            plan_type: None,
            primary: Some(CodexRateLimitWindow {
                used_percent: used,
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
    fn key(profile: &str, account: &str) -> crate::domain::account::AccountCacheKey {
        codex_account(
            CodexProfile::from_input(CodexProfileInput {
                profile_id: profile.into(),
                home: None,
            })
            .unwrap(),
        )
        .cache_key_for_resolved_id(account)
    }
    #[test]
    fn profile_cache_isolation_includes_duplicate_resolved_account_and_failures() {
        let mut cache = CodexRateLimitCache::default();
        let a = key("codex/a", "acct");
        let b = key("codex/b", "acct");
        assert!(cache.store(a.clone(), stamp(1), 1, limits(10.0)));
        assert!(cache.store(b.clone(), stamp(2), 1, limits(20.0)));
        cache.invalidate(Some(&a), "codex/a", 2);
        assert!(
            !cache
                .retain_for_account(Some(&a), "timeout".into())
                .connected
        );
        assert_eq!(
            cache
                .retain_for_account(Some(&b), "timeout".into())
                .primary
                .unwrap()
                .used_percent,
            20.0
        );
    }
    #[test]
    fn auth_stamp_fallback_and_unknown_auth_failure_are_profile_scoped() {
        let mut cache = CodexRateLimitCache::default();
        let a = key("codex/a", "acct-a");
        let b = key("codex/b", "acct-b");
        cache.store(a, stamp(1), 1, limits(10.0));
        cache.store(b.clone(), stamp(1), 1, limits(20.0));
        assert!(
            cache
                .retain_for_auth_stamp("codex/a", Some(&stamp(1)), "io".into())
                .connected
        );
        assert!(
            cache
                .retain_for_auth_stamp("codex/b", Some(&stamp(1)), "io".into())
                .connected
        );
        cache.invalidate(None, "codex/a", 2);
        assert!(cache.retain_for_account(Some(&b), "io".into()).connected);
    }
    #[test]
    fn older_success_cannot_replace_newer_success_for_same_profile_account() {
        let mut cache = CodexRateLimitCache::default();
        let a = key("codex/a", "acct");
        assert!(cache.store(a.clone(), stamp(2), 2, limits(20.0)));
        assert!(!cache.store(a, stamp(1), 1, limits(10.0)));
    }
}

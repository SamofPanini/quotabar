use crate::domain::account::{AccountCacheKey, RouteKey};
use crate::domain::models::CodexRateLimits;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AuthFileStamp { pub(super) len: u64, pub(super) modified: SystemTime }
struct Cached { key: AccountCacheKey, stamp: AuthFileStamp, sequence: u64, limits: CodexRateLimits }
#[derive(Default)] struct RouteState { cached: Option<Cached>, known_auth_failure: u64, unknown_auth_failure: u64 }
#[derive(Default)] struct Cache { routes: HashMap<RouteKey, RouteState> }
fn stale(limits: &CodexRateLimits, error: String) -> CodexRateLimits { let mut value = limits.clone(); value.error = Some(error); value }
impl Cache {
    fn retain(&self, key: Option<&AccountCacheKey>, error: String) -> CodexRateLimits { key.and_then(|key| self.routes.get(key.route()).and_then(|state| state.cached.as_ref()).filter(|cached| &cached.key == key)).map_or_else(|| CodexRateLimits::disconnected(error.clone()), |cached| stale(&cached.limits, error.clone())) }
    fn retain_stamp(&self, route: &RouteKey, stamp: Option<&AuthFileStamp>, error: String) -> CodexRateLimits { self.routes.get(route).and_then(|state| state.cached.as_ref()).filter(|cached| stamp.is_some_and(|stamp| cached.stamp == *stamp)).map_or_else(|| CodexRateLimits::disconnected(error.clone()), |cached| stale(&cached.limits, error.clone())) }
    fn store(&mut self, key: AccountCacheKey, stamp: AuthFileStamp, sequence: u64, limits: CodexRateLimits) -> bool {
        let state = self.routes.entry(key.route().clone()).or_default();
        if state.known_auth_failure >= sequence || state.unknown_auth_failure >= sequence || state.cached.as_ref().is_some_and(|cached| cached.sequence > sequence) { return false; }
        state.cached = Some(Cached { key, stamp, sequence, limits }); true
    }
    fn invalidate(&mut self, route: &RouteKey, key: Option<&AccountCacheKey>, sequence: u64) {
        let state = self.routes.entry(route.clone()).or_default();
        if key.is_some() { state.known_auth_failure = state.known_auth_failure.max(sequence); } else { state.unknown_auth_failure = state.unknown_auth_failure.max(sequence); }
        if state.cached.as_ref().is_some_and(|cached| cached.sequence <= sequence && key.is_none_or(|key| cached.key == *key)) { state.cached = None; }
    }
}
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new(); static NEXT: AtomicU64 = AtomicU64::new(1);
fn cache() -> &'static Mutex<Cache> { CACHE.get_or_init(|| Mutex::new(Cache::default())) }
pub(super) fn retain_for_account(key: Option<&AccountCacheKey>, error: String) -> Result<CodexRateLimits, String> { cache().lock().map_err(|e| e.to_string()).map(|cache| cache.retain(key, error)) }
pub(super) fn retain_for_auth_stamp(route: &RouteKey, stamp: Option<&AuthFileStamp>, error: String) -> Result<CodexRateLimits, String> { cache().lock().map_err(|e| e.to_string()).map(|cache| cache.retain_stamp(route, stamp, error)) }
pub(super) fn store(key: AccountCacheKey, stamp: AuthFileStamp, sequence: u64, limits: CodexRateLimits) -> Result<bool, String> { Ok(cache().lock().map_err(|e| e.to_string())?.store(key, stamp, sequence, limits)) }
pub(super) fn invalidate(route: &RouteKey, key: Option<&AccountCacheKey>, sequence: u64) -> Result<(), String> { cache().lock().map_err(|e| e.to_string())?.invalidate(route, key, sequence); Ok(()) }
pub(super) fn next_request_sequence() -> u64 { NEXT.fetch_add(1, Ordering::Relaxed) }

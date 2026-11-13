use crate::domain::models::{
    ClaudeAppAccount, ClaudeAutoRenewStatus, ClaudeStaleReason, QuotaData, UsageInfo,
};
use crate::services::http::{is_transient_os_error, shared_http_client};
use serde::Serialize;
use std::collections::HashMap;
#[cfg(not(test))]
use std::fs::OpenOptions;
use std::future::Future;
#[cfg(not(test))]
use std::io::Write as IoWrite;
use std::path::Path;
use std::pin::Pin;
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Default, serde::Deserialize)]
#[serde(untagged)]
enum DesktopUuid {
    Text(String),
    Invalid(serde::de::IgnoredAny),
    #[default]
    Missing,
}

#[derive(serde::Deserialize)]
struct DesktopAccountFile {
    #[serde(rename = "lastKnownAccountUuid", default)]
    last_known_account_uuid: DesktopUuid,
}

fn uuid_tail(value: &str) -> Option<String> {
    let uuid = uuid::Uuid::parse_str(value).ok()?;
    let full = uuid.simple().to_string();
    Some(full[full.len() - 6..].to_string())
}

fn desktop_account_tail_from_content(content: Result<&str, ()>) -> Result<String, &'static str> {
    let content = content.map_err(|_| "missing")?;
    let parsed: DesktopAccountFile = serde_json::from_str(content).map_err(|_| "parse")?;
    match parsed.last_known_account_uuid {
        DesktopUuid::Text(uuid) => uuid_tail(&uuid).ok_or("notUuid"),
        DesktopUuid::Invalid(_) => Err("notUuid"),
        DesktopUuid::Missing => Err("noKey"),
    }
}

fn desktop_account_status(
    default_tail: Option<&str>,
    labels: &HashMap<String, String>,
) -> ClaudeAppAccount {
    let path =
        dirs::home_dir().map(|home| home.join("Library/Application Support/Claude/config.json"));
    let content = path.and_then(|path| std::fs::read_to_string(path).ok());
    desktop_account_status_from(default_tail, labels, content.as_deref().ok_or(()))
}

fn desktop_account_status_from(
    default_tail: Option<&str>,
    labels: &HashMap<String, String>,
    content: Result<&str, ()>,
) -> ClaudeAppAccount {
    match desktop_account_tail_from_content(content) {
        Ok(tail) => match default_tail {
            Some(default) if default == tail => ClaudeAppAccount::Same,
            Some(_) => ClaudeAppAccount::Different {
                app_label: labels.get(&tail).cloned().unwrap_or(tail),
            },
            None => ClaudeAppAccount::Unknown,
        },
        Err(reason) => {
            static REASONS: OnceLock<Mutex<std::collections::HashSet<&'static str>>> =
                OnceLock::new();
            let mut seen = REASONS
                .get_or_init(|| Mutex::new(std::collections::HashSet::new()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if seen.insert(reason) {
                log_msg(&format!("[Claude] app account: unknown ({reason})"));
            }
            ClaudeAppAccount::Unknown
        }
    }
}

#[derive(serde::Deserialize)]
struct ClaudeConfigIdentity {
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<ClaudeConfigAccount>,
}

#[derive(serde::Deserialize)]
struct ClaudeConfigAccount {
    #[serde(rename = "accountUuid")]
    account_uuid: Option<String>,
}

fn cross_check_identity(config_dir: Option<&str>, identity: &Identity) -> bool {
    let path = match config_dir {
        Some(dir) => std::path::PathBuf::from(dir).join(".claude.json"),
        None => match dirs::home_dir() {
            Some(home) => home.join(".claude.json"),
            None => return false,
        },
    };
    cross_check_identity_from(std::fs::read_to_string(path).ok().as_deref(), identity)
}

fn cross_check_identity_from(content: Option<&str>, identity: &Identity) -> bool {
    let saved = content
        .and_then(|content| serde_json::from_str::<ClaudeConfigIdentity>(content).ok())
        .and_then(|config| config.oauth_account?.account_uuid)
        .and_then(|value| uuid::Uuid::parse_str(&value).ok());
    match (saved, uuid::Uuid::parse_str(&identity.uuid).ok()) {
        (Some(saved), Some(found)) => saved != found,
        _ => false,
    }
}

async fn resolve_identity(
    state: &ProfileState,
    credentials: &CachedCredentials,
) -> Option<Identity> {
    resolve_identity_with(state, credentials, Instant::now(), || {
        fetch_profile_identity(&credentials.access_token)
    })
    .await
}

async fn resolve_identity_with<F, Fut>(
    state: &ProfileState,
    credentials: &CachedCredentials,
    now: Instant,
    fetch: F,
) -> Option<Identity>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Option<Identity>>,
{
    if let Some(identity) = &credentials.identity {
        return Some(identity.clone());
    }
    {
        let mut cache = state.identity_cache.lock().ok()?;
        if let Some(previous) = cache.as_ref() {
            if previous.expires_at_ms == credentials.expires_at_ms
                && previous.service == credentials.service
                && (previous.identity.is_some()
                    || now.saturating_duration_since(previous.attempted_at)
                        < Duration::from_secs(600))
            {
                return previous.identity.clone();
            }
        }
        *cache = Some(IdentityCache {
            expires_at_ms: credentials.expires_at_ms,
            service: credentials.service.clone(),
            attempted_at: now,
            identity: None,
        });
    }
    let identity = fetch().await;
    if let Ok(mut cache) = state.identity_cache.lock() {
        if let Some(entry) = cache.as_mut() {
            if entry.expires_at_ms == credentials.expires_at_ms
                && entry.service == credentials.service
            {
                entry.identity = identity.clone();
            }
        }
    }
    identity
}

async fn fetch_profile_identity(access_token: &str) -> Option<Identity> {
    let response = match shared_http_client() {
        Ok(client) => client
            .get("https://api.anthropic.com/api/oauth/profile")
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {access_token}"))
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("User-Agent", "claude-code/1.0.0")
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .ok(),
        Err(_) => None,
    };
    if let Some(response) = response {
        if response.status().is_success() {
            response
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|value| {
                    let account = value.get("account")?;
                    let uuid = account.get("uuid")?.as_str()?;
                    uuid_tail(uuid)?;
                    Some(Identity {
                        uuid: uuid.to_string(),
                        email: account
                            .get("email")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned),
                        source: crate::services::quota_history::AccountSource::ProfileApi,
                    })
                })
        } else {
            None
        }
    } else {
        None
    }
}

const TOKEN_CACHE_TTL: Duration = Duration::from_secs(300);
const QUOTA_CACHE_TTL: Duration = Duration::from_secs(120);
const MAX_STALE_QUOTA_AGE: Duration = Duration::from_secs(15 * 60);
const EXPIRY_SAFETY_WINDOW_MS: u64 = 60_000;
const AUTO_RENEW_INTERVAL_MS: u64 = 10 * 60 * 1_000;
const MANUAL_RENEW_INTERVAL_MS: u64 = 60 * 1_000;
const DEFAULT_RETRY_AFTER_SECS: u64 = 300;
const CLAUDE_TOKEN_ENV_KEY: &str = "CLAUDE_CODE_OAUTH_TOKEN";
const CLAUDE_AUTH_RELOGIN_MESSAGE: &str = "Claude Code login expired. Press Ping to renew it.";
const CLAUDE_SIGNED_OUT_MESSAGE: &str =
    "Claude Code is signed out. Run claude auth login in Terminal.";
#[cfg(not(test))]
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

const CREDENTIAL_NAMES: [&str; 4] = [
    "Claude Code-credentials",
    "claude-credentials",
    "Claude-credentials",
    "claudecode-credentials",
];
const FABLE5_QUOTA_KEYS: [&str; 5] = [
    "seven_day_fable5",
    "seven_day_fable_5",
    "seven_day_fable",
    "seven_day_claude_fable5",
    "seven_day_claude_fable_5",
];

static REQUEST_COUNT: AtomicU64 = AtomicU64::new(0);
static CLAUDE_AUTO_RENEW_ENABLED: AtomicBool = AtomicBool::new(true);

#[derive(Default)]
struct ProfileState {
    last_request_time: Mutex<Option<Instant>>,
    last_auto_renew_attempt_ms: Mutex<Option<u64>>,
    last_manual_renew_attempt_ms: Mutex<Option<u64>>,
    signed_out_mark: Mutex<Option<Option<u64>>>,
    auto_ping_history: Mutex<Option<AutoPingAttempt>>,
    credentials_cache: Mutex<Option<CachedCredentials>>,
    quota_cache: Mutex<Option<CachedQuota>>,
    last_successful_quota: Mutex<Option<LastSuccessfulQuota>>,
    request_gate_state: Mutex<RequestGateState>,
    identity_cache: Mutex<Option<IdentityCache>>,
}

#[derive(Clone)]
struct Identity {
    uuid: String,
    email: Option<String>,
    source: crate::services::quota_history::AccountSource,
}

struct IdentityCache {
    expires_at_ms: Option<u64>,
    service: String,
    attempted_at: Instant,
    identity: Option<Identity>,
}

fn default_state() -> &'static Arc<ProfileState> {
    static STATE: OnceLock<Arc<ProfileState>> = OnceLock::new();
    STATE.get_or_init(|| Arc::new(ProfileState::default()))
}

fn profile_state(key: &str) -> Arc<ProfileState> {
    static STATES: OnceLock<Mutex<HashMap<String, Arc<ProfileState>>>> = OnceLock::new();
    if key == "default" {
        return Arc::clone(default_state());
    }
    let mut states = STATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(states.entry(key.to_owned()).or_default())
}

fn last_request_time() -> &'static Mutex<Option<Instant>> {
    &default_state().last_request_time
}

fn last_auto_renew_attempt_ms() -> &'static Mutex<Option<u64>> {
    &default_state().last_auto_renew_attempt_ms
}

fn last_manual_renew_attempt_ms() -> &'static Mutex<Option<u64>> {
    &default_state().last_manual_renew_attempt_ms
}

fn signed_out_mark() -> &'static Mutex<Option<Option<u64>>> {
    &default_state().signed_out_mark
}

#[derive(Clone, Copy)]
struct AutoPingAttempt {
    expires_at_ms: Option<u64>,
    attempted_at_ms: u64,
}

fn auto_ping_history() -> &'static Mutex<Option<AutoPingAttempt>> {
    &default_state().auto_ping_history
}

pub(crate) fn set_auto_renew_enabled(enabled: bool) {
    CLAUDE_AUTO_RENEW_ENABLED.store(enabled, Ordering::Relaxed);
}

#[cfg(not(test))]
fn rotate_log_if_needed(path: &Path) {
    rotate_log_if_needed_with_limit(path, MAX_LOG_BYTES);
}

fn rotate_log_if_needed_with_limit(path: &Path, max_bytes: u64) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() <= max_bytes {
        return;
    }
    let rotated = path.with_extension("log.1");
    if let Err(error) = std::fs::rename(path, &rotated) {
        eprintln!("[log] failed to rotate log: {error}");
    }
}

#[cfg(not(test))]
fn log_msg(msg: &str) {
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let line = format!("[{timestamp}] {msg}\n");

    print!("{line}");

    let log_dir = dirs::home_dir()
        .unwrap_or_default()
        .join("Library/Logs/quotabar");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!("[log] failed to create log dir: {e}");
        return;
    }

    let log_path = log_dir.join("claude.log");
    rotate_log_if_needed(&log_path);

    match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(mut file) => {
            if let Err(e) = file.write_all(line.as_bytes()) {
                eprintln!("[log] failed to write log: {e}");
            }
        }
        Err(e) => eprintln!("[log] failed to open log file: {e}"),
    }
}

#[cfg(test)]
const TEST_LOG_DESTINATION: &str = "memory";

#[cfg(test)]
static TEST_LOG_MESSAGES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

#[cfg(test)]
fn test_log_messages() -> &'static Mutex<Vec<String>> {
    TEST_LOG_MESSAGES.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(test)]
fn log_msg(msg: &str) {
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let line = format!("[{timestamp}] {msg}\n");
    print!("{line}");
    if let Ok(mut messages) = test_log_messages().lock() {
        messages.push(line);
    }
}

fn log_response_headers(response: &reqwest::Response) {
    let headers = response.headers();
    let interesting = [
        "retry-after",
        "x-ratelimit-limit-requests",
        "x-ratelimit-limit-tokens",
        "x-ratelimit-remaining-requests",
        "x-ratelimit-remaining-tokens",
        "x-ratelimit-reset-requests",
        "x-ratelimit-reset-tokens",
        "cf-ray",
        "x-should-retry",
        "request-id",
    ];

    let mut parts = Vec::new();
    for name in interesting {
        if let Some(val) = headers.get(name) {
            let val_str = val.to_str().unwrap_or("?");
            parts.push(format!("{name}={val_str}"));
        }
    }
    if !parts.is_empty() {
        log_msg(&format!("[API] response headers: {}", parts.join(", ")));
    }
}

fn track_request() -> (u64, Option<f64>) {
    let count = REQUEST_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let gap = if let Ok(mut guard) = last_request_time().lock() {
        let gap = guard.map(|t| t.elapsed().as_secs_f64());
        *guard = Some(Instant::now());
        gap
    } else {
        None
    };
    (count, gap)
}

#[derive(Clone)]
struct CachedCredentials {
    access_token: String,
    cached_at: Instant,
    expires_at_ms: Option<u64>,
    service: String,
    identity: Option<Identity>,
}

fn credentials_cache() -> &'static Mutex<Option<CachedCredentials>> {
    &default_state().credentials_cache
}

struct CachedQuota {
    data: QuotaData,
    cached_at: Instant,
}

#[derive(Clone)]
struct LastSuccessfulQuota {
    data: QuotaData,
    recorded_at: String,
}

fn quota_cache() -> &'static Mutex<Option<CachedQuota>> {
    &default_state().quota_cache
}

fn last_successful_quota() -> &'static Mutex<Option<LastSuccessfulQuota>> {
    &default_state().last_successful_quota
}

/// A remembered "401 after a forced re-read". Only the credential's expiry is
/// kept; `expires_at_ms: None` means the failing credential had no expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AuthFailure {
    expires_at_ms: Option<u64>,
}

#[derive(Default)]
struct RequestGateState {
    auth_failure: Option<AuthFailure>,
    rate_limited_until: Option<u64>,
}

fn request_gate_state() -> &'static Mutex<RequestGateState> {
    &default_state().request_gate_state
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Debug, PartialEq, Eq)]
enum QuotaRequestGate {
    Allow,
    AuthBlocked,
    RateLimited,
}

/// Decides whether a quota HTTP request may be made without inspecting a
/// credential or making any network call. The production path supplies the
/// injectable wall clock value from `now_epoch_ms`.
fn quota_request_gate(
    now_ms: u64,
    expires_at_ms: Option<u64>,
    auth_failure: Option<AuthFailure>,
    rate_limited_until: Option<u64>,
    signed_out: bool,
) -> QuotaRequestGate {
    if signed_out
        || auth_failure.is_some_and(|failure| failure.expires_at_ms == expires_at_ms)
        || expires_at_ms
            .is_some_and(|expires| expires <= now_ms.saturating_add(EXPIRY_SAFETY_WINDOW_MS))
    {
        return QuotaRequestGate::AuthBlocked;
    }
    if rate_limited_until.is_some_and(|until| now_ms < until) {
        return QuotaRequestGate::RateLimited;
    }
    QuotaRequestGate::Allow
}

fn retry_after_secs(value: Option<&reqwest::header::HeaderValue>) -> u64 {
    value
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_RETRY_AFTER_SECS)
}

fn rate_limited_until(now_ms: u64, retry_after_secs: u64) -> u64 {
    now_ms.saturating_add(retry_after_secs.saturating_mul(1_000))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ClaudeLoginRefreshResult {
    Refreshed,
    Unchanged,
    Failed,
    SignedOut,
    ProfileUnavailable,
}

/// The frontend only needs to know whether the local Claude Code credential
/// differs from the credential that most recently failed authentication.
/// Deliberately keep the credential itself out of this IPC projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeCredentialChanged {
    pub changed: bool,
}

pub(crate) fn parse_auth_status_logged_in(stdout: &[u8]) -> Option<bool> {
    serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()?
        .get("loggedIn")?
        .as_bool()
}

fn signed_out_outcome(
    refresh: ClaudeLoginRefreshResult,
    auth_status: impl FnOnce() -> Option<bool>,
) -> ClaudeLoginRefreshResult {
    if refresh == ClaudeLoginRefreshResult::Unchanged && auth_status() == Some(false) {
        ClaudeLoginRefreshResult::SignedOut
    } else {
        refresh
    }
}

fn login_blocked_message(
    signed_out_mark: Option<Option<u64>>,
    current_expires_at_ms: Option<u64>,
) -> &'static str {
    if signed_out_mark == Some(current_expires_at_ms) {
        CLAUDE_SIGNED_OUT_MESSAGE
    } else {
        CLAUDE_AUTH_RELOGIN_MESSAGE
    }
}

fn login_blocked_message_for(current_expires_at_ms: Option<u64>) -> &'static str {
    clear_signed_out_mark_if_changed(current_expires_at_ms);
    let mark = signed_out_mark().lock().map(|mark| *mark).unwrap_or(None);
    login_blocked_message(mark, current_expires_at_ms)
}

fn clear_signed_out_mark_if_changed(current_expires_at_ms: Option<u64>) {
    let Ok(mut mark) = signed_out_mark().lock() else {
        return;
    };
    if mark
        .as_ref()
        .is_some_and(|saved| *saved != current_expires_at_ms)
    {
        *mark = None;
    }
}

fn signed_out_mark_matches(current_expires_at_ms: Option<u64>) -> bool {
    let Ok(mut mark) = signed_out_mark().lock() else {
        return false;
    };
    match *mark {
        Some(saved) if saved == current_expires_at_ms => true,
        Some(_) => {
            *mark = None;
            false
        }
        None => false,
    }
}

fn login_refresh_result(
    before_expires_at_ms: Option<u64>,
    after_expires_at_ms: Option<u64>,
    now_ms: u64,
) -> ClaudeLoginRefreshResult {
    if after_expires_at_ms
        .is_some_and(|after| after > before_expires_at_ms.unwrap_or(0) && after > now_ms)
    {
        ClaudeLoginRefreshResult::Refreshed
    } else {
        ClaudeLoginRefreshResult::Unchanged
    }
}

pub(crate) fn auto_renew_due(
    now_ms: u64,
    last_attempt_ms: Option<u64>,
    env_token_present: bool,
) -> bool {
    !env_token_present
        && last_attempt_ms.is_none_or(|last| now_ms.saturating_sub(last) >= AUTO_RENEW_INTERVAL_MS)
}

pub(crate) fn manual_renew_due(
    now_ms: u64,
    last_attempt_ms: Option<u64>,
    env_token_present: bool,
) -> bool {
    !env_token_present
        && last_attempt_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= MANUAL_RENEW_INTERVAL_MS)
}

fn quota_is_login_expired(data: &QuotaData) -> bool {
    data.error.as_deref() == Some(CLAUDE_AUTH_RELOGIN_MESSAGE)
}

type QuotaFuture = Pin<Box<dyn Future<Output = QuotaData> + Send>>;
type SignedOutProbeFuture = Pin<Box<dyn Future<Output = ClaudeLoginRefreshResult> + Send>>;
type CredentialExpiryFuture = Pin<Box<dyn Future<Output = Result<Option<u64>, ()>> + Send>>;
type BoolFuture = Pin<Box<dyn Future<Output = bool> + Send>>;
type SignedOutNowFuture = Pin<Box<dyn Future<Output = Result<bool, ()>> + Send>>;

async fn fetch_quota_with_signed_out_probe_core<F, P, D>(
    mut fetch: F,
    mut probe: P,
    due: D,
) -> QuotaData
where
    F: FnMut() -> QuotaFuture,
    P: FnMut() -> SignedOutProbeFuture,
    D: FnOnce() -> bool,
{
    let mut first = fetch().await;
    if quota_is_login_expired(&first)
        && due()
        && probe().await == ClaudeLoginRefreshResult::SignedOut
    {
        first.error = Some(CLAUDE_SIGNED_OUT_MESSAGE.to_string());
        first.stale_reason = Some(ClaudeStaleReason::SignedOut);
    }
    first
}

fn read_oauth_token_from_env() -> Option<String> {
    std::env::var(CLAUDE_TOKEN_ENV_KEY)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

struct KeychainCredentials {
    access_token: String,
    expires_at_ms: Option<u64>,
    cred_name: String,
    identity: Option<Identity>,
}

#[cfg(target_os = "macos")]
fn read_credentials_from_system() -> Result<KeychainCredentials, String> {
    read_credentials_for_system(None)
}

#[cfg(target_os = "macos")]
fn read_credentials_for_system(config_dir: Option<&str>) -> Result<KeychainCredentials, String> {
    let username = std::env::var("USER").unwrap_or_default();
    if config_dir.is_some() && username.is_empty() {
        return Err("OAuth token not found".into());
    }
    read_credentials_with(config_dir, |cred_name| {
        let mut args = vec!["find-generic-password"];
        if !username.is_empty() {
            args.extend(["-a", &username]);
        }
        args.extend(["-s", cred_name, "-w"]);
        let output = Command::new("security").args(&args).output().ok()?;
        output.status.success().then_some(output.stdout)
    })
}

fn read_credentials_with(
    config_dir: Option<&str>,
    mut read: impl FnMut(&str) -> Option<Vec<u8>>,
) -> Result<KeychainCredentials, String> {
    let names: Vec<String> = match config_dir {
        Some(dir) => vec![crate::services::claude_profiles::credential_service_name(
            dir,
        )],
        None => CREDENTIAL_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
    };
    for cred_name in names {
        if let Some(bytes) = read(&cred_name) {
            let creds_json = String::from_utf8_lossy(&bytes).trim().to_string();
            if creds_json.is_empty() {
                continue;
            }

            if let Ok(creds) = serde_json::from_str::<serde_json::Value>(&creds_json) {
                let oauth = &creds["claudeAiOauth"];
                if let Some(access_token) = oauth["accessToken"].as_str() {
                    if access_token.is_empty() && config_dir.is_some() {
                        continue;
                    }
                    let expires_at_ms = oauth["expiresAt"].as_u64();
                    let token_account = oauth.get("tokenAccount");
                    static LOG_TOKEN_ACCOUNT: OnceLock<()> = OnceLock::new();
                    LOG_TOKEN_ACCOUNT.get_or_init(|| {
                        log_msg(&format!(
                            "[OAuth] tokenAccount present: {}",
                            token_account.is_some()
                        ));
                    });
                    let identity = token_account.and_then(|account| {
                        let uuid = account.get("uuid")?.as_str()?;
                        uuid_tail(uuid)?;
                        Some(Identity {
                            uuid: uuid.to_string(),
                            email: account
                                .get("emailAddress")
                                .and_then(|email| email.as_str())
                                .map(str::to_owned),
                            source: crate::services::quota_history::AccountSource::Credential,
                        })
                    });
                    return Ok(KeychainCredentials {
                        access_token: access_token.to_string(),
                        expires_at_ms,
                        cred_name,
                        identity,
                    });
                }
            }
        }
    }

    Err(format!(
        "OAuth token not found. Please login to Claude Code or set {CLAUDE_TOKEN_ENV_KEY}."
    ))
}

#[cfg(not(target_os = "macos"))]
fn read_credentials_from_system() -> Result<KeychainCredentials, String> {
    Err(format!(
        "OAuth token not configured for this OS. Set {CLAUDE_TOKEN_ENV_KEY}."
    ))
}

#[cfg(not(target_os = "macos"))]
fn read_credentials_for_system(_config_dir: Option<&str>) -> Result<KeychainCredentials, String> {
    read_credentials_from_system()
}

struct RedactedCredential<'a> {
    _secret: &'a str,
}

impl<'a> RedactedCredential<'a> {
    fn new(secret: &'a str) -> Self {
        Self { _secret: secret }
    }
}

impl std::fmt::Display for RedactedCredential<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

fn oauth_cache_hit_diagnostic(
    access_token: &str,
    elapsed: Duration,
    expires_at_ms: Option<u64>,
) -> String {
    format!(
        "[OAuth] cache hit, credential={}, age={:.0}s, ttl={:.0}s remaining, expires_at={expires_at_ms:?}",
        RedactedCredential::new(access_token),
        elapsed.as_secs_f64(),
        (TOKEN_CACHE_TTL - elapsed).as_secs_f64()
    )
}

fn oauth_env_source_diagnostic(access_token: &str) -> String {
    format!(
        "[OAuth] using env var credentials: credential={}",
        RedactedCredential::new(access_token)
    )
}

fn oauth_keychain_source_diagnostic(
    cred_name: &str,
    access_token: &str,
    expires_at_ms: Option<u64>,
) -> String {
    format!(
        "[OAuth] keychain read ok: cred_name={cred_name}, credential={}, expires_at={expires_at_ms:?}",
        RedactedCredential::new(access_token)
    )
}

fn request_quota_diagnostic(access_token: &str, count: u64, gap: Option<f64>) -> String {
    format!(
        "[API] request_quota: credential={}, req_count={count}, gap={:.1}s",
        RedactedCredential::new(access_token),
        gap.unwrap_or(0.0)
    )
}

fn get_oauth_credentials(force_refresh: bool) -> Result<CachedCredentials, String> {
    log_msg(&format!(
        "[OAuth] get_oauth_token called, force_refresh={force_refresh}"
    ));

    if !force_refresh {
        if let Ok(guard) = credentials_cache().lock() {
            if let Some(creds) = guard.as_ref() {
                let elapsed = creds.cached_at.elapsed();
                if elapsed < TOKEN_CACHE_TTL {
                    log_msg(&oauth_cache_hit_diagnostic(
                        &creds.access_token,
                        elapsed,
                        creds.expires_at_ms,
                    ));
                    return Ok(creds.clone());
                } else {
                    log_msg(&format!(
                        "[OAuth] cache expired, age={:.0}s > ttl={:.0}s, re-reading credentials",
                        elapsed.as_secs_f64(),
                        TOKEN_CACHE_TTL.as_secs_f64()
                    ));
                }
            } else {
                log_msg("[OAuth] cache empty, first-time read");
            }
        }
    }

    if let Some(token) = read_oauth_token_from_env() {
        log_msg(&oauth_env_source_diagnostic(&token));
        if let Ok(mut guard) = credentials_cache().lock() {
            *guard = Some(CachedCredentials {
                access_token: token.clone(),
                cached_at: Instant::now(),
                expires_at_ms: None,
                service: "env".into(),
                identity: None,
            });
        }
        return Ok(CachedCredentials {
            access_token: token,
            cached_at: Instant::now(),
            expires_at_ms: None,
            service: "env".into(),
            identity: None,
        });
    }

    log_msg("[OAuth] reading from keychain...");
    let keychain = read_credentials_from_system()?;
    log_msg(&oauth_keychain_source_diagnostic(
        &keychain.cred_name,
        &keychain.access_token,
        keychain.expires_at_ms,
    ));

    if let Ok(mut guard) = credentials_cache().lock() {
        *guard = Some(CachedCredentials {
            access_token: keychain.access_token.clone(),
            cached_at: Instant::now(),
            expires_at_ms: keychain.expires_at_ms,
            service: keychain.cred_name.clone(),
            identity: keychain.identity.clone(),
        });
    }
    Ok(CachedCredentials {
        access_token: keychain.access_token,
        cached_at: Instant::now(),
        expires_at_ms: keychain.expires_at_ms,
        service: keychain.cred_name,
        identity: keychain.identity,
    })
}

async fn request_quota(access_token: &str) -> Result<reqwest::Response, String> {
    request_quota_for(access_token, default_state()).await
}

async fn request_quota_for(
    access_token: &str,
    state: &ProfileState,
) -> Result<reqwest::Response, String> {
    let (count, gap) = if std::ptr::eq(state, default_state().as_ref()) {
        track_request()
    } else {
        let count = REQUEST_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        let gap = state.last_request_time.lock().ok().and_then(|mut value| {
            let gap = value.map(|time| time.elapsed().as_secs_f64());
            *value = Some(Instant::now());
            gap
        });
        (count, gap)
    };
    log_msg(&request_quota_diagnostic(access_token, count, gap));

    let start = Instant::now();
    let client = shared_http_client().map_err(|err| {
        log_msg(&format!("[API] request_quota: network error: {err}"));
        format!("Network error: {err}")
    })?;
    let response = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", "claude-code/1.0.0")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|err| {
            log_msg(&format!("[API] request_quota: network error: {err}"));
            format!("Network error: {err}")
        })?;

    let elapsed = start.elapsed();
    let status = response.status();
    log_msg(&format!(
        "[API] request_quota: status={status}, latency={:.1}s",
        elapsed.as_secs_f64()
    ));
    log_response_headers(&response);

    Ok(response)
}

fn is_auth_error(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN
}

fn parse_quota_window(value: &serde_json::Value) -> Option<UsageInfo> {
    if value.is_null() || !value.is_object() {
        return None;
    }

    let utilization = value.get("utilization")?.as_f64()?;
    let resets_at = value["resets_at"].as_str().map(ToString::to_string);

    Some(UsageInfo {
        used: utilization,
        limit: 100.0,
        percentage: utilization,
        reset_time: resets_at,
    })
}

fn parse_first_quota_window(data: &serde_json::Value, keys: &[&str]) -> Option<UsageInfo> {
    keys.iter().find_map(|key| parse_quota_window(&data[*key]))
}

fn parse_weekly_scoped_model_quota(
    data: &serde_json::Value,
    model_display_name: &str,
) -> Option<UsageInfo> {
    let limits = data.get("limits")?.as_array()?;
    limits.iter().find_map(|limit| {
        if limit.get("group")?.as_str()? != "weekly" {
            return None;
        }
        if limit.get("kind")?.as_str()? != "weekly_scoped" {
            return None;
        }
        let display_name = limit
            .get("scope")?
            .get("model")?
            .get("display_name")?
            .as_str()?;
        if !display_name.eq_ignore_ascii_case(model_display_name) {
            return None;
        }

        let percent = limit.get("percent")?.as_f64()?;
        let resets_at = limit["resets_at"].as_str().map(ToString::to_string);
        Some(UsageInfo {
            used: percent,
            limit: 100.0,
            percentage: percent,
            reset_time: resets_at,
        })
    })
}

fn get_cached_quota() -> Option<QuotaData> {
    let guard = quota_cache().lock().ok()?;
    let cached = guard.as_ref()?;
    let age = cached.cached_at.elapsed();
    if age < QUOTA_CACHE_TTL {
        log_msg(&format!(
            "[Quota] response cache hit, age={:.0}s, ttl={:.0}s remaining",
            age.as_secs_f64(),
            (QUOTA_CACHE_TTL - age).as_secs_f64()
        ));
        Some(cached.data.clone())
    } else {
        log_msg(&format!(
            "[Quota] response cache expired, age={:.0}s",
            age.as_secs_f64()
        ));
        None
    }
}

fn stale_quota_usable(connected: bool, age: Duration) -> bool {
    connected && age < MAX_STALE_QUOTA_AGE
}

fn get_stale_cached_quota() -> Option<QuotaData> {
    let guard = quota_cache().lock().ok()?;
    let cached = guard.as_ref()?;
    let age = cached.cached_at.elapsed();
    if stale_quota_usable(cached.data.connected, age) {
        log_msg(&format!(
            "[Quota] returning stale cache as fallback, age={:.0}s",
            age.as_secs_f64()
        ));
        Some(cached.data.clone())
    } else {
        if cached.data.connected {
            log_msg(&format!(
                "[Quota] stale cache too old to use as fallback, age={:.0}s",
                age.as_secs_f64()
            ));
        }
        None
    }
}

fn mark_quota_fetch_error(mut data: QuotaData, error: String) -> QuotaData {
    data.error = Some(error);
    data
}

fn stale_or_disconnected(error: String) -> QuotaData {
    if let Some(stale) = get_stale_cached_quota() {
        return mark_quota_fetch_error(stale, error);
    }
    QuotaData::disconnected(error)
}

fn stale_login_quota(reason: ClaudeStaleReason, error: String) -> QuotaData {
    let last = last_successful_quota()
        .lock()
        .ok()
        .and_then(|quota| quota.clone());
    stale_login_quota_from_last(last, reason, error)
}

fn stale_login_quota_from_last(
    last: Option<LastSuccessfulQuota>,
    reason: ClaudeStaleReason,
    error: String,
) -> QuotaData {
    let Some(last) = last else {
        return QuotaData::disconnected(error);
    };
    let mut data = last.data;
    data.stale_since = Some(last.recorded_at);
    data.stale_reason = Some(reason);
    data.error = Some(error);
    data
}

fn save_quota_cache(data: &QuotaData) {
    if let Ok(mut guard) = quota_cache().lock() {
        *guard = Some(CachedQuota {
            data: data.clone(),
            cached_at: Instant::now(),
        });
    }
    if data.connected && data.error.is_none() {
        if let Ok(mut guard) = last_successful_quota().lock() {
            *guard = Some(LastSuccessfulQuota {
                data: data.clone(),
                recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            });
        }
    }
}

fn is_rate_limited(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// On transient OS errors (EMFILE / EAGAIN), return the last successful
/// QuotaData instead of surfacing the error to the UI.
fn fallback_or_disconnected(error: String) -> QuotaData {
    if is_transient_os_error(&error) {
        return stale_or_disconnected(error);
    }
    QuotaData::disconnected(error)
}

fn gate_error(gate: QuotaRequestGate, expires_at_ms: Option<u64>) -> QuotaData {
    match gate {
        QuotaRequestGate::AuthBlocked => {
            remember_auth_failure(expires_at_ms);
            let message = login_blocked_message_for(expires_at_ms);
            let reason = if message == CLAUDE_SIGNED_OUT_MESSAGE {
                ClaudeStaleReason::SignedOut
            } else {
                ClaudeStaleReason::Expired
            };
            stale_login_quota(reason, message.to_string())
        }
        QuotaRequestGate::RateLimited => {
            stale_or_disconnected("API error: 429 Too Many Requests".to_string())
        }
        QuotaRequestGate::Allow => unreachable!("allow does not have a gate error"),
    }
}

fn stale_login_or_disconnected(reason: ClaudeStaleReason, error: String) -> QuotaData {
    stale_login_quota(reason, error)
}

fn with_auto_renew_status(mut quota: QuotaData, status: ClaudeAutoRenewStatus) -> QuotaData {
    quota.auto_renew = Some(status);
    quota
}

fn credential_error_or_disconnected(error: String) -> QuotaData {
    if error.starts_with("OAuth token not found") {
        remember_auth_failure(None);
        log_msg("[Auth] credentials missing; treating Claude as signed out");
        return stale_login_quota(
            ClaudeStaleReason::SignedOut,
            CLAUDE_SIGNED_OUT_MESSAGE.to_string(),
        );
    }
    fallback_or_disconnected(error)
}

/// Production auto-renewal orchestration. All side effects are injected so
/// tests exercise this exact sequence without reading credentials or starting
/// a process. The flight is acquired before history is read and is held until
/// the attempted send is recorded, making the once-per-expiry rule atomic with
/// the manual Ping path.
async fn auto_renew_expired_quota_core<C, S, P, R, F, I, L>(
    quota: QuotaData,
    enabled: bool,
    now_secs: i64,
    now_ms: u64,
    history: &Mutex<Option<AutoPingAttempt>>,
    stale: L,
    read_credentials: C,
    signed_out_now: S,
    other_claude_running: P,
    ping: R,
    refetch: F,
    invalidate: I,
) -> QuotaData
where
    C: FnOnce() -> CredentialExpiryFuture,
    S: FnOnce() -> SignedOutNowFuture,
    P: FnOnce() -> BoolFuture,
    R: FnOnce() -> BoolFuture,
    F: FnOnce() -> QuotaFuture,
    I: FnOnce(),
    L: Fn(ClaudeStaleReason, String) -> QuotaData,
{
    if quota.stale_reason != Some(ClaudeStaleReason::Expired) {
        return quota;
    }
    if !enabled {
        log_msg("[Auth] auto-renew skipped: setting off");
        return with_auto_renew_status(quota, ClaudeAutoRenewStatus::Skipped);
    }

    let expires_at_ms = match read_credentials().await {
        Ok(expires_at_ms) => expires_at_ms,
        Err(()) => {
            log_msg("[Auth] auto-renew skipped: credential state unavailable");
            return stale(
                ClaudeStaleReason::SignedOut,
                CLAUDE_SIGNED_OUT_MESSAGE.to_string(),
            );
        }
    };
    // This probe deliberately bypasses the ten-minute refresh throttle. An
    // unavailable result is treated as signed out, because sending a Ping is
    // less safe than declining to renew.
    if signed_out_now().await.unwrap_or(true) {
        log_msg("[Auth] auto-renew skipped: signed out or login state unavailable");
        return stale(
            ClaudeStaleReason::SignedOut,
            CLAUDE_SIGNED_OUT_MESSAGE.to_string(),
        );
    }

    let Some(_flight) = crate::services::window_ping::ClaudeFlight::acquire() else {
        log_msg("[Auth] auto-renew skipped: ping busy");
        return with_auto_renew_status(quota, ClaudeAutoRenewStatus::Skipped);
    };
    let other_claude_running = other_claude_running().await;
    let last_reset_time = quota
        .session
        .as_ref()
        .and_then(|session| session.reset_time.as_deref());
    let should_send = {
        let mut history = match history.lock() {
            Ok(history) => history,
            Err(_) => return with_auto_renew_status(quota, ClaudeAutoRenewStatus::Skipped),
        };
        let (last_expires_at_ms, last_attempt_ms) = match *history {
            Some(attempt) => (Some(attempt.expires_at_ms), Some(attempt.attempted_at_ms)),
            None => (None, None),
        };
        let decision = crate::services::window_ping::auto_ping_decision(
            crate::services::window_ping::AutoPingInput {
                enabled,
                expired: true,
                signed_out: false,
                last_reset_time,
                now_secs,
                other_claude_running,
                expires_at_ms,
                last_expires_at_ms,
                last_attempt_ms,
                now_ms,
            },
        );
        if decision != crate::services::window_ping::AutoPingDecision::Send {
            log_msg(&format!("[Auth] auto-renew skipped: {decision:?}"));
        }
        if decision == crate::services::window_ping::AutoPingDecision::Send {
            *history = Some(AutoPingAttempt {
                expires_at_ms,
                attempted_at_ms: now_ms,
            });
            true
        } else {
            false
        }
    };
    if !should_send {
        return with_auto_renew_status(quota, ClaudeAutoRenewStatus::Skipped);
    }

    if !ping().await {
        log_msg("[Auth] auto-renew ping failed");
        return with_auto_renew_status(quota, ClaudeAutoRenewStatus::Skipped);
    }
    log_msg("[Auth] auto-renew sent; reloading Claude quota");
    invalidate();
    let refetched = refetch().await;
    if refetched.connected && refetched.error.is_none() {
        return refetched;
    }
    let error = refetched
        .error
        .unwrap_or_else(|| "Claude quota reload failed after renewal".to_string());
    with_auto_renew_status(
        stale(ClaudeStaleReason::Expired, error),
        ClaudeAutoRenewStatus::Sent,
    )
}

fn gate_snapshot() -> (Option<AuthFailure>, Option<u64>) {
    request_gate_state()
        .lock()
        .map(|state| (state.auth_failure, state.rate_limited_until))
        .unwrap_or((None, None))
}

fn clear_auth_gate() {
    if let Ok(mut state) = request_gate_state().lock() {
        state.auth_failure = None;
    }
}

fn clear_auth_gate_if_refreshed(expires_at_ms: Option<u64>, failure: Option<AuthFailure>) {
    if failure.is_some_and(|failure| failure.expires_at_ms != expires_at_ms) {
        clear_auth_gate();
    }
}

fn remember_auth_failure(expires_at_ms: Option<u64>) {
    if let Ok(mut state) = request_gate_state().lock() {
        state.auth_failure = Some(AuthFailure { expires_at_ms });
    }
}

/// Whether `fetch_quota` must read credentials and consult the gate before
/// anything else (including the response cache). Reading credentials is local;
/// it lets an expired login outrank an active 429 window.
fn request_gate_active(
    now_ms: u64,
    auth_failure: Option<AuthFailure>,
    rate_limited_until: Option<u64>,
    signed_out_mark_present: bool,
) -> bool {
    signed_out_mark_present
        || auth_failure.is_some()
        || rate_limited_until.is_some_and(|until| now_ms < until)
}

fn remember_rate_limit(retry_after: u64) {
    if let Ok(mut state) = request_gate_state().lock() {
        state.rate_limited_until = Some(rate_limited_until(now_epoch_ms(), retry_after));
    }
}

fn read_credentials(force_refresh: bool) -> Result<CachedCredentials, String> {
    let credentials = get_oauth_credentials(force_refresh)?;
    clear_signed_out_mark_if_changed(credentials.expires_at_ms);
    Ok(credentials)
}

fn credential_changed_since_auth_failure(
    auth_failure: Option<AuthFailure>,
    current_expiry: Result<Option<u64>, ()>,
) -> ClaudeCredentialChanged {
    ClaudeCredentialChanged {
        changed: auth_failure.is_some_and(|failure| {
            current_expiry
                .ok()
                .is_some_and(|expiry| expiry != failure.expires_at_ms)
        }),
    }
}

/// Reads only the expiration metadata from the Claude Code keychain entry.
/// This deliberately bypasses the environment-token path because an external
/// Claude Code renewal changes the keychain credential, not that override.
pub(crate) async fn claude_credential_changed() -> ClaudeCredentialChanged {
    let auth_failure = gate_snapshot().0;
    let current_expiry = match tauri::async_runtime::spawn_blocking(|| {
        read_credentials_from_system()
            .map(|credentials| credentials.expires_at_ms)
            .map_err(|_| ())
    })
    .await
    {
        Ok(expiry) => expiry,
        Err(_) => Err(()),
    };
    credential_changed_since_auth_failure(auth_failure, current_expiry)
}

fn clear_credentials_cache() {
    if let Ok(mut guard) = credentials_cache().lock() {
        *guard = None;
    }
}

/// Returns whether a forced Claude Ping may renew a locally-expired login.
/// This only consults the local credential source and the auth gate; it never
/// sends a quota request.
pub(crate) async fn login_renewal_needed() -> bool {
    // An explicit CLAUDE_CODE_OAUTH_TOKEN outranks the keychain, so renewing the
    // Claude Code login could not change what QuotaBar reads.
    if read_oauth_token_from_env().is_some() {
        return false;
    }
    match tauri::async_runtime::spawn_blocking(|| read_credentials(true)).await {
        Ok(Ok(credentials)) => {
            if signed_out_mark_matches(credentials.expires_at_ms) {
                return false;
            }
            let (auth_failure, _) = gate_snapshot();
            if auth_failure.is_some() {
                return true;
            }
            quota_request_gate(
                now_epoch_ms(),
                credentials.expires_at_ms,
                None,
                None,
                signed_out_mark_matches(credentials.expires_at_ms),
            ) == QuotaRequestGate::AuthBlocked
        }
        _ => false,
    }
}

/// Makes the next quota read use the credentials written by Claude Code after
/// a successful renewal Ping.
pub(crate) fn invalidate_login_state() {
    clear_credentials_cache();
    clear_auth_gate();
    // The fallback for a failed reload is `last_successful_quota`, so the
    // fresh-response cache can be dropped to force a real request.
    if let Ok(mut guard) = quota_cache().lock() {
        *guard = None;
    }
}

/// Only a parsed `loggedIn: true` proves the user is still signed in. A failed,
/// timed-out or unparseable `auth status` counts as signed out for auto-renew.
fn signed_out_from_auth_status(status: Result<Option<bool>, &'static str>) -> Result<bool, ()> {
    match status {
        Ok(Some(logged_in)) => Ok(!logged_in),
        _ => Err(()),
    }
}

fn login_is_blocked(expires_at_ms: Option<u64>, now_ms: u64) -> bool {
    let (auth_failure, _) = gate_snapshot();
    quota_request_gate(
        now_ms,
        expires_at_ms,
        auth_failure,
        None,
        signed_out_mark_matches(expires_at_ms),
    ) == QuotaRequestGate::AuthBlocked
}

fn renew_attempt_due(attempts: &'static Mutex<Option<u64>>, now_ms: u64, interval_ms: u64) -> bool {
    let Ok(mut last_attempt) = attempts.lock() else {
        return false;
    };
    if last_attempt.is_some_and(|last| now_ms.saturating_sub(last) < interval_ms) {
        return false;
    }
    *last_attempt = Some(now_ms);
    true
}

/// Runs `claude auth status --json` and keeps only its `loggedIn` boolean.
/// The caller must already hold the `ClaudeFlight`.
async fn auth_status_logged_in() -> Result<Option<bool>, &'static str> {
    let command = match crate::services::window_ping::build_claude_auth_status_command(None) {
        Ok(command) => command,
        Err(crate::services::window_ping::PingOutcome::CliNotFound { .. }) => {
            return Err("cli_not_found");
        }
        Err(_) => return Err("failed"),
    };
    Ok(
        match crate::services::window_ping::run_claude_auth_status_without_blocking(
            command,
            Duration::from_secs(10),
        )
        .await
        {
            crate::services::window_ping::ChildResult::Success(stdout) => {
                parse_auth_status_logged_in(&stdout)
            }
            _ => None,
        },
    )
}

fn log_signed_out_probe(result: &str, started: Instant) {
    log_msg(&format!(
        "[Auth] signed-out probe: result={result} secs={:.1}",
        started.elapsed().as_secs_f64()
    ));
}

async fn probe_claude_signed_out() -> ClaudeLoginRefreshResult {
    let started = Instant::now();
    let Some(_flight) = crate::services::window_ping::ClaudeFlight::acquire() else {
        log_signed_out_probe("skipped_busy", started);
        return ClaudeLoginRefreshResult::Failed;
    };
    let expires_at_ms = match tauri::async_runtime::spawn_blocking(|| read_credentials(false)).await
    {
        Ok(Ok(credentials)) => credentials.expires_at_ms,
        _ => {
            log_signed_out_probe("failed", started);
            return ClaudeLoginRefreshResult::Failed;
        }
    };
    let auth_status = match auth_status_logged_in().await {
        Ok(auth_status) => auth_status,
        Err("cli_not_found") => {
            log_signed_out_probe("cli_not_found", started);
            return ClaudeLoginRefreshResult::Failed;
        }
        Err(_) => {
            log_signed_out_probe("failed", started);
            return ClaudeLoginRefreshResult::Failed;
        }
    };
    let result = signed_out_outcome(ClaudeLoginRefreshResult::Unchanged, || auth_status);
    if result == ClaudeLoginRefreshResult::SignedOut {
        if let Ok(mut mark) = signed_out_mark().lock() {
            *mark = Some(expires_at_ms);
        }
        log_signed_out_probe("signed_out", started);
    } else {
        log_signed_out_probe("unchanged", started);
    }
    result
}

async fn refresh_claude_login_core<B, A, M, L, D, P>(
    mut read_before: B,
    mut read_after: A,
    now_ms: u64,
    signed_out_mark_matches: M,
    login_is_blocked: L,
    probe_due: D,
    mut probe: P,
) -> ClaudeLoginRefreshResult
where
    B: FnMut() -> CredentialExpiryFuture,
    A: FnMut() -> CredentialExpiryFuture,
    M: FnOnce(Option<u64>) -> bool,
    L: FnOnce(Option<u64>, u64) -> bool,
    D: FnOnce() -> bool,
    P: FnMut() -> SignedOutProbeFuture,
{
    let before = match read_before().await {
        Ok(expires_at_ms) => expires_at_ms,
        Err(()) => return ClaudeLoginRefreshResult::Failed,
    };
    let after = match read_after().await {
        Ok(expires_at_ms) => expires_at_ms,
        Err(()) => return ClaudeLoginRefreshResult::Failed,
    };
    let result = login_refresh_result(before, after, now_ms);
    if result == ClaudeLoginRefreshResult::Refreshed {
        return result;
    }
    if signed_out_mark_matches(after) {
        return ClaudeLoginRefreshResult::SignedOut;
    }
    if !login_is_blocked(after, now_ms) {
        return result;
    }
    if !probe_due() {
        log_signed_out_probe("skipped_throttle", Instant::now());
        return result;
    }
    probe().await
}

pub async fn refresh_claude_login() -> ClaudeLoginRefreshResult {
    let now_ms = now_epoch_ms();
    let result = refresh_claude_login_core(
        || {
            Box::pin(async {
                tauri::async_runtime::spawn_blocking(|| read_credentials(false))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .map(|credentials| credentials.expires_at_ms)
                    .ok_or(())
            })
        },
        || {
            clear_credentials_cache();
            Box::pin(async {
                tauri::async_runtime::spawn_blocking(|| read_credentials(true))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .map(|credentials| credentials.expires_at_ms)
                    .ok_or(())
            })
        },
        now_ms,
        signed_out_mark_matches,
        login_is_blocked,
        || {
            let due = manual_renew_due(
                now_ms,
                last_manual_renew_attempt_ms()
                    .lock()
                    .ok()
                    .and_then(|attempt| *attempt),
                read_oauth_token_from_env().is_some(),
            );
            due && renew_attempt_due(
                last_manual_renew_attempt_ms(),
                now_ms,
                MANUAL_RENEW_INTERVAL_MS,
            )
        },
        || Box::pin(probe_claude_signed_out()),
    )
    .await;
    if result == ClaudeLoginRefreshResult::Refreshed {
        clear_auth_gate();
        if let Ok(mut mark) = signed_out_mark().lock() {
            *mark = None;
        }
    }
    result
}

pub async fn fetch_quota() -> QuotaData {
    let now_ms = now_epoch_ms();
    let env_token_present = read_oauth_token_from_env().is_some();
    let quota = fetch_quota_with_signed_out_probe_core(
        || Box::pin(fetch_quota_once()),
        || Box::pin(probe_claude_signed_out()),
        || {
            auto_renew_due(
                now_ms,
                last_auto_renew_attempt_ms()
                    .lock()
                    .ok()
                    .and_then(|attempt| *attempt),
                env_token_present,
            ) && renew_attempt_due(last_auto_renew_attempt_ms(), now_ms, AUTO_RENEW_INTERVAL_MS)
        },
    )
    .await;

    let mut result = auto_renew_expired_quota_core(
        quota,
        CLAUDE_AUTO_RENEW_ENABLED.load(Ordering::Relaxed),
        chrono::Utc::now().timestamp(),
        now_epoch_ms(),
        auto_ping_history(),
        stale_login_quota,
        || {
            Box::pin(async {
                tauri::async_runtime::spawn_blocking(|| read_credentials(false))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .map(|credentials| credentials.expires_at_ms)
                    .ok_or(())
            })
        },
        || Box::pin(async { signed_out_from_auth_status(auth_status_logged_in().await) }),
        || {
            Box::pin(async {
                tauri::async_runtime::spawn_blocking(
                    crate::services::window_ping::other_claude_process_running,
                )
                .await
                .unwrap_or(true)
            })
        },
        || Box::pin(crate::services::window_ping::run_claude_renewal_ping()),
        || Box::pin(fetch_quota_once()),
        invalidate_login_state,
    )
    .await;
    let credentials = credentials_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.clone());
    if result.connected {
        if let Some(credentials) = credentials {
            let identity = resolve_identity(default_state(), &credentials).await;
            if identity
                .as_ref()
                .is_some_and(|identity| cross_check_identity(None, identity))
            {
                return QuotaData::disconnected("Claude account mismatch");
            }
            let tail = identity
                .as_ref()
                .and_then(|identity| uuid_tail(&identity.uuid));
            result.account = identity.as_ref().and_then(|identity| {
                Some(crate::domain::models::ClaudeAccount {
                    tail: uuid_tail(&identity.uuid)?,
                    email: identity.email.clone(),
                })
            });
            let labels = crate::services::claude_profiles::load()
                .map(|registry| registry.labels)
                .unwrap_or_default();
            result.account_label = result.account.as_ref().map(|account| {
                labels
                    .get(&account.tail)
                    .cloned()
                    .or_else(|| account.email.clone())
                    .unwrap_or_else(|| account.tail.clone())
            });
            result.app_account = Some(desktop_account_status(tail.as_deref(), &labels));
        }
    }
    result
}

async fn fetch_quota_once() -> QuotaData {
    log_msg("[Quota] ---- fetch_quota start ----");

    let (auth_failure, rate_limited_until) = gate_snapshot();
    if request_gate_active(
        now_epoch_ms(),
        auth_failure,
        rate_limited_until,
        signed_out_mark()
            .lock()
            .map(|mark| mark.is_some())
            .unwrap_or(false),
    ) {
        // Local credential read first, so an expired login outranks the 429 window.
        let force_refresh = auth_failure.is_some();
        let credentials =
            match tauri::async_runtime::spawn_blocking(move || read_credentials(force_refresh))
                .await
            {
                Ok(Ok(credentials)) => credentials,
                Ok(Err(error)) => return credential_error_or_disconnected(error),
                Err(error) => {
                    return fallback_or_disconnected(format!("OAuth token task failed: {error}"))
                }
            };
        clear_auth_gate_if_refreshed(credentials.expires_at_ms, auth_failure);
        let gate = quota_request_gate(
            now_epoch_ms(),
            credentials.expires_at_ms,
            auth_failure,
            rate_limited_until,
            signed_out_mark_matches(credentials.expires_at_ms),
        );
        if gate != QuotaRequestGate::Allow {
            return gate_error(gate, credentials.expires_at_ms);
        }
    }

    // Return cached response only after active request gates have been checked.
    if let Some(cached) = get_cached_quota() {
        return cached;
    }

    let mut credentials =
        match tauri::async_runtime::spawn_blocking(|| read_credentials(false)).await {
            Ok(Ok(credentials)) => credentials,
            Ok(Err(error)) => {
                log_msg(&format!("[Quota] credential read failed: {error}"));
                return credential_error_or_disconnected(error);
            }
            Err(error) => {
                log_msg(&format!("[Quota] oauth token task failed: {error}"));
                return QuotaData::disconnected(format!("OAuth token task failed: {error}"));
            }
        };

    let (auth_failure, rate_limited_until) = gate_snapshot();
    let gate = quota_request_gate(
        now_epoch_ms(),
        credentials.expires_at_ms,
        auth_failure,
        rate_limited_until,
        signed_out_mark_matches(credentials.expires_at_ms),
    );
    if gate != QuotaRequestGate::Allow {
        return gate_error(gate, credentials.expires_at_ms);
    }

    let mut response = match request_quota(&credentials.access_token).await {
        Ok(resp) => resp,
        Err(error) => {
            log_msg(&format!("[Quota] initial request failed: {error}"));
            return stale_or_disconnected(error);
        }
    };

    let status = response.status();
    log_msg(&format!("[Quota] initial response: status={status}"));

    // 429: return stale cache data if available, but always include error
    // so the frontend can trigger adaptive backoff
    if is_rate_limited(status) {
        remember_rate_limit(retry_after_secs(response.headers().get("retry-after")));
        log_msg("[Quota] 429 rate limited, returning stale cache if available");
        if let Some(stale) = get_stale_cached_quota() {
            return mark_quota_fetch_error(stale, "API error: 429 Too Many Requests".to_string());
        }
        return QuotaData::disconnected("API error: 429 Too Many Requests");
    }

    if is_auth_error(status) {
        log_msg(&format!(
            "[Quota] auth error ({status}), step 1: force re-read from keychain"
        ));
        let fresh_credentials =
            match tauri::async_runtime::spawn_blocking(|| read_credentials(true)).await {
                Ok(Ok(credentials)) => credentials,
                Ok(Err(error)) => {
                    log_msg(&format!("[Quota] keychain re-read failed: {error}"));
                    return credential_error_or_disconnected(error);
                }
                Err(error) => {
                    log_msg(&format!("[Quota] oauth token retry task failed: {error}"));
                    return fallback_or_disconnected(format!("OAuth token task failed: {error}"));
                }
            };

        // The re-read credential passes the same gate before the second request.
        let (auth_failure, rate_limited_until) = gate_snapshot();
        let gate = quota_request_gate(
            now_epoch_ms(),
            fresh_credentials.expires_at_ms,
            auth_failure,
            rate_limited_until,
            signed_out_mark_matches(fresh_credentials.expires_at_ms),
        );
        if gate != QuotaRequestGate::Allow {
            log_msg("[Quota] re-read credential blocked by request gate; skipping retry");
            return gate_error(gate, fresh_credentials.expires_at_ms);
        }

        response = match request_quota(&fresh_credentials.access_token).await {
            Ok(resp) => resp,
            Err(error) => {
                log_msg(&format!(
                    "[Quota] retry with keychain token failed: {error}"
                ));
                return fallback_or_disconnected(error);
            }
        };

        let status2 = response.status();
        log_msg(&format!(
            "[Quota] keychain retry response: status={status2}"
        ));

        if is_rate_limited(status2) {
            remember_rate_limit(retry_after_secs(response.headers().get("retry-after")));
            log_msg("[Quota] 429 after keychain retry, returning stale cache");
            if let Some(stale) = get_stale_cached_quota() {
                return mark_quota_fetch_error(
                    stale,
                    "API error: 429 Too Many Requests".to_string(),
                );
            }
            return QuotaData::disconnected("API error: 429 Too Many Requests");
        }

        if is_auth_error(status2) {
            log_msg(&format!(
                "[Quota] auth error ({status2}) after keychain re-read; stopping until Claude Code login is refreshed"
            ));
            remember_auth_failure(fresh_credentials.expires_at_ms);
            let message = login_blocked_message_for(fresh_credentials.expires_at_ms);
            let reason = if message == CLAUDE_SIGNED_OUT_MESSAGE {
                ClaudeStaleReason::SignedOut
            } else {
                ClaudeStaleReason::Expired
            };
            return stale_login_or_disconnected(reason, message.to_string());
        }
        credentials = fresh_credentials;
    }

    let final_status = response.status();
    if !final_status.is_success() {
        log_msg(&format!("[Quota] non-success response: {final_status}"));
        return stale_or_disconnected(format!("API error: {final_status}"));
    }

    let data = match response.json::<serde_json::Value>().await {
        Ok(data) => data,
        Err(err) => {
            log_msg(&format!("[Quota] parse error: {err}"));
            return QuotaData::disconnected(format!("Failed to parse response: {err}"));
        }
    };

    if data["error"].is_object() {
        let error_msg = data["error"]["message"].as_str().unwrap_or("API error");
        log_msg(&format!("[Quota] API returned error: {error_msg}"));
        return QuotaData::disconnected(format!("{error_msg} (Token may be expired)"));
    }

    let five_hour = data["five_hour"]["utilization"].as_f64();
    let seven_day = data["seven_day"]["utilization"].as_f64();
    let seven_day_design = data["seven_day_omelette"]["utilization"].as_f64();
    let weekly_fable5 = parse_first_quota_window(&data, &FABLE5_QUOTA_KEYS)
        .or_else(|| parse_weekly_scoped_model_quota(&data, "Fable"));
    let seven_day_fable5 = weekly_fable5.as_ref().map(|window| window.percentage);
    log_msg(&format!(
        "[Quota] SUCCESS: five_hour={five_hour:?}%, seven_day={seven_day:?}%, seven_day_omelette={seven_day_design:?}%, seven_day_fable5={seven_day_fable5:?}%"
    ));

    let session = parse_quota_window(&data["five_hour"]);
    let weekly_total = parse_quota_window(&data["seven_day"]);
    let weekly_opus = parse_quota_window(&data["seven_day_opus"]);
    let weekly_sonnet = parse_quota_window(&data["seven_day_sonnet"]);
    let weekly_design = parse_quota_window(&data["seven_day_omelette"]);

    if session.is_none()
        && weekly_total.is_none()
        && weekly_opus.is_none()
        && weekly_sonnet.is_none()
        && weekly_design.is_none()
        && weekly_fable5.is_none()
    {
        log_msg("[Quota] parse error: no numeric quota utilization fields");
        return QuotaData::disconnected(
            "Failed to parse response: no numeric quota utilization fields",
        );
    }

    let mut result = QuotaData::connected(
        session,
        weekly_total,
        weekly_opus,
        weekly_sonnet,
        weekly_design,
        weekly_fable5,
    );

    let identity = resolve_identity(default_state(), &credentials).await;
    let mismatch = identity
        .as_ref()
        .is_some_and(|identity| cross_check_identity(None, identity));
    let tail = identity
        .as_ref()
        .and_then(|identity| uuid_tail(&identity.uuid));
    let labels = crate::services::claude_profiles::load()
        .map(|registry| registry.labels)
        .unwrap_or_default();
    result.app_account = Some(desktop_account_status(
        if mismatch { None } else { tail.as_deref() },
        &labels,
    ));
    if mismatch {
        crate::services::quota_history::record_profile_success(
            &data,
            final_status,
            chrono::Utc::now(),
            "default",
            None,
            None,
        );
        return QuotaData::disconnected("Claude account mismatch");
    }
    result.account = identity.as_ref().and_then(|identity| {
        Some(crate::domain::models::ClaudeAccount {
            tail: uuid_tail(&identity.uuid)?,
            email: identity.email.clone(),
        })
    });
    result.account_label = result.account.as_ref().map(|account| {
        labels
            .get(&account.tail)
            .cloned()
            .or_else(|| account.email.clone())
            .unwrap_or_else(|| account.tail.clone())
    });
    crate::services::quota_history::record_profile_success(
        &data,
        final_status,
        chrono::Utc::now(),
        "default",
        tail.as_deref(),
        identity.as_ref().map(|identity| identity.source),
    );
    save_quota_cache(&result);
    result
}

fn custom_credentials(
    profile: &crate::services::claude_profiles::Profile,
    state: &ProfileState,
    force: bool,
    read: impl FnOnce(Option<&str>) -> Result<KeychainCredentials, String>,
) -> Result<CachedCredentials, String> {
    custom_credentials_with(profile, state, force, read)
}

fn custom_credentials_with(
    profile: &crate::services::claude_profiles::Profile,
    state: &ProfileState,
    force: bool,
    read: impl FnOnce(Option<&str>) -> Result<KeychainCredentials, String>,
) -> Result<CachedCredentials, String> {
    if !force {
        if let Some(cached) = state
            .credentials_cache
            .lock()
            .ok()
            .and_then(|cache| cache.clone())
        {
            if cached.cached_at.elapsed() < TOKEN_CACHE_TTL {
                return Ok(cached);
            }
        }
    }
    let keychain = read(Some(&profile.config_dir))?;
    let credentials = CachedCredentials {
        access_token: keychain.access_token,
        cached_at: Instant::now(),
        expires_at_ms: keychain.expires_at_ms,
        service: keychain.cred_name,
        identity: keychain.identity,
    };
    if let Ok(mut mark) = state.signed_out_mark.lock() {
        if mark
            .as_ref()
            .is_some_and(|saved| *saved != credentials.expires_at_ms)
        {
            *mark = None;
        }
    }
    if let Ok(mut cache) = state.credentials_cache.lock() {
        let changed = cache.as_ref().is_some_and(|old| {
            old.expires_at_ms != credentials.expires_at_ms || old.service != credentials.service
        });
        if changed {
            if let Ok(mut quota) = state.quota_cache.lock() {
                *quota = None;
            }
            if let Ok(mut last) = state.last_successful_quota.lock() {
                *last = None;
            }
            if let Ok(mut gate) = state.request_gate_state.lock() {
                gate.auth_failure = None;
            }
        }
        *cache = Some(credentials.clone());
    }
    Ok(credentials)
}

pub(crate) fn invalidate_profile_login_state(profile: &crate::services::claude_profiles::Profile) {
    let state = profile_state(&profile.config_dir);
    if let Ok(mut value) = state.credentials_cache.lock() {
        *value = None;
    }
    if let Ok(mut value) = state.quota_cache.lock() {
        *value = None;
    }
    if let Ok(mut value) = state.request_gate_state.lock() {
        value.auth_failure = None;
    };
}

fn mark_profile_signed_out(state: &ProfileState, expiry: Option<u64>) {
    if let Ok(mut mark) = state.signed_out_mark.lock() {
        *mark = Some(expiry);
    }
}

fn profile_signed_out_matches(state: &ProfileState, expiry: Option<u64>) -> bool {
    state
        .signed_out_mark
        .lock()
        .ok()
        .is_some_and(|mark| *mark == Some(expiry))
}

pub(crate) async fn profile_login_renewal_needed(
    profile: &crate::services::claude_profiles::Profile,
) -> bool {
    let state = profile_state(&profile.config_dir);
    let input = profile.clone();
    tauri::async_runtime::spawn_blocking(move || {
        custom_credentials(&input, &state, true, read_credentials_for_system)
            .ok()
            .is_some_and(|c| {
                let signed_out = profile_signed_out_matches(&state, c.expires_at_ms);
                let auth_failed = state.request_gate_state.lock().ok().is_some_and(|gate| {
                    gate.auth_failure
                        .is_some_and(|failure| failure.expires_at_ms == c.expires_at_ms)
                });
                !signed_out
                    && (auth_failed
                        || c.expires_at_ms.is_some_and(|expiry| {
                            expiry <= now_epoch_ms().saturating_add(EXPIRY_SAFETY_WINDOW_MS)
                        }))
            })
    })
    .await
    .unwrap_or(false)
}

pub(crate) async fn refresh_profile_login(
    profile: &crate::services::claude_profiles::Profile,
) -> ClaudeLoginRefreshResult {
    let state = profile_state(&profile.config_dir);
    let input_before = profile.clone();
    let before_state = Arc::clone(&state);
    let before = match tauri::async_runtime::spawn_blocking(move || {
        custom_credentials(
            &input_before,
            &before_state,
            false,
            read_credentials_for_system,
        )
    })
    .await
    {
        Ok(Ok(credentials)) => credentials.expires_at_ms,
        _ => return ClaudeLoginRefreshResult::SignedOut,
    };
    let input = profile.clone();
    let refreshed_state = Arc::clone(&state);
    let after = match tauri::async_runtime::spawn_blocking(move || {
        custom_credentials(&input, &refreshed_state, true, read_credentials_for_system)
    })
    .await
    {
        Ok(Ok(credentials)) => credentials.expires_at_ms,
        _ => return ClaudeLoginRefreshResult::SignedOut,
    };
    let result = login_refresh_result(before, after, now_epoch_ms());
    if result == ClaudeLoginRefreshResult::Refreshed {
        invalidate_profile_login_state(profile);
        return result;
    }
    if !after.is_some_and(|expiry| expiry <= now_epoch_ms().saturating_add(EXPIRY_SAFETY_WINDOW_MS))
    {
        return result;
    }
    let now = now_epoch_ms();
    {
        let Ok(mut last) = state.last_manual_renew_attempt_ms.lock() else {
            return result;
        };
        if last.is_some_and(|last| now.saturating_sub(last) < MANUAL_RENEW_INTERVAL_MS) {
            return result;
        }
        *last = Some(now);
    }
    let Some(_flight) = crate::services::window_ping::ClaudeFlight::acquire() else {
        return result;
    };
    if tauri::async_runtime::spawn_blocking(
        crate::services::window_ping::other_claude_process_running,
    )
    .await
    .unwrap_or(true)
    {
        return result;
    }
    let Ok(command) =
        crate::services::window_ping::build_claude_auth_status_command(Some(&profile.config_dir))
    else {
        return result;
    };
    if let crate::services::window_ping::ChildResult::Success(stdout) =
        crate::services::window_ping::run_claude_auth_status_without_blocking(
            command,
            Duration::from_secs(10),
        )
        .await
    {
        if parse_auth_status_logged_in(&stdout) == Some(false) {
            mark_profile_signed_out(&state, after);
            return ClaudeLoginRefreshResult::SignedOut;
        }
    }
    result
}

async fn auto_renew_profile(
    profile: &crate::services::claude_profiles::Profile,
    state: &ProfileState,
    credentials: &CachedCredentials,
) -> bool {
    let sent = auto_renew_profile_with(
        state,
        credentials.expires_at_ms,
        now_epoch_ms(),
        CLAUDE_AUTO_RENEW_ENABLED.load(Ordering::Relaxed),
        || async {
            tauri::async_runtime::spawn_blocking(
                crate::services::window_ping::other_claude_process_running,
            )
            .await
            .unwrap_or(true)
        },
        || async {
            let Ok(command) = crate::services::window_ping::build_claude_auth_status_command(Some(
                &profile.config_dir,
            )) else {
                return false;
            };
            match crate::services::window_ping::run_claude_auth_status_without_blocking(
                command,
                Duration::from_secs(10),
            )
            .await
            {
                crate::services::window_ping::ChildResult::Success(stdout) => {
                    parse_auth_status_logged_in(&stdout) == Some(true)
                }
                _ => false,
            }
        },
        || crate::services::window_ping::run_claude_renewal_ping_for(Some(&profile.config_dir)),
    )
    .await;
    if sent {
        invalidate_profile_login_state(profile);
    }
    sent
}

fn profile_last_successful_quota(state: &ProfileState) -> Option<LastSuccessfulQuota> {
    state
        .last_successful_quota
        .lock()
        .ok()
        .and_then(|last| last.clone())
}

async fn auto_renew_profile_with<P, PF, A, AF, R, RF>(
    state: &ProfileState,
    expires_at_ms: Option<u64>,
    now: u64,
    enabled: bool,
    probe: P,
    auth: A,
    run: R,
) -> bool
where
    P: FnOnce() -> PF,
    PF: Future<Output = bool>,
    A: FnOnce() -> AF,
    AF: Future<Output = bool>,
    R: FnOnce() -> RF,
    RF: Future<Output = bool>,
{
    if !enabled {
        return false;
    }
    {
        let Ok(mut last) = state.last_auto_renew_attempt_ms.lock() else {
            return false;
        };
        if last.is_some_and(|last| now.saturating_sub(last) < AUTO_RENEW_INTERVAL_MS) {
            return false;
        }
        *last = Some(now);
    }
    let Some(_flight) = crate::services::window_ping::ClaudeFlight::acquire() else {
        return false;
    };
    if probe().await {
        return false;
    }
    if !auth().await {
        return false;
    }
    let last_quota = profile_last_successful_quota(state);
    let reset = last_quota
        .as_ref()
        .and_then(|last| last.data.session.as_ref())
        .and_then(|session| session.reset_time.as_deref());
    {
        let Ok(mut history) = state.auto_ping_history.lock() else {
            return false;
        };
        let decision = crate::services::window_ping::auto_ping_decision(
            crate::services::window_ping::AutoPingInput {
                enabled,
                expired: true,
                signed_out: false,
                last_reset_time: reset,
                now_secs: (now / 1_000) as i64,
                other_claude_running: false,
                expires_at_ms,
                last_expires_at_ms: history.as_ref().map(|entry| entry.expires_at_ms),
                last_attempt_ms: history.as_ref().map(|entry| entry.attempted_at_ms),
                now_ms: now,
            },
        );
        if decision != crate::services::window_ping::AutoPingDecision::Send {
            return false;
        }
        *history = Some(AutoPingAttempt {
            expires_at_ms,
            attempted_at_ms: now,
        });
    }
    run().await
}

async fn profile_preflight_with<F, Fut>(
    enabled: bool,
    read: F,
) -> Result<CachedCredentials, &'static str>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<CachedCredentials, ()>>,
{
    if !enabled {
        return Err("disabled");
    }
    read().await.map_err(|_| "signedOut")
}

type ProfileFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

struct ProfileUsage {
    status: reqwest::StatusCode,
    retry_after: u64,
    data: Option<serde_json::Value>,
}

trait ProfileIo: Sync {
    fn enabled(&self, profile: &crate::services::claude_profiles::Profile) -> bool;
    fn now_ms(&self) -> u64;
    fn credentials<'a>(
        &'a self,
        profile: &'a crate::services::claude_profiles::Profile,
        state: Arc<ProfileState>,
        force: bool,
    ) -> ProfileFuture<'a, Result<CachedCredentials, ()>>;
    fn identity<'a>(
        &'a self,
        state: &'a ProfileState,
        credentials: &'a CachedCredentials,
    ) -> ProfileFuture<'a, Option<Identity>>;
    fn mismatch(
        &self,
        profile: &crate::services::claude_profiles::Profile,
        identity: &Identity,
    ) -> bool;
    fn usage<'a>(
        &'a self,
        credentials: &'a CachedCredentials,
        state: &'a ProfileState,
    ) -> ProfileFuture<'a, Result<ProfileUsage, String>>;
    fn renew<'a>(
        &'a self,
        profile: &'a crate::services::claude_profiles::Profile,
        state: &'a ProfileState,
        credentials: &'a CachedCredentials,
    ) -> ProfileFuture<'a, bool>;
    fn record(
        &self,
        data: &serde_json::Value,
        status: reqwest::StatusCode,
        alias: &str,
        tail: Option<&str>,
        source: Option<crate::services::quota_history::AccountSource>,
    );
}

struct SystemProfileIo;
impl ProfileIo for SystemProfileIo {
    fn enabled(&self, profile: &crate::services::claude_profiles::Profile) -> bool {
        crate::services::claude_profiles::directory_enabled(profile)
    }
    fn now_ms(&self) -> u64 {
        now_epoch_ms()
    }
    fn credentials<'a>(
        &'a self,
        profile: &'a crate::services::claude_profiles::Profile,
        state: Arc<ProfileState>,
        force: bool,
    ) -> ProfileFuture<'a, Result<CachedCredentials, ()>> {
        let profile = profile.clone();
        Box::pin(async move {
            tauri::async_runtime::spawn_blocking(move || {
                custom_credentials(&profile, &state, force, read_credentials_for_system)
            })
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or(())
        })
    }
    fn identity<'a>(
        &'a self,
        state: &'a ProfileState,
        credentials: &'a CachedCredentials,
    ) -> ProfileFuture<'a, Option<Identity>> {
        Box::pin(resolve_identity(state, credentials))
    }
    fn mismatch(
        &self,
        profile: &crate::services::claude_profiles::Profile,
        identity: &Identity,
    ) -> bool {
        cross_check_identity(Some(&profile.config_dir), identity)
    }
    fn usage<'a>(
        &'a self,
        credentials: &'a CachedCredentials,
        state: &'a ProfileState,
    ) -> ProfileFuture<'a, Result<ProfileUsage, String>> {
        Box::pin(async move {
            let response = request_quota_for(&credentials.access_token, state).await?;
            let status = response.status();
            let retry_after = retry_after_secs(response.headers().get("retry-after"));
            let data = if status.is_success() {
                Some(
                    response
                        .json::<serde_json::Value>()
                        .await
                        .map_err(|_| "Failed to parse response".to_string())?,
                )
            } else {
                None
            };
            Ok(ProfileUsage {
                status,
                retry_after,
                data,
            })
        })
    }
    fn renew<'a>(
        &'a self,
        profile: &'a crate::services::claude_profiles::Profile,
        state: &'a ProfileState,
        credentials: &'a CachedCredentials,
    ) -> ProfileFuture<'a, bool> {
        Box::pin(auto_renew_profile(profile, state, credentials))
    }
    fn record(
        &self,
        data: &serde_json::Value,
        status: reqwest::StatusCode,
        alias: &str,
        tail: Option<&str>,
        source: Option<crate::services::quota_history::AccountSource>,
    ) {
        crate::services::quota_history::record_profile_success(
            data,
            status,
            chrono::Utc::now(),
            alias,
            tail,
            source,
        );
    }
}

pub(crate) async fn fetch_profile(
    profile: &crate::services::claude_profiles::Profile,
    labels: &HashMap<String, String>,
) -> crate::services::claude_profiles::PublicProfile {
    fetch_profile_with(profile, labels, &SystemProfileIo).await
}

async fn fetch_profile_with(
    profile: &crate::services::claude_profiles::Profile,
    labels: &HashMap<String, String>,
    io: &dyn ProfileIo,
) -> crate::services::claude_profiles::PublicProfile {
    use crate::services::claude_profiles::{credential_service_name, PublicProfile};
    let mut public = PublicProfile {
        alias: profile.alias.clone(),
        label: profile.alias.clone(),
        status: "ok",
        account: None,
        quota: None,
        expected_service_name: None,
    };
    let state = profile_state(&profile.config_dir);
    let credentials_state = Arc::clone(&state);
    let force_refresh = state
        .request_gate_state
        .lock()
        .ok()
        .is_some_and(|gate| gate.auth_failure.is_some());
    let credentials = match profile_preflight_with(io.enabled(profile), || {
        io.credentials(profile, credentials_state, force_refresh)
    })
    .await
    {
        Ok(credentials) => credentials,
        Err(status) => {
            public.status = status;
            if status == "signedOut" {
                public.expected_service_name = Some(credential_service_name(&profile.config_dir));
            }
            return public;
        }
    };
    let now = io.now_ms();
    if credentials
        .expires_at_ms
        .is_some_and(|expiry| expiry <= now)
    {
        public.status = "expired";
        if io.renew(profile, &state, &credentials).await {
            return Box::pin(fetch_profile_with(profile, labels, io)).await;
        }
        return public;
    }
    let identity = io.identity(&state, &credentials).await;
    let mismatch = identity
        .as_ref()
        .is_some_and(|identity| io.mismatch(profile, identity));
    if mismatch {
        public.status = "mismatch";
    }
    let active_limit = state
        .request_gate_state
        .lock()
        .ok()
        .and_then(|gate| gate.rate_limited_until)
        .is_some_and(|until| now < until);
    if active_limit {
        if !mismatch {
            public.quota = Some(QuotaData::disconnected("API error: 429 Too Many Requests"));
        }
        return public;
    }
    let blocked_auth = state
        .request_gate_state
        .lock()
        .ok()
        .and_then(|gate| gate.auth_failure)
        .is_some_and(|failure| failure.expires_at_ms == credentials.expires_at_ms);
    if blocked_auth {
        if !mismatch {
            public.quota = Some(QuotaData::disconnected(CLAUDE_AUTH_RELOGIN_MESSAGE));
        }
        return public;
    }
    let cached = state.quota_cache.lock().ok().and_then(|cache| {
        cache
            .as_ref()
            .filter(|entry| entry.cached_at.elapsed() < QUOTA_CACHE_TTL)
            .map(|entry| entry.data.clone())
    });
    if let Some(mut quota) = cached.filter(|_| !mismatch) {
        public.account = identity.as_ref().and_then(|identity| {
            Some(crate::domain::models::ClaudeAccount {
                tail: uuid_tail(&identity.uuid)?,
                email: identity.email.clone(),
            })
        });
        quota.account = public.account.clone();
        public.status = if public.account.is_some() {
            "ok"
        } else {
            "unidentified"
        };
        if let Some(account) = &public.account {
            public.label = labels
                .get(&account.tail)
                .cloned()
                .unwrap_or_else(|| profile.alias.clone());
        }
        public.quota = Some(quota);
        return public;
    }
    let response = match io.usage(&credentials, &state).await {
        Ok(response) => response,
        Err(error) => {
            if !mismatch {
                public.quota = Some(QuotaData::disconnected(error));
            }
            return public;
        }
    };
    let status = response.status;
    if is_auth_error(status) {
        if let Ok(mut gate) = state.request_gate_state.lock() {
            gate.auth_failure = Some(AuthFailure {
                expires_at_ms: credentials.expires_at_ms,
            });
        }
        if !mismatch {
            public.quota = Some(QuotaData::disconnected(CLAUDE_AUTH_RELOGIN_MESSAGE));
        }
        return public;
    }
    if is_rate_limited(status) {
        let until = rate_limited_until(now, response.retry_after);
        if let Ok(mut gate) = state.request_gate_state.lock() {
            gate.rate_limited_until = Some(until);
        }
    }
    if !status.is_success() {
        if !mismatch {
            public.quota = Some(QuotaData::disconnected(format!("API error: {status}")));
        }
        return public;
    }
    let data = match response.data {
        Some(data) => data,
        None => {
            if !mismatch {
                public.quota = Some(QuotaData::disconnected("Failed to parse response"));
            }
            return public;
        }
    };
    let session = parse_quota_window(&data["five_hour"]);
    let weekly_total = parse_quota_window(&data["seven_day"]);
    let weekly_opus = parse_quota_window(&data["seven_day_opus"]);
    let weekly_sonnet = parse_quota_window(&data["seven_day_sonnet"]);
    let weekly_design = parse_quota_window(&data["seven_day_omelette"]);
    let weekly_fable5 = parse_first_quota_window(&data, &FABLE5_QUOTA_KEYS)
        .or_else(|| parse_weekly_scoped_model_quota(&data, "Fable"));
    if session.is_none()
        && weekly_total.is_none()
        && weekly_opus.is_none()
        && weekly_sonnet.is_none()
        && weekly_design.is_none()
        && weekly_fable5.is_none()
    {
        if !mismatch {
            public.quota = Some(QuotaData::disconnected(
                "Failed to parse response: no numeric quota utilization fields",
            ));
        }
        return public;
    }
    if mismatch {
        public.status = "mismatch";
        io.record(&data, status, &profile.alias, None, None);
        return public;
    }
    let account = identity.as_ref().and_then(|identity| {
        Some(crate::domain::models::ClaudeAccount {
            tail: uuid_tail(&identity.uuid)?,
            email: identity.email.clone(),
        })
    });
    if let Some(account) = &account {
        public.label = labels
            .get(&account.tail)
            .cloned()
            .unwrap_or_else(|| profile.alias.clone());
    } else {
        public.status = "unidentified";
    }
    let mut quota = QuotaData::connected(
        session,
        weekly_total,
        weekly_opus,
        weekly_sonnet,
        weekly_design,
        weekly_fable5,
    );
    quota.account = account.clone();
    io.record(
        &data,
        status,
        &profile.alias,
        account.as_ref().map(|a| a.tail.as_str()),
        identity.as_ref().map(|i| i.source),
    );
    if let Ok(mut cache) = state.quota_cache.lock() {
        *cache = Some(CachedQuota {
            data: quota.clone(),
            cached_at: Instant::now(),
        });
    }
    if let Ok(mut last) = state.last_successful_quota.lock() {
        *last = Some(LastSuccessfulQuota {
            data: quota.clone(),
            recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        });
    }
    public.account = account;
    public.quota = Some(quota);
    public
}

#[cfg(test)]
mod tests {
    use super::{
        auto_renew_due, auto_renew_expired_quota_core, credential_changed_since_auth_failure,
        credential_error_or_disconnected, fetch_quota_with_signed_out_probe_core, log_msg,
        login_blocked_message, login_refresh_result, manual_renew_due, mark_quota_fetch_error,
        oauth_cache_hit_diagnostic, oauth_env_source_diagnostic, oauth_keychain_source_diagnostic,
        parse_auth_status_logged_in, parse_first_quota_window, parse_quota_window,
        parse_weekly_scoped_model_quota, quota_request_gate, rate_limited_until,
        refresh_claude_login_core, request_gate_active, request_quota_diagnostic, retry_after_secs,
        signed_out_outcome, stale_login_quota_from_last, stale_quota_usable, test_log_messages,
        AuthFailure, AutoPingAttempt, ClaudeCredentialChanged, ClaudeLoginRefreshResult,
        LastSuccessfulQuota, QuotaRequestGate, CLAUDE_AUTH_RELOGIN_MESSAGE,
        CLAUDE_SIGNED_OUT_MESSAGE, DEFAULT_RETRY_AFTER_SECS, EXPIRY_SAFETY_WINDOW_MS,
        FABLE5_QUOTA_KEYS, MAX_STALE_QUOTA_AGE, TEST_LOG_DESTINATION,
    };
    use crate::domain::models::{ClaudeAutoRenewStatus, ClaudeStaleReason, QuotaData, UsageInfo};
    use serde_json::{json, Value};
    use std::cell::Cell;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const SENTINEL_TOKEN: &str = "secret-prefix-sensitive-value-secret-suffix";
    static AUTO_RENEW_TEST_LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();

    struct FakeProfileIo {
        enabled: bool,
        has_credentials: bool,
        identity: Option<super::Identity>,
        mismatch: bool,
        expires_at_ms: Option<u64>,
        usage_percent: f64,
        reads: std::sync::atomic::AtomicUsize,
        usages: std::sync::atomic::AtomicUsize,
        records: Mutex<
            Vec<(
                Option<String>,
                Option<crate::services::quota_history::AccountSource>,
            )>,
        >,
    }

    impl super::ProfileIo for FakeProfileIo {
        fn enabled(&self, _: &crate::services::claude_profiles::Profile) -> bool {
            self.enabled
        }
        fn now_ms(&self) -> u64 {
            100
        }
        fn credentials<'a>(
            &'a self,
            _: &'a crate::services::claude_profiles::Profile,
            _: Arc<super::ProfileState>,
            _: bool,
        ) -> super::ProfileFuture<'a, Result<super::CachedCredentials, ()>> {
            Box::pin(async move {
                self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if !self.has_credentials {
                    return Err(());
                }
                Ok(super::CachedCredentials {
                    access_token: "fixture".into(),
                    cached_at: std::time::Instant::now(),
                    expires_at_ms: self.expires_at_ms,
                    service: "fixture".into(),
                    identity: self.identity.clone(),
                })
            })
        }
        fn identity<'a>(
            &'a self,
            _: &'a super::ProfileState,
            _: &'a super::CachedCredentials,
        ) -> super::ProfileFuture<'a, Option<super::Identity>> {
            Box::pin(async move { self.identity.clone() })
        }
        fn mismatch(
            &self,
            _: &crate::services::claude_profiles::Profile,
            _: &super::Identity,
        ) -> bool {
            self.mismatch
        }
        fn usage<'a>(
            &'a self,
            _: &'a super::CachedCredentials,
            _: &'a super::ProfileState,
        ) -> super::ProfileFuture<'a, Result<super::ProfileUsage, String>> {
            Box::pin(async move {
                self.usages
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(super::ProfileUsage {
                    status: reqwest::StatusCode::OK,
                    retry_after: 300,
                    data: Some(
                        json!({"five_hour":{"utilization":self.usage_percent,"resets_at":null}}),
                    ),
                })
            })
        }
        fn renew<'a>(
            &'a self,
            _: &'a crate::services::claude_profiles::Profile,
            _: &'a super::ProfileState,
            _: &'a super::CachedCredentials,
        ) -> super::ProfileFuture<'a, bool> {
            Box::pin(async { false })
        }
        fn record(
            &self,
            _: &Value,
            _: reqwest::StatusCode,
            _: &str,
            tail: Option<&str>,
            source: Option<crate::services::quota_history::AccountSource>,
        ) {
            self.records
                .lock()
                .unwrap()
                .push((tail.map(str::to_owned), source));
        }
    }

    fn fake_profile_io() -> FakeProfileIo {
        FakeProfileIo {
            enabled: true,
            has_credentials: true,
            identity: Some(super::Identity {
                uuid: "7fd160a2-6103-4038-9fa1-000000d5fb0b".into(),
                email: Some("fixture@example.test".into()),
                source: crate::services::quota_history::AccountSource::Credential,
            }),
            mismatch: false,
            expires_at_ms: Some(10_000),
            usage_percent: 37.0,
            reads: std::sync::atomic::AtomicUsize::new(0),
            usages: std::sync::atomic::AtomicUsize::new(0),
            records: Mutex::new(vec![]),
        }
    }

    fn fake_profile(path: &str) -> crate::services::claude_profiles::Profile {
        crate::services::claude_profiles::Profile {
            alias: "lucky".into(),
            config_dir: path.into(),
        }
    }

    #[test]
    fn profile_orchestration_distinguishes_disabled_and_signed_out() {
        let mut io = fake_profile_io();
        io.enabled = false;
        let profile = fake_profile("/fixture/disabled-orchestration");
        let disabled = tauri::async_runtime::block_on(super::fetch_profile_with(
            &profile,
            &std::collections::HashMap::new(),
            &io,
        ));
        assert_eq!(disabled.status, "disabled");
        assert_eq!(io.reads.load(std::sync::atomic::Ordering::SeqCst), 0);
        io.enabled = true;
        io.has_credentials = false;
        let signed_out = tauri::async_runtime::block_on(super::fetch_profile_with(
            &profile,
            &std::collections::HashMap::new(),
            &io,
        ));
        assert_eq!(signed_out.status, "signedOut");
        assert_eq!(io.usages.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn profile_orchestration_hides_mismatch_and_records_no_identity() {
        let mut io = fake_profile_io();
        io.mismatch = true;
        let profile = fake_profile("/fixture/mismatch-orchestration");
        let result = tauri::async_runtime::block_on(super::fetch_profile_with(
            &profile,
            &std::collections::HashMap::new(),
            &io,
        ));
        assert_eq!(result.status, "mismatch");
        assert!(result.quota.is_none());
        assert_eq!(*io.records.lock().unwrap(), vec![(None, None)]);
    }

    #[test]
    fn profile_orchestration_keeps_unidentified_quota_without_identity() {
        let mut io = fake_profile_io();
        io.identity = None;
        io.mismatch = true;
        let profile = fake_profile("/fixture/unidentified-orchestration");
        let result = tauri::async_runtime::block_on(super::fetch_profile_with(
            &profile,
            &std::collections::HashMap::new(),
            &io,
        ));
        assert_eq!(result.status, "unidentified");
        assert!(result.quota.unwrap().session.is_some());
        assert_eq!(*io.records.lock().unwrap(), vec![(None, None)]);
    }

    #[test]
    fn profile_orchestration_expires_before_usage() {
        let mut io = fake_profile_io();
        io.expires_at_ms = Some(99);
        let profile = fake_profile("/fixture/expired-orchestration");
        let result = tauri::async_runtime::block_on(super::fetch_profile_with(
            &profile,
            &std::collections::HashMap::new(),
            &io,
        ));
        assert_eq!(result.status, "expired");
        assert_eq!(io.usages.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn profile_orchestration_never_reuses_another_profiles_quota_or_identity() {
        let io_a = fake_profile_io();
        let mut io_b = fake_profile_io();
        io_b.usage_percent = 82.0;
        io_b.identity.as_mut().unwrap().uuid = "7fd160a2-6103-4038-9fa1-0000008c6913".into();
        let a = fake_profile("/fixture/isolation-a");
        let b = fake_profile("/fixture/isolation-b");
        let labels = std::collections::HashMap::from([("8c6913".into(), "Lucky".into())]);
        let result_a =
            tauri::async_runtime::block_on(super::fetch_profile_with(&a, &labels, &io_a));
        let result_b =
            tauri::async_runtime::block_on(super::fetch_profile_with(&b, &labels, &io_b));
        assert_eq!(result_a.account.unwrap().tail, "d5fb0b");
        assert_eq!(result_b.account.unwrap().tail, "8c6913");
        assert_eq!(result_a.quota.unwrap().session.unwrap().percentage, 37.0);
        assert_eq!(result_b.quota.unwrap().session.unwrap().percentage, 82.0);
        assert_eq!(result_a.label, "lucky");
        assert_eq!(result_b.label, "Lucky");
    }

    fn auto_renew_test_lock() -> &'static Mutex<()> {
        AUTO_RENEW_TEST_LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn profile_credentials_cache_is_isolated() {
        let a = fake_profile("test-credentials-a");
        let b = fake_profile("test-credentials-b");
        let read = |token: &'static str| {
            move |_: Option<&str>| {
                Ok(super::KeychainCredentials {
                    access_token: token.into(),
                    expires_at_ms: Some(123),
                    cred_name: "fixture".into(),
                    identity: None,
                })
            }
        };
        let a_state = super::profile_state(&a.config_dir);
        let b_state = super::profile_state(&b.config_dir);
        assert_eq!(
            super::custom_credentials(&a, &a_state, false, read("a"))
                .unwrap()
                .access_token,
            "a"
        );
        assert_eq!(
            super::custom_credentials(&b, &b_state, false, read("b"))
                .unwrap()
                .access_token,
            "b"
        );
        assert_eq!(
            super::custom_credentials(&a, &a_state, false, |_| panic!("cache missed"))
                .unwrap()
                .access_token,
            "a"
        );
    }

    #[test]
    fn profile_quota_cache_is_isolated() {
        let a = fake_profile("test-quota-production-a");
        let b = fake_profile("test-quota-production-b");
        let io_a = fake_profile_io();
        let mut io_b = fake_profile_io();
        io_b.usage_percent = 82.0;
        let labels = std::collections::HashMap::new();
        tauri::async_runtime::block_on(super::fetch_profile_with(&a, &labels, &io_a));
        let result = tauri::async_runtime::block_on(super::fetch_profile_with(&b, &labels, &io_b));
        assert_eq!(result.quota.unwrap().session.unwrap().percentage, 82.0);
        assert_eq!(io_b.usages.load(std::sync::atomic::Ordering::SeqCst), 1);
        let cached_a =
            tauri::async_runtime::block_on(super::fetch_profile_with(&a, &labels, &io_a));
        let cached_b =
            tauri::async_runtime::block_on(super::fetch_profile_with(&b, &labels, &io_b));
        assert_eq!(cached_a.quota.unwrap().session.unwrap().percentage, 37.0);
        assert_eq!(cached_b.quota.unwrap().session.unwrap().percentage, 82.0);
        assert_eq!(io_a.usages.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(io_b.usages.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn profile_last_successful_quota_is_isolated() {
        let a = fake_profile("test-last-production-a");
        let b = fake_profile("test-last-production-b");
        tauri::async_runtime::block_on(super::fetch_profile_with(
            &a,
            &std::collections::HashMap::new(),
            &fake_profile_io(),
        ));
        assert!(
            super::profile_last_successful_quota(&super::profile_state(&a.config_dir)).is_some()
        );
        assert!(
            super::profile_last_successful_quota(&super::profile_state(&b.config_dir)).is_none()
        );
    }

    #[test]
    fn profile_signed_out_mark_is_isolated() {
        let a = super::profile_state("test-signed-production-a");
        let b = super::profile_state("test-signed-production-b");
        super::mark_profile_signed_out(&a, Some(1));
        assert!(super::profile_signed_out_matches(&a, Some(1)));
        assert!(!super::profile_signed_out_matches(&b, Some(1)));
    }

    #[test]
    fn profile_auto_renew_throttle_is_isolated() {
        let _guard = auto_renew_test_lock().lock().unwrap();
        let a = super::profile_state("test-auto-production-a");
        let b = super::profile_state("test-auto-production-b");
        let probes = Cell::new(0);
        let run = |state: &super::ProfileState| {
            tauri::async_runtime::block_on(super::auto_renew_profile_with(
                state,
                Some(42),
                1,
                true,
                || async {
                    probes.set(probes.get() + 1);
                    true
                },
                || async { true },
                || async { true },
            ))
        };
        assert!(!run(&a));
        assert!(!run(&a));
        assert!(!run(&b));
        assert_eq!(probes.get(), 2);
    }

    #[test]
    fn profile_manual_renew_throttle_is_isolated() {
        let a = super::profile_state("test-manual-a");
        let b = super::profile_state("test-manual-b");
        *a.last_manual_renew_attempt_ms.lock().unwrap() = Some(1);
        assert!(b.last_manual_renew_attempt_ms.lock().unwrap().is_none());
    }

    #[test]
    fn profile_request_time_is_isolated() {
        let a = super::profile_state("test-time-a");
        let b = super::profile_state("test-time-b");
        *a.last_request_time.lock().unwrap() = Some(std::time::Instant::now());
        assert!(b.last_request_time.lock().unwrap().is_none());
    }

    #[test]
    fn profile_request_gate_is_isolated() {
        let a = super::profile_state("test-gate-a");
        let b = super::profile_state("test-gate-b");
        a.request_gate_state.lock().unwrap().rate_limited_until = Some(1);
        assert!(b
            .request_gate_state
            .lock()
            .unwrap()
            .rate_limited_until
            .is_none());
    }

    #[test]
    fn profile_auto_ping_history_is_isolated() {
        let a = super::profile_state("test-ping-a");
        let b = super::profile_state("test-ping-b");
        *a.auto_ping_history.lock().unwrap() = Some(AutoPingAttempt {
            expires_at_ms: None,
            attempted_at_ms: 1,
        });
        assert!(b.auto_ping_history.lock().unwrap().is_none());
    }

    #[test]
    fn disabled_profile_does_not_read_keychain() {
        let calls = Cell::new(0);
        let result =
            tauri::async_runtime::block_on(super::profile_preflight_with(false, || async {
                calls.set(calls.get() + 1);
                Err(())
            }));
        assert_eq!(result.err(), Some("disabled"));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn custom_keychain_lookup_uses_exact_single_service_without_default_fallback() {
        let mut names = Vec::new();
        let result = super::read_credentials_with(Some("/Users/sam/.claude-acct-lucky"), |name| {
            names.push(name.to_owned());
            None
        });
        assert!(result.is_err());
        assert_eq!(names, vec!["Claude Code-credentials-2becd79b"]);
    }

    #[test]
    fn default_keychain_lookup_keeps_four_candidates_in_order() {
        let mut names = Vec::new();
        let result = super::read_credentials_with(None, |name| {
            names.push(name.to_owned());
            None
        });
        assert!(result.is_err());
        assert_eq!(
            names.iter().map(String::as_str).collect::<Vec<_>>(),
            super::CREDENTIAL_NAMES.to_vec()
        );
    }

    #[test]
    fn keychain_token_account_is_the_preferred_identity() {
        let json = br#"{"claudeAiOauth":{"accessToken":"fixture","expiresAt":123,"tokenAccount":{"uuid":"7fd160a2-6103-4038-9fa1-000000d5fb0b","emailAddress":"fixture@example.test"}}}"#;
        let result = super::read_credentials_with(Some("/tmp/test"), |_| Some(json.to_vec()))
            .ok()
            .unwrap();
        let identity = result.identity.unwrap();
        assert_eq!(super::uuid_tail(&identity.uuid).as_deref(), Some("d5fb0b"));
        assert_eq!(identity.email.as_deref(), Some("fixture@example.test"));
        assert_eq!(
            identity.source,
            crate::services::quota_history::AccountSource::Credential
        );
    }

    #[test]
    fn missing_profile_credentials_is_signed_out_not_disabled() {
        let calls = Cell::new(0);
        let result =
            tauri::async_runtime::block_on(super::profile_preflight_with(true, || async {
                calls.set(calls.get() + 1);
                Err(())
            }));
        assert_eq!(result.err(), Some("signedOut"));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn profile_auto_renew_sends_at_most_once_per_expiry() {
        let _guard = auto_renew_test_lock().lock().unwrap();
        let state = super::ProfileState::default();
        let mut quota = QuotaData::disconnected("fixture");
        quota.session = Some(UsageInfo {
            used: 1.0,
            limit: 100.0,
            percentage: 1.0,
            reset_time: Some("1970-01-01T01:00:00Z".into()),
        });
        *state.last_successful_quota.lock().unwrap() = Some(LastSuccessfulQuota {
            data: quota,
            recorded_at: "fixture".into(),
        });
        let sends = Cell::new(0);
        let run = |now| {
            tauri::async_runtime::block_on(super::auto_renew_profile_with(
                &state,
                Some(42),
                now,
                true,
                || async { false },
                || async { true },
                || async {
                    sends.set(sends.get() + 1);
                    true
                },
            ))
        };
        assert!(run(100));
        assert!(!run(100 + super::AUTO_RENEW_INTERVAL_MS));
        assert_eq!(sends.get(), 1);
    }

    #[test]
    fn expired_profile_without_open_window_never_sends() {
        let _guard = auto_renew_test_lock().lock().unwrap();
        let state = super::ProfileState::default();
        let sends = Cell::new(0);
        let sent = tauri::async_runtime::block_on(super::auto_renew_profile_with(
            &state,
            Some(42),
            100_000,
            true,
            || async { false },
            || async { true },
            || async {
                sends.set(sends.get() + 1);
                true
            },
        ));
        assert!(!sent);
        assert_eq!(sends.get(), 0);
    }

    #[test]
    fn open_window_in_profile_a_does_not_authorize_profile_b() {
        let _guard = auto_renew_test_lock().lock().unwrap();
        let a = super::profile_state("test-window-a");
        let b = super::profile_state("test-window-b");
        let mut quota = QuotaData::disconnected("fixture");
        quota.session = Some(UsageInfo {
            used: 1.0,
            limit: 100.0,
            percentage: 1.0,
            reset_time: Some("1970-01-01T01:00:00Z".into()),
        });
        *a.last_successful_quota.lock().unwrap() = Some(LastSuccessfulQuota {
            data: quota,
            recorded_at: "fixture".into(),
        });
        let sends = Cell::new(0);
        let run = |state: &super::ProfileState| {
            tauri::async_runtime::block_on(super::auto_renew_profile_with(
                state,
                Some(42),
                100_000,
                true,
                || async { false },
                || async { true },
                || async {
                    sends.set(sends.get() + 1);
                    true
                },
            ))
        };
        assert!(run(&a));
        assert!(!run(&b));
        assert_eq!(sends.get(), 1);
    }

    #[test]
    fn profile_auto_renew_process_probe_blocks_send() {
        let _guard = auto_renew_test_lock().lock().unwrap();
        let state = super::ProfileState::default();
        let sends = Cell::new(0);
        let result = tauri::async_runtime::block_on(super::auto_renew_profile_with(
            &state,
            Some(42),
            1,
            true,
            || async { true },
            || async { true },
            || async {
                sends.set(sends.get() + 1);
                true
            },
        ));
        assert!(!result);
        assert_eq!(sends.get(), 0);
    }

    #[test]
    fn identity_profile_api_failure_is_not_retried_until_timeout_or_credential_change() {
        let state = super::ProfileState::default();
        let mut credentials = super::CachedCredentials {
            access_token: "fixture".into(),
            cached_at: std::time::Instant::now(),
            expires_at_ms: Some(1),
            service: "test".into(),
            identity: None,
        };
        let now = std::time::Instant::now();
        let calls = Cell::new(0);
        let invoke = |credentials: &super::CachedCredentials, at| {
            tauri::async_runtime::block_on(super::resolve_identity_with(
                &state,
                credentials,
                at,
                || async {
                    calls.set(calls.get() + 1);
                    None
                },
            ))
        };
        assert!(invoke(&credentials, now).is_none());
        assert!(invoke(&credentials, now + Duration::from_secs(599)).is_none());
        assert_eq!(calls.get(), 1);
        assert!(invoke(&credentials, now + Duration::from_secs(600)).is_none());
        assert_eq!(calls.get(), 2);
        credentials.expires_at_ms = Some(2);
        assert!(invoke(&credentials, now + Duration::from_secs(601)).is_none());
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn identity_profile_api_success_is_cached_until_credential_changes() {
        let state = super::ProfileState::default();
        let mut credentials = super::CachedCredentials {
            access_token: "fixture".into(),
            cached_at: std::time::Instant::now(),
            expires_at_ms: Some(1),
            service: "first".into(),
            identity: None,
        };
        let calls = Cell::new(0);
        let now = std::time::Instant::now();
        let invoke = |credentials: &super::CachedCredentials, at| {
            tauri::async_runtime::block_on(super::resolve_identity_with(
                &state,
                credentials,
                at,
                || async {
                    calls.set(calls.get() + 1);
                    Some(super::Identity {
                        uuid: "7fd160a2-6103-4038-9fa1-000000d5fb0b".into(),
                        email: None,
                        source: crate::services::quota_history::AccountSource::ProfileApi,
                    })
                },
            ))
        };
        assert!(invoke(&credentials, now).is_some());
        assert!(invoke(&credentials, now + Duration::from_secs(3600)).is_some());
        assert_eq!(calls.get(), 1);
        credentials.service = "second".into();
        assert!(invoke(&credentials, now + Duration::from_secs(3601)).is_some());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn credential_identity_does_not_call_profile_api() {
        let state = super::ProfileState::default();
        let credentials = super::CachedCredentials {
            access_token: "fixture".into(),
            cached_at: std::time::Instant::now(),
            expires_at_ms: Some(1),
            service: "test".into(),
            identity: Some(super::Identity {
                uuid: "7fd160a2-6103-4038-9fa1-000000d5fb0b".into(),
                email: None,
                source: crate::services::quota_history::AccountSource::Credential,
            }),
        };
        let calls = Cell::new(0);
        let identity = tauri::async_runtime::block_on(super::resolve_identity_with(
            &state,
            &credentials,
            std::time::Instant::now(),
            || async {
                calls.set(calls.get() + 1);
                None
            },
        ));
        assert_eq!(
            identity
                .and_then(|value| super::uuid_tail(&value.uuid))
                .as_deref(),
            Some("d5fb0b")
        );
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn claude_json_checks_identity_but_never_supplies_one() {
        let identity = super::Identity {
            uuid: "7fd160a2-6103-4038-9fa1-000000d5fb0b".into(),
            email: None,
            source: crate::services::quota_history::AccountSource::Credential,
        };
        let same = r#"{"oauthAccount":{"accountUuid":"7fd160a2-6103-4038-9fa1-000000d5fb0b"}}"#;
        let other = r#"{"oauthAccount":{"accountUuid":"7fd160a2-6103-4038-9fa1-0000008c6913"}}"#;
        assert!(!super::cross_check_identity_from(Some(same), &identity));
        assert!(super::cross_check_identity_from(Some(other), &identity));
        assert!(!super::cross_check_identity_from(None, &identity));
        assert_eq!(
            identity.source,
            crate::services::quota_history::AccountSource::Credential
        );
    }

    #[test]
    fn desktop_account_non_string_uuid_is_not_uuid() {
        assert_eq!(
            super::desktop_account_tail_from_content(Ok(r#"{"lastKnownAccountUuid":42}"#)),
            Err("notUuid")
        );
    }

    #[test]
    fn desktop_account_sentinel_never_reaches_payload_or_diagnostics() {
        let sentinel = "SENTINEL-TOKEN-7f3a";
        let content = format!(
            r#"{{"lastKnownAccountUuid":"7fd160a2-6103-4038-9fa1-000000d5fb0b","oauth:tokenCache":"{sentinel}"}}"#
        );
        let mut quota = QuotaData::disconnected("test");
        quota.app_account = Some(super::desktop_account_status_from(
            Some("8c6913"),
            &std::collections::HashMap::new(),
            Ok(&content),
        ));
        let payload = serde_json::to_string(&quota).unwrap();
        assert!(!payload.contains(sentinel));
        let invalid = format!("{{invalid {sentinel}");
        let error = super::desktop_account_tail_from_content(Ok(&invalid)).unwrap_err();
        let _ = super::desktop_account_status_from(
            Some("8c6913"),
            &std::collections::HashMap::new(),
            Ok(&invalid),
        );
        assert_eq!(error, "parse");
        assert!(!error.contains(sentinel));
        assert!(!super::test_log_messages()
            .lock()
            .unwrap()
            .join("")
            .contains(sentinel));
    }

    fn assert_excludes_token_fragments(diagnostic: &str) {
        assert!(SENTINEL_TOKEN.is_ascii());
        assert!(diagnostic.contains("<redacted>"));
        assert!(!diagnostic.contains(SENTINEL_TOKEN));
        for width in 6..=SENTINEL_TOKEN.len() {
            for start in 0..=SENTINEL_TOKEN.len() - width {
                let fragment = &SENTINEL_TOKEN[start..start + width];
                assert!(
                    !diagnostic.contains(fragment),
                    "diagnostic contains a token-derived fragment"
                );
            }
        }
        assert!(!diagnostic.contains("token="));
    }

    #[test]
    fn credential_diagnostics_exclude_oauth_token_fragments() {
        let diagnostics = [
            oauth_cache_hit_diagnostic(
                SENTINEL_TOKEN,
                Duration::from_secs(12),
                Some(1_800_000_000_000),
            ),
            oauth_env_source_diagnostic(SENTINEL_TOKEN),
            oauth_keychain_source_diagnostic(
                "Claude Code-credentials",
                SENTINEL_TOKEN,
                Some(1_800_000_000_000),
            ),
        ];

        for diagnostic in diagnostics {
            assert_excludes_token_fragments(&diagnostic);
        }
    }

    #[test]
    fn request_diagnostic_excludes_oauth_token_fragments() {
        let diagnostic = request_quota_diagnostic(SENTINEL_TOKEN, 7, Some(1.5));

        assert_excludes_token_fragments(&diagnostic);
    }

    #[test]
    fn credential_changed_projection_handles_same_different_and_missing_credentials() {
        let failed = Some(AuthFailure {
            expires_at_ms: Some(10),
        });
        assert_eq!(
            credential_changed_since_auth_failure(failed, Ok(Some(10))),
            ClaudeCredentialChanged { changed: false }
        );
        let changed = credential_changed_since_auth_failure(failed, Ok(Some(11)));
        assert_eq!(changed, ClaudeCredentialChanged { changed: true });
        assert_eq!(
            credential_changed_since_auth_failure(failed, Err(())),
            ClaudeCredentialChanged { changed: false }
        );
        assert_eq!(
            credential_changed_since_auth_failure(
                Some(AuthFailure {
                    expires_at_ms: None
                }),
                Ok(Some(11)),
            ),
            ClaudeCredentialChanged { changed: true }
        );

        let serialized = serde_json::to_value(changed).expect("projection serializes");
        assert_eq!(serialized, json!({ "changed": true }));
        assert!(serialized.get("accessToken").is_none());
        assert!(serialized.get("token").is_none());
    }

    #[test]
    fn test_logging_does_not_touch_user_log_dir() {
        log_msg("[test] claude log destination");
        assert_eq!(TEST_LOG_DESTINATION, "memory");
        assert!(test_log_messages()
            .lock()
            .expect("test log buffer lock")
            .iter()
            .any(|line| line.contains("[test] claude log destination")));
    }

    #[test]
    fn parse_quota_window_requires_numeric_utilization() {
        assert!(parse_quota_window(&json!({ "resets_at": "2026-06-06T00:00:00Z" })).is_none());
        assert!(parse_quota_window(&json!({ "utilization": "0" })).is_none());
    }

    #[test]
    fn parse_quota_window_maps_numeric_utilization() {
        let parsed = parse_quota_window(&json!({
            "utilization": 42.5,
            "resets_at": "2026-06-06T00:00:00Z"
        }));
        let window = match parsed {
            Some(window) => window,
            None => panic!("numeric utilization should parse"),
        };

        assert_eq!(window.used, 42.5);
        assert_eq!(window.limit, 100.0);
        assert_eq!(window.percentage, 42.5);
        assert_eq!(window.reset_time.as_deref(), Some("2026-06-06T00:00:00Z"));
    }

    #[test]
    fn parse_first_quota_window_accepts_fable5_aliases() {
        for (index, key) in FABLE5_QUOTA_KEYS.iter().enumerate() {
            let utilization = 60.0 + index as f64;
            let mut data = serde_json::Map::new();
            data.insert(
                (*key).to_string(),
                json!({
                    "utilization": utilization,
                    "resets_at": "2026-07-09T00:00:00Z"
                }),
            );

            let parsed = parse_first_quota_window(&Value::Object(data), &FABLE5_QUOTA_KEYS);
            let window = match parsed {
                Some(window) => window,
                None => panic!("{key} should parse as Fable 5 usage"),
            };

            assert_eq!(window.percentage, utilization);
            assert_eq!(window.reset_time.as_deref(), Some("2026-07-09T00:00:00Z"));
        }
    }

    #[test]
    fn parse_weekly_scoped_model_quota_accepts_limits_array_fable() {
        let parsed = parse_weekly_scoped_model_quota(
            &json!({
                "limits": [
                    {
                        "group": "weekly",
                        "kind": "weekly_scoped",
                        "scope": {
                            "model": {
                                "display_name": "Fable"
                            }
                        },
                        "percent": 28,
                        "resets_at": "2026-07-09T00:00:00Z"
                    }
                ]
            }),
            "Fable",
        );
        let window = match parsed {
            Some(window) => window,
            None => panic!("Fable scoped weekly limit should parse"),
        };

        assert_eq!(window.percentage, 28.0);
        assert_eq!(window.reset_time.as_deref(), Some("2026-07-09T00:00:00Z"));
    }

    #[test]
    fn stale_quota_rejects_disconnected_and_expired_snapshots() {
        assert!(stale_quota_usable(true, Duration::from_secs(60)));
        assert!(!stale_quota_usable(false, Duration::from_secs(1)));
        assert!(!stale_quota_usable(true, MAX_STALE_QUOTA_AGE));
        assert!(stale_quota_usable(
            true,
            MAX_STALE_QUOTA_AGE - Duration::from_secs(1)
        ));
    }

    #[test]
    fn stale_quota_fallback_keeps_connected_data_and_sets_error() {
        let stale = mark_quota_fetch_error(
            crate::domain::models::QuotaData {
                account: None,
                account_label: None,
                app_account: None,
                connected: true,
                session: None,
                weekly_total: None,
                weekly_opus: None,
                weekly_sonnet: None,
                weekly_design: None,
                weekly_fable5: None,
                stale_since: None,
                stale_reason: None,
                auto_renew: None,
                error: None,
            },
            "Network error: connection reset".to_string(),
        );
        assert!(stale.connected);
        assert_eq!(
            stale.error.as_deref(),
            Some("Network error: connection reset")
        );
    }

    #[test]
    fn expired_quota_returns_last_reading_with_stale_since() {
        let last = LastSuccessfulQuota {
            data: QuotaData::connected(
                Some(crate::domain::models::UsageInfo {
                    used: 12.0,
                    limit: 100.0,
                    percentage: 12.0,
                    reset_time: Some("2026-10-11T10:00:00Z".to_string()),
                }),
                None,
                None,
                None,
                None,
                None,
            ),
            // Deliberately older than MAX_STALE_QUOTA_AGE: login state keeps it.
            recorded_at: "2026-10-11T09:18:00Z".to_string(),
        };
        let stale = stale_login_quota_from_last(
            Some(last),
            ClaudeStaleReason::Expired,
            CLAUDE_AUTH_RELOGIN_MESSAGE.to_string(),
        );
        assert!(stale.connected);
        assert_eq!(stale.stale_since.as_deref(), Some("2026-10-11T09:18:00Z"));
        assert_eq!(stale.stale_reason, Some(ClaudeStaleReason::Expired));
        assert!(stale.session.is_some());
    }

    #[test]
    fn transient_error_stale_limit_unchanged() {
        assert!(stale_quota_usable(
            true,
            MAX_STALE_QUOTA_AGE - Duration::from_secs(1)
        ));
        assert!(!stale_quota_usable(true, MAX_STALE_QUOTA_AGE));
    }

    #[test]
    fn quota_gate_blocks_expired_or_near_expiry_credentials_without_a_request() {
        let now = 1_000_000;
        assert_eq!(
            quota_request_gate(now, Some(now), None, None, false),
            QuotaRequestGate::AuthBlocked
        );
        assert_eq!(
            quota_request_gate(now, Some(now + EXPIRY_SAFETY_WINDOW_MS), None, None, false),
            QuotaRequestGate::AuthBlocked
        );
        assert_eq!(
            quota_request_gate(now, Some(now + 3_600_000), None, None, false),
            QuotaRequestGate::Allow
        );
        assert_eq!(
            quota_request_gate(now, None, None, None, false),
            QuotaRequestGate::Allow
        );
    }

    #[test]
    fn quota_gate_blocks_matching_signed_out_mark_even_with_valid_or_missing_expiry() {
        let now = 1_000_000;
        assert_eq!(
            quota_request_gate(now, Some(now + 3_600_000), None, None, true),
            QuotaRequestGate::AuthBlocked
        );
        assert_eq!(
            quota_request_gate(now, None, None, None, true),
            QuotaRequestGate::AuthBlocked
        );
        assert_eq!(
            quota_request_gate(now, Some(now + 3_600_000), None, None, false),
            QuotaRequestGate::Allow
        );
        assert_eq!(
            quota_request_gate(now, None, None, None, false),
            QuotaRequestGate::Allow
        );
    }

    #[test]
    fn quota_gate_requires_a_changed_expiry_after_an_auth_failure() {
        let now = 1_000_000;
        assert_eq!(
            quota_request_gate(
                now,
                Some(now + 3_600_000),
                failure(Some(now + 3_600_000)),
                None,
                false
            ),
            QuotaRequestGate::AuthBlocked
        );
        assert_eq!(
            quota_request_gate(
                now,
                Some(now + 7_200_000),
                failure(Some(now + 3_600_000)),
                None,
                false
            ),
            QuotaRequestGate::Allow
        );
        // A failing credential without an expiry stays blocked while it is unchanged.
        assert_eq!(
            quota_request_gate(now, None, failure(None), None, false),
            QuotaRequestGate::AuthBlocked
        );
        assert_eq!(
            quota_request_gate(now, Some(now + 3_600_000), failure(None), None, false),
            QuotaRequestGate::Allow
        );
    }

    fn failure(expires_at_ms: Option<u64>) -> Option<AuthFailure> {
        Some(AuthFailure { expires_at_ms })
    }

    #[test]
    fn request_gate_reads_credentials_first_whenever_a_gate_is_active() {
        let now = 1_000_000;
        assert!(!request_gate_active(now, None, None, false));
        assert!(!request_gate_active(now, None, Some(now), false));
        assert!(request_gate_active(now, None, Some(now + 1), false));
        assert!(request_gate_active(now, failure(None), None, false));
        assert!(request_gate_active(now, None, None, true));
        // Expired credential during an active 429 window reports the auth error.
        assert_eq!(
            quota_request_gate(now, Some(now - 1), None, Some(now + 120_000), false),
            QuotaRequestGate::AuthBlocked
        );
    }

    #[test]
    fn quota_gate_observes_retry_after_and_auth_has_priority() {
        let now = 1_000_000;
        let until = rate_limited_until(now, 120);
        assert_eq!(
            quota_request_gate(now, None, None, Some(until), false),
            QuotaRequestGate::RateLimited
        );
        assert_eq!(
            quota_request_gate(until, None, None, Some(until), false),
            QuotaRequestGate::Allow
        );
        assert_eq!(
            quota_request_gate(
                now,
                Some(now + 3_600_000),
                failure(Some(now + 3_600_000)),
                Some(until),
                false
            ),
            QuotaRequestGate::AuthBlocked
        );
        let seconds = reqwest::header::HeaderValue::from_static("120");
        assert_eq!(retry_after_secs(Some(&seconds)), 120);
        assert_eq!(retry_after_secs(None), DEFAULT_RETRY_AFTER_SECS);
        let invalid = reqwest::header::HeaderValue::from_static("not-seconds");
        assert_eq!(retry_after_secs(Some(&invalid)), DEFAULT_RETRY_AFTER_SECS);
    }

    #[test]
    fn refresh_login_decision_requires_a_future_increased_expiry() {
        let now = 1_000_000;
        assert_eq!(
            login_refresh_result(Some(now + 1), Some(now + 2), now),
            ClaudeLoginRefreshResult::Refreshed
        );
        assert_eq!(
            login_refresh_result(Some(now + 1), Some(now + 1), now),
            ClaudeLoginRefreshResult::Unchanged
        );
        assert_eq!(
            login_refresh_result(Some(now + 1), Some(now), now),
            ClaudeLoginRefreshResult::Unchanged
        );
    }

    #[test]
    fn refresh_core_reports_refreshed_without_probing() {
        let now = 1_000_000;
        let probes = Cell::new(0);
        let result = tauri::async_runtime::block_on(refresh_claude_login_core(
            || Box::pin(std::future::ready(Ok(Some(now - 1)))),
            || Box::pin(std::future::ready(Ok(Some(now + 1)))),
            now,
            |_| panic!("a refreshed login must not check the signed-out mark"),
            |_, _| panic!("a refreshed login must not check the auth gate"),
            || panic!("a refreshed login must not check the throttle"),
            || {
                probes.set(probes.get() + 1);
                Box::pin(std::future::ready(ClaudeLoginRefreshResult::SignedOut))
            },
        ));
        assert_eq!(result, ClaudeLoginRefreshResult::Refreshed);
        assert_eq!(probes.get(), 0);
    }

    #[test]
    fn refresh_core_calls_probe_once_and_returns_its_result_when_expired_and_due() {
        let now = 1_000_000;
        for probe_result in [
            ClaudeLoginRefreshResult::SignedOut,
            ClaudeLoginRefreshResult::Unchanged,
            ClaudeLoginRefreshResult::Failed,
        ] {
            let probes = Cell::new(0);
            let result = tauri::async_runtime::block_on(refresh_claude_login_core(
                || Box::pin(std::future::ready(Ok(Some(now - 1)))),
                || Box::pin(std::future::ready(Ok(Some(now - 1)))),
                now,
                |_| false,
                |_, _| true,
                || true,
                || {
                    probes.set(probes.get() + 1);
                    Box::pin(std::future::ready(probe_result))
                },
            ));
            assert_eq!(result, probe_result);
            assert_eq!(probes.get(), 1);
        }
    }

    #[test]
    fn refresh_core_skips_probe_when_throttled() {
        let now = 1_000_000;
        let probes = Cell::new(0);
        let result = tauri::async_runtime::block_on(refresh_claude_login_core(
            || Box::pin(std::future::ready(Ok(Some(now - 1)))),
            || Box::pin(std::future::ready(Ok(Some(now - 1)))),
            now,
            |_| false,
            |_, _| true,
            || false,
            || {
                probes.set(probes.get() + 1);
                Box::pin(std::future::ready(ClaudeLoginRefreshResult::SignedOut))
            },
        ));
        assert_eq!(result, ClaudeLoginRefreshResult::Unchanged);
        assert_eq!(probes.get(), 0);
    }

    #[test]
    fn refresh_core_skips_probe_when_an_env_token_disables_manual_renewal() {
        let now = 1_000_000;
        let probes = Cell::new(0);
        let env_token_present = true;
        let result = tauri::async_runtime::block_on(refresh_claude_login_core(
            || Box::pin(std::future::ready(Ok(Some(now - 1)))),
            || Box::pin(std::future::ready(Ok(Some(now - 1)))),
            now,
            |_| false,
            |_, _| true,
            || !env_token_present,
            || {
                probes.set(probes.get() + 1);
                Box::pin(std::future::ready(ClaudeLoginRefreshResult::SignedOut))
            },
        ));
        assert_eq!(result, ClaudeLoginRefreshResult::Unchanged);
        assert_eq!(probes.get(), 0);
    }

    #[test]
    fn refresh_core_returns_signed_out_for_a_matching_mark_without_probing() {
        let now = 1_000_000;
        let probes = Cell::new(0);
        let result = tauri::async_runtime::block_on(refresh_claude_login_core(
            || Box::pin(std::future::ready(Ok(Some(now - 1)))),
            || Box::pin(std::future::ready(Ok(Some(now - 1)))),
            now,
            |_| true,
            |_, _| panic!("a matching signed-out mark must bypass the auth gate"),
            || panic!("a matching signed-out mark must bypass the throttle"),
            || {
                probes.set(probes.get() + 1);
                Box::pin(std::future::ready(ClaudeLoginRefreshResult::Unchanged))
            },
        ));
        assert_eq!(result, ClaudeLoginRefreshResult::SignedOut);
        assert_eq!(probes.get(), 0);
    }

    #[test]
    fn parses_claude_auth_status_logged_in_only_when_boolean() {
        assert_eq!(
            parse_auth_status_logged_in(br#"{"loggedIn":false,"authMethod":"none"}"#),
            Some(false)
        );
        assert_eq!(
            parse_auth_status_logged_in(br#"{"loggedIn":true}"#),
            Some(true)
        );
        for stdout in [
            b"{}".as_slice(),
            b"not json",
            b"",
            br#"{"loggedIn":"false"}"#,
        ] {
            assert_eq!(parse_auth_status_logged_in(stdout), None);
        }
    }

    #[test]
    fn signed_out_outcome_only_checks_unchanged_refreshes() {
        assert_eq!(
            signed_out_outcome(ClaudeLoginRefreshResult::Unchanged, || Some(false)),
            ClaudeLoginRefreshResult::SignedOut
        );
        for auth_status in [Some(true), None] {
            assert_eq!(
                signed_out_outcome(ClaudeLoginRefreshResult::Unchanged, || auth_status),
                ClaudeLoginRefreshResult::Unchanged
            );
        }
        for refresh in [
            ClaudeLoginRefreshResult::Refreshed,
            ClaudeLoginRefreshResult::Failed,
        ] {
            let calls = Cell::new(0);
            assert_eq!(
                signed_out_outcome(refresh, || {
                    calls.set(calls.get() + 1);
                    Some(false)
                }),
                refresh
            );
            assert_eq!(calls.get(), 0);
        }
    }

    #[test]
    fn login_blocked_message_uses_matching_signed_out_mark() {
        assert_eq!(
            login_blocked_message(Some(Some(0)), Some(0)),
            CLAUDE_SIGNED_OUT_MESSAGE
        );
        assert_eq!(
            login_blocked_message(Some(Some(0)), Some(123)),
            CLAUDE_AUTH_RELOGIN_MESSAGE
        );
        assert_eq!(
            login_blocked_message(None, Some(0)),
            CLAUDE_AUTH_RELOGIN_MESSAGE
        );
        assert_eq!(
            login_blocked_message(Some(None), None),
            CLAUDE_SIGNED_OUT_MESSAGE
        );
    }

    #[test]
    fn auto_and_manual_renew_due_obey_intervals_and_env_override() {
        let now = 1_000_000;
        assert!(auto_renew_due(now, None, false));
        assert!(!auto_renew_due(
            now,
            Some(now - 9 * 60 * 1_000 - 59_000),
            false
        ));
        assert!(auto_renew_due(now, Some(now - 10 * 60 * 1_000), false));
        assert!(!auto_renew_due(now, None, true));
        assert!(manual_renew_due(now, None, false));
        assert!(!manual_renew_due(now, Some(now - 59_000), false));
        assert!(manual_renew_due(now, Some(now - 60_000), false));
        assert!(!manual_renew_due(now, None, true));
    }

    #[test]
    fn signed_out_probe_core_replaces_expired_error_without_refetching() {
        let fetches = Cell::new(0);
        let probes = Cell::new(0);
        let first = QuotaData::disconnected(CLAUDE_AUTH_RELOGIN_MESSAGE.to_string());
        let result = tauri::async_runtime::block_on(fetch_quota_with_signed_out_probe_core(
            || {
                fetches.set(fetches.get() + 1);
                Box::pin(std::future::ready(first.clone()))
            },
            || {
                probes.set(probes.get() + 1);
                Box::pin(std::future::ready(ClaudeLoginRefreshResult::SignedOut))
            },
            || true,
        ));
        assert_eq!(fetches.get(), 1);
        assert_eq!(probes.get(), 1);
        assert_eq!(result.error.as_deref(), Some(CLAUDE_SIGNED_OUT_MESSAGE));
    }

    #[test]
    fn signed_out_probe_core_keeps_expired_result_when_probe_does_not_detect_sign_out() {
        for outcome in [
            ClaudeLoginRefreshResult::Unchanged,
            ClaudeLoginRefreshResult::Failed,
        ] {
            let fetches = Cell::new(0);
            let first = QuotaData::disconnected(CLAUDE_AUTH_RELOGIN_MESSAGE.to_string());
            let result = tauri::async_runtime::block_on(fetch_quota_with_signed_out_probe_core(
                || {
                    fetches.set(fetches.get() + 1);
                    Box::pin(std::future::ready(first.clone()))
                },
                || Box::pin(std::future::ready(outcome)),
                || true,
            ));
            assert_eq!(fetches.get(), 1);
            assert_eq!(result.error.as_deref(), Some(CLAUDE_AUTH_RELOGIN_MESSAGE));
        }
    }

    #[test]
    fn signed_out_probe_core_skips_probe_before_due() {
        let probes = Cell::new(0);
        let first = QuotaData::disconnected(CLAUDE_AUTH_RELOGIN_MESSAGE.to_string());
        let _ = tauri::async_runtime::block_on(fetch_quota_with_signed_out_probe_core(
            || Box::pin(std::future::ready(first.clone())),
            || {
                probes.set(probes.get() + 1);
                Box::pin(std::future::ready(ClaudeLoginRefreshResult::SignedOut))
            },
            || false,
        ));
        assert_eq!(probes.get(), 0);
    }

    #[test]
    fn claude_http_client_unavailable_is_transient_not_auth() {
        let probes = Cell::new(0);
        let result = tauri::async_runtime::block_on(fetch_quota_with_signed_out_probe_core(
            || {
                Box::pin(std::future::ready(QuotaData::disconnected(
                    "Network error: shared HTTP client unavailable: injected failure".to_string(),
                )))
            },
            || {
                probes.set(probes.get() + 1);
                Box::pin(std::future::ready(ClaudeLoginRefreshResult::SignedOut))
            },
            || true,
        ));
        assert_eq!(probes.get(), 0);
        assert_eq!(
            result.error.as_deref(),
            Some("Network error: shared HTTP client unavailable: injected failure")
        );
        assert_eq!(result.stale_reason, None);
    }

    #[test]
    fn signed_out_probe_core_never_checks_due_or_probes_signed_out_result() {
        let due_calls = Cell::new(0);
        let probes = Cell::new(0);
        let first = QuotaData::disconnected(CLAUDE_SIGNED_OUT_MESSAGE.to_string());
        let result = tauri::async_runtime::block_on(fetch_quota_with_signed_out_probe_core(
            || Box::pin(std::future::ready(first.clone())),
            || {
                probes.set(probes.get() + 1);
                Box::pin(std::future::ready(ClaudeLoginRefreshResult::SignedOut))
            },
            || {
                due_calls.set(due_calls.get() + 1);
                true
            },
        ));
        assert_eq!(result.error.as_deref(), Some(CLAUDE_SIGNED_OUT_MESSAGE));
        assert_eq!(due_calls.get(), 0);
        assert_eq!(probes.get(), 0);
    }

    #[test]
    fn signed_out_probe_core_does_not_consult_due_for_other_results() {
        for data in [
            QuotaData::disconnected("API error: 429 Too Many Requests".to_string()),
            QuotaData::connected(None, None, None, None, None, None),
        ] {
            let due_calls = Cell::new(0);
            let probes = Cell::new(0);
            let _ = tauri::async_runtime::block_on(fetch_quota_with_signed_out_probe_core(
                || Box::pin(std::future::ready(data.clone())),
                || {
                    probes.set(probes.get() + 1);
                    Box::pin(std::future::ready(ClaudeLoginRefreshResult::SignedOut))
                },
                || {
                    due_calls.set(due_calls.get() + 1);
                    true
                },
            ));
            assert_eq!(due_calls.get(), 0);
            assert_eq!(probes.get(), 0);
        }
    }

    #[test]
    fn rotates_oversized_claude_logs() {
        use super::rotate_log_if_needed_with_limit;
        let dir = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "quotabar-claude-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("claude.log");
        std::fs::write(&path, vec![b'x'; 32]).unwrap();
        rotate_log_if_needed_with_limit(&path, 16);
        assert!(!path.exists());
        assert!(dir.join("claude.log.1").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn expired_quota() -> QuotaData {
        let mut quota = QuotaData::connected(
            Some(UsageInfo {
                used: 12.0,
                limit: 100.0,
                percentage: 12.0,
                reset_time: Some("2023-11-15T03:13:20Z".to_string()),
            }),
            None,
            None,
            None,
            None,
            None,
        );
        quota.stale_since = Some("2023-11-15T00:00:00Z".to_string());
        quota.stale_reason = Some(ClaudeStaleReason::Expired);
        quota.error = Some(CLAUDE_AUTH_RELOGIN_MESSAGE.to_string());
        quota
    }

    fn retained_stale(reason: ClaudeStaleReason, error: String) -> QuotaData {
        let mut quota = expired_quota();
        quota.stale_reason = Some(reason);
        quota.error = Some(error);
        quota
    }

    #[test]
    fn credential_missing_keeps_last_reading_as_signed_out() {
        let _serial = auto_renew_test_lock().lock().unwrap();
        let last = LastSuccessfulQuota {
            data: expired_quota(),
            recorded_at: "2023-11-15T00:00:00Z".to_string(),
        };
        *super::last_successful_quota().lock().unwrap() = Some(last);
        let result =
            credential_error_or_disconnected("OAuth token not found. Please login".to_string());
        *super::last_successful_quota().lock().unwrap() = None;

        assert!(result.connected);
        assert_eq!(result.stale_reason, Some(ClaudeStaleReason::SignedOut));
        assert!(result.session.is_some());
    }

    #[test]
    fn auto_ping_not_sent_when_signed_out_within_probe_throttle() {
        let _serial = auto_renew_test_lock().lock().unwrap();
        let history = Mutex::new(None);
        let pings = Cell::new(0);
        let result = tauri::async_runtime::block_on(auto_renew_expired_quota_core(
            expired_quota(),
            true,
            1_700_000_000,
            1_700_000_000_000,
            &history,
            retained_stale,
            || Box::pin(std::future::ready(Ok(Some(9)))),
            // This direct, injected probe represents the post-logout refresh
            // within the normal ten-minute probe throttle.
            || Box::pin(std::future::ready(Ok(true))),
            || Box::pin(std::future::ready(false)),
            || {
                pings.set(pings.get() + 1);
                Box::pin(std::future::ready(true))
            },
            || {
                Box::pin(std::future::ready(QuotaData::connected(
                    None, None, None, None, None, None,
                )))
            },
            || {},
        ));

        assert_eq!(pings.get(), 0);
        assert_eq!(result.stale_reason, Some(ClaudeStaleReason::SignedOut));
    }

    #[test]
    fn concurrent_refreshes_send_at_most_one_auto_ping() {
        let _serial = auto_renew_test_lock().lock().unwrap();
        let history = Arc::new(Mutex::new(None));
        let pings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let history = Arc::clone(&history);
            let pings = Arc::clone(&pings);
            workers.push(std::thread::spawn(move || {
                tauri::async_runtime::block_on(auto_renew_expired_quota_core(
                    expired_quota(),
                    true,
                    1_700_000_000,
                    1_700_000_000_000,
                    &history,
                    retained_stale,
                    || Box::pin(std::future::ready(Ok(Some(9)))),
                    || Box::pin(std::future::ready(Ok(false))),
                    || Box::pin(std::future::ready(false)),
                    || {
                        Box::pin(async move {
                            pings.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(50));
                            true
                        })
                    },
                    || {
                        Box::pin(std::future::ready(QuotaData::connected(
                            None, None, None, None, None, None,
                        )))
                    },
                    || {},
                ))
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(pings.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_refetch_after_successful_ping_keeps_last_reading() {
        let _serial = auto_renew_test_lock().lock().unwrap();
        let history = Mutex::new(None);
        let result = tauri::async_runtime::block_on(auto_renew_expired_quota_core(
            expired_quota(),
            true,
            1_700_000_000,
            1_700_000_000_000,
            &history,
            retained_stale,
            || Box::pin(std::future::ready(Ok(Some(9)))),
            || Box::pin(std::future::ready(Ok(false))),
            || Box::pin(std::future::ready(false)),
            || Box::pin(std::future::ready(true)),
            || Box::pin(std::future::ready(QuotaData::disconnected("network error"))),
            || {},
        ));

        assert!(result.connected);
        assert!(result.session.is_some());
        assert_eq!(result.auto_renew, Some(ClaudeAutoRenewStatus::Sent));
    }

    #[test]
    fn auto_renew_status_reflects_skip_reason() {
        let _serial = auto_renew_test_lock().lock().unwrap();
        let setting_off = tauri::async_runtime::block_on(auto_renew_expired_quota_core(
            expired_quota(),
            false,
            1_700_000_000,
            1_700_000_000_000,
            &Mutex::new(None),
            retained_stale,
            || Box::pin(std::future::ready(Ok(Some(9)))),
            || Box::pin(std::future::ready(Ok(false))),
            || Box::pin(std::future::ready(false)),
            || Box::pin(std::future::ready(true)),
            || {
                Box::pin(std::future::ready(QuotaData::connected(
                    None, None, None, None, None, None,
                )))
            },
            || {},
        ));
        let same_expiry = tauri::async_runtime::block_on(auto_renew_expired_quota_core(
            expired_quota(),
            true,
            1_700_000_000,
            1_700_000_000_000,
            &Mutex::new(Some(AutoPingAttempt {
                expires_at_ms: Some(9),
                attempted_at_ms: 0,
            })),
            retained_stale,
            || Box::pin(std::future::ready(Ok(Some(9)))),
            || Box::pin(std::future::ready(Ok(false))),
            || Box::pin(std::future::ready(false)),
            || Box::pin(std::future::ready(true)),
            || {
                Box::pin(std::future::ready(QuotaData::connected(
                    None, None, None, None, None, None,
                )))
            },
            || {},
        ));
        let other_process = tauri::async_runtime::block_on(auto_renew_expired_quota_core(
            expired_quota(),
            true,
            1_700_000_000,
            1_700_000_000_000,
            &Mutex::new(None),
            retained_stale,
            || Box::pin(std::future::ready(Ok(Some(9)))),
            || Box::pin(std::future::ready(Ok(false))),
            || Box::pin(std::future::ready(true)),
            || Box::pin(std::future::ready(true)),
            || {
                Box::pin(std::future::ready(QuotaData::connected(
                    None, None, None, None, None, None,
                )))
            },
            || {},
        ));

        for result in [setting_off, same_expiry, other_process] {
            assert_eq!(result.auto_renew, Some(ClaudeAutoRenewStatus::Skipped));
        }
    }
    fn run_auto_renew_once(
        history: &Mutex<Option<AutoPingAttempt>>,
        now_ms: u64,
        signed_out: Result<bool, ()>,
        pings: &std::sync::atomic::AtomicUsize,
    ) -> QuotaData {
        tauri::async_runtime::block_on(auto_renew_expired_quota_core(
            expired_quota(),
            true,
            1_700_000_000,
            now_ms,
            history,
            retained_stale,
            || Box::pin(std::future::ready(Ok(Some(9)))),
            move || Box::pin(std::future::ready(signed_out)),
            || Box::pin(std::future::ready(false)),
            || {
                pings.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(std::future::ready(true))
            },
            || {
                Box::pin(std::future::ready(QuotaData::connected(
                    None, None, None, None, None, None,
                )))
            },
            || {},
        ))
    }

    #[test]
    fn sequential_refreshes_with_same_expiry_send_one_auto_ping() {
        let _serial = auto_renew_test_lock().lock().unwrap();
        let history = Mutex::new(None);
        let pings = std::sync::atomic::AtomicUsize::new(0);
        run_auto_renew_once(&history, 1_700_000_000_000, Ok(false), &pings);
        // An hour later the 30-minute spacing no longer applies; only the
        // recorded expiry may stop the second Ping.
        run_auto_renew_once(&history, 1_700_003_600_000, Ok(false), &pings);
        assert_eq!(pings.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn manual_ping_in_flight_blocks_auto_ping() {
        let _serial = auto_renew_test_lock().lock().unwrap();
        let held = crate::services::window_ping::ClaudeFlight::acquire();
        assert!(held.is_some());
        let history = Mutex::new(None);
        let pings = std::sync::atomic::AtomicUsize::new(0);
        let result = run_auto_renew_once(&history, 1_700_000_000_000, Ok(false), &pings);
        drop(held);
        assert_eq!(pings.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(result.auto_renew, Some(ClaudeAutoRenewStatus::Skipped));
    }

    #[test]
    fn signed_out_probe_failure_is_treated_as_signed_out() {
        let _serial = auto_renew_test_lock().lock().unwrap();
        let history = Mutex::new(None);
        let pings = std::sync::atomic::AtomicUsize::new(0);
        let result = run_auto_renew_once(&history, 1_700_000_000_000, Err(()), &pings);
        assert_eq!(pings.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(result.stale_reason, Some(ClaudeStaleReason::SignedOut));
    }
    #[test]
    fn auto_renew_probe_failure_and_unparsed_status_count_as_signed_out() {
        assert_eq!(
            super::signed_out_from_auth_status(Ok(Some(true))),
            Ok(false)
        );
        assert_eq!(
            super::signed_out_from_auth_status(Ok(Some(false))),
            Ok(true)
        );
        assert_eq!(super::signed_out_from_auth_status(Ok(None)), Err(()));
        assert_eq!(super::signed_out_from_auth_status(Err("timeout")), Err(()));
    }

    #[test]
    fn invalidate_login_state_drops_fresh_cache_but_keeps_last_reading() {
        let _serial = auto_renew_test_lock().lock().unwrap();
        super::save_quota_cache(&expired_quota_connected_ok());
        assert!(super::get_cached_quota().is_some());
        super::invalidate_login_state();
        let cache_cleared = super::get_cached_quota().is_none();
        let last_kept = super::last_successful_quota().lock().unwrap().is_some();
        *super::last_successful_quota().lock().unwrap() = None;
        assert!(cache_cleared);
        assert!(last_kept);
    }

    #[test]
    fn credential_missing_uses_canonical_signed_out_message() {
        let _serial = auto_renew_test_lock().lock().unwrap();
        let result =
            credential_error_or_disconnected("OAuth token not found. Please login".to_string());
        assert_eq!(
            result.error.as_deref(),
            Some(super::CLAUDE_SIGNED_OUT_MESSAGE)
        );
    }

    fn expired_quota_connected_ok() -> QuotaData {
        let mut quota = expired_quota();
        quota.error = None;
        quota.stale_reason = None;
        quota.stale_since = None;
        quota.connected = true;
        quota
    }
}

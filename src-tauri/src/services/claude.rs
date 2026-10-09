use crate::domain::models::{QuotaData, UsageInfo};
use crate::services::http::{is_transient_os_error, shared_http_client};
use serde::Serialize;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write as IoWrite;
use std::path::Path;
use std::pin::Pin;
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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
static LAST_REQUEST_TIME: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
static LAST_AUTO_RENEW_ATTEMPT_MS: OnceLock<Mutex<Option<u64>>> = OnceLock::new();
static LAST_MANUAL_RENEW_ATTEMPT_MS: OnceLock<Mutex<Option<u64>>> = OnceLock::new();
static SIGNED_OUT_MARK: OnceLock<Mutex<Option<Option<u64>>>> = OnceLock::new();

fn last_request_time() -> &'static Mutex<Option<Instant>> {
    LAST_REQUEST_TIME.get_or_init(|| Mutex::new(None))
}

fn last_auto_renew_attempt_ms() -> &'static Mutex<Option<u64>> {
    LAST_AUTO_RENEW_ATTEMPT_MS.get_or_init(|| Mutex::new(None))
}

fn last_manual_renew_attempt_ms() -> &'static Mutex<Option<u64>> {
    LAST_MANUAL_RENEW_ATTEMPT_MS.get_or_init(|| Mutex::new(None))
}

fn signed_out_mark() -> &'static Mutex<Option<Option<u64>>> {
    SIGNED_OUT_MARK.get_or_init(|| Mutex::new(None))
}

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
}

static CREDENTIALS_CACHE: OnceLock<Mutex<Option<CachedCredentials>>> = OnceLock::new();

fn credentials_cache() -> &'static Mutex<Option<CachedCredentials>> {
    CREDENTIALS_CACHE.get_or_init(|| Mutex::new(None))
}

struct CachedQuota {
    data: QuotaData,
    cached_at: Instant,
}

static QUOTA_CACHE: OnceLock<Mutex<Option<CachedQuota>>> = OnceLock::new();

fn quota_cache() -> &'static Mutex<Option<CachedQuota>> {
    QUOTA_CACHE.get_or_init(|| Mutex::new(None))
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

static REQUEST_GATE_STATE: OnceLock<Mutex<RequestGateState>> = OnceLock::new();

fn request_gate_state() -> &'static Mutex<RequestGateState> {
    REQUEST_GATE_STATE.get_or_init(|| Mutex::new(RequestGateState::default()))
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
}

#[cfg(target_os = "macos")]
fn read_credentials_from_system() -> Result<KeychainCredentials, String> {
    let username = std::env::var("USER").unwrap_or_default();

    for cred_name in CREDENTIAL_NAMES {
        // Use -a $USER to match the exact keychain entry that Claude Code CLI uses
        let mut args = vec!["find-generic-password"];
        if !username.is_empty() {
            args.extend(["-a", &username]);
        }
        args.extend(["-s", cred_name, "-w"]);

        let output = Command::new("security").args(&args).output();

        if let Ok(result) = output {
            if result.status.success() {
                let creds_json = String::from_utf8_lossy(&result.stdout).trim().to_string();
                if creds_json.is_empty() {
                    continue;
                }

                if let Ok(creds) = serde_json::from_str::<serde_json::Value>(&creds_json) {
                    let oauth = &creds["claudeAiOauth"];
                    if let Some(access_token) = oauth["accessToken"].as_str() {
                        let expires_at_ms = oauth["expiresAt"].as_u64();
                        return Ok(KeychainCredentials {
                            access_token: access_token.to_string(),
                            expires_at_ms,
                            cred_name: cred_name.to_string(),
                        });
                    }
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
            });
        }
        return Ok(CachedCredentials {
            access_token: token,
            cached_at: Instant::now(),
            expires_at_ms: None,
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
        });
    }
    Ok(CachedCredentials {
        access_token: keychain.access_token,
        cached_at: Instant::now(),
        expires_at_ms: keychain.expires_at_ms,
    })
}

async fn request_quota(access_token: &str) -> Result<reqwest::Response, String> {
    let (count, gap) = track_request();
    log_msg(&request_quota_diagnostic(access_token, count, gap));

    let start = Instant::now();
    let response = shared_http_client()
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

fn save_quota_cache(data: &QuotaData) {
    if let Ok(mut guard) = quota_cache().lock() {
        *guard = Some(CachedQuota {
            data: data.clone(),
            cached_at: Instant::now(),
        });
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
            stale_or_disconnected(login_blocked_message_for(expires_at_ms).to_string())
        }
        QuotaRequestGate::RateLimited => {
            stale_or_disconnected("API error: 429 Too Many Requests".to_string())
        }
        QuotaRequestGate::Allow => unreachable!("allow does not have a gate error"),
    }
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
    if let Ok(mut guard) = quota_cache().lock() {
        *guard = None;
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
    let command = match crate::services::window_ping::build_claude_auth_status_command() {
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
        return ClaudeLoginRefreshResult::Unchanged;
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
            return ClaudeLoginRefreshResult::Unchanged;
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
    fetch_quota_with_signed_out_probe_core(
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
    .await
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
                Ok(Err(error)) => return fallback_or_disconnected(error),
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

    let credentials = match tauri::async_runtime::spawn_blocking(|| read_credentials(false)).await {
        Ok(Ok(credentials)) => credentials,
        Ok(Err(error)) => {
            log_msg(&format!("[Quota] credential read failed: {error}"));
            return fallback_or_disconnected(error);
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
                    return fallback_or_disconnected(error);
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
            return stale_or_disconnected(
                login_blocked_message_for(fresh_credentials.expires_at_ms).to_string(),
            );
        }
    }

    if !response.status().is_success() {
        let final_status = response.status();
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

    let result = QuotaData::connected(
        session,
        weekly_total,
        weekly_opus,
        weekly_sonnet,
        weekly_design,
        weekly_fable5,
    );

    save_quota_cache(&result);
    result
}

#[cfg(test)]
mod tests {
    use super::{
        auto_renew_due, fetch_quota_with_signed_out_probe_core, login_blocked_message,
        login_refresh_result, manual_renew_due, mark_quota_fetch_error, oauth_cache_hit_diagnostic,
        oauth_env_source_diagnostic, oauth_keychain_source_diagnostic, parse_auth_status_logged_in,
        parse_first_quota_window, parse_quota_window, parse_weekly_scoped_model_quota,
        quota_request_gate, rate_limited_until, refresh_claude_login_core, request_gate_active,
        request_quota_diagnostic, retry_after_secs, signed_out_outcome, stale_quota_usable,
        AuthFailure, ClaudeLoginRefreshResult, QuotaRequestGate, CLAUDE_AUTH_RELOGIN_MESSAGE,
        CLAUDE_SIGNED_OUT_MESSAGE, DEFAULT_RETRY_AFTER_SECS, EXPIRY_SAFETY_WINDOW_MS,
        FABLE5_QUOTA_KEYS, MAX_STALE_QUOTA_AGE,
    };
    use crate::domain::models::QuotaData;
    use serde_json::{json, Value};
    use std::cell::Cell;
    use std::time::Duration;

    const SENTINEL_TOKEN: &str = "secret-prefix-sensitive-value-secret-suffix";

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
                connected: true,
                session: None,
                weekly_total: None,
                weekly_opus: None,
                weekly_sonnet: None,
                weekly_design: None,
                weekly_fable5: None,
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
        let dir = std::env::temp_dir().join(format!(
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
}

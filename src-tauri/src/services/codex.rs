use crate::domain::account::{
    default_codex_profile, AccountCacheKey, CodexProfile, CodexProfileInput,
    CodexProfilePublicQuota, CodexProfileQuota, RouteKey,
};
use crate::domain::models::{
    CodexCredits, CodexData, CodexRateLimitWindow, CodexRateLimits, CodexResetCredit,
    CodexResetCredits,
};
use crate::services::codex_cache::{self, AuthFileStamp};
use crate::services::http::{error_is_transient, is_transient_os_error, shared_http_client};
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

struct LastGoodInfo {
    stamp: AuthFileStamp,
    data: CodexData,
}

/// Most recent successful fetch results, retained without TTL so a transient
/// polling failure does not erase quota that was already displayed.
static LAST_GOOD_INFO: OnceLock<Mutex<HashMap<RouteKey, LastGoodInfo>>> = OnceLock::new();

fn last_good_info() -> &'static Mutex<HashMap<RouteKey, LastGoodInfo>> {
    LAST_GOOD_INFO.get_or_init(|| Mutex::new(HashMap::new()))
}

fn log_msg(msg: &str) {
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let line = format!("[{timestamp}] {msg}\n");

    print!("{line}");

    let home_dir = match dirs::home_dir() {
        Some(path) => path,
        None => {
            eprintln!("[CodexLog] failed to resolve home directory");
            return;
        }
    };
    let log_dir = home_dir.join("Library/Logs/quotabar");
    if let Err(error) = fs::create_dir_all(&log_dir) {
        eprintln!("[CodexLog] failed to create log directory: {error}");
        return;
    }

    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("codex.log"))
    {
        Ok(mut file) => {
            if let Err(error) = file.write_all(line.as_bytes()) {
                eprintln!("[CodexLog] failed to write log: {error}");
            }
        }
        Err(error) => eprintln!("[CodexLog] failed to open log file: {error}"),
    }
}

pub(crate) fn get_codex_home() -> Option<PathBuf> {
    match std::env::var_os("CODEX_HOME") {
        Some(home) if !home.is_empty() => Some(PathBuf::from(home)),
        Some(_) => None,
        None => dirs::home_dir().map(|home| home.join(".codex")),
    }
}

fn profile_home(profile: &CodexProfile) -> Option<PathBuf> {
    profile.home().cloned().or_else(get_codex_home)
}

fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }

    let payload = parts[1];
    let padded = match payload.len() % 4 {
        2 => format!("{payload}=="),
        3 => format!("{payload}="),
        _ => payload.to_string(),
    };
    let standard = padded.replace('-', "+").replace('_', "/");

    STANDARD_NO_PAD
        .decode(&standard)
        .ok()
        .or_else(|| {
            base64::engine::general_purpose::STANDARD
                .decode(&standard)
                .ok()
        })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|json| serde_json::from_str(&json).ok())
}

fn auth_file_path(profile: &CodexProfile) -> Result<PathBuf, String> {
    let codex_home =
        profile_home(profile).ok_or_else(|| "Could not find home directory".to_string())?;
    let auth_file = codex_home.join("auth.json");
    if !auth_file.exists() {
        return Err("Codex not configured. Please run 'codex' to login.".to_string());
    }
    Ok(auth_file)
}

fn auth_file_stamp(auth_file: &Path) -> Result<AuthFileStamp, String> {
    let metadata =
        fs::metadata(auth_file).map_err(|error| format!("Failed to inspect auth.json: {error}"))?;
    let modified = metadata
        .modified()
        .map_err(|error| format!("Failed to inspect auth.json modification time: {error}"))?;
    Ok(AuthFileStamp {
        len: metadata.len(),
        modified,
    })
}

struct StampedAuthReadError {
    message: String,
    pre_read_stamp: Option<AuthFileStamp>,
}

fn read_auth_json_with_stamp(
    profile: &CodexProfile,
) -> Result<(serde_json::Value, AuthFileStamp), StampedAuthReadError> {
    let auth_file = auth_file_path(profile).map_err(|message| StampedAuthReadError {
        message,
        pre_read_stamp: None,
    })?;
    let stamp_before = auth_file_stamp(&auth_file).map_err(|message| StampedAuthReadError {
        message,
        pre_read_stamp: None,
    })?;

    let content = fs::read_to_string(&auth_file).map_err(|error| StampedAuthReadError {
        message: format!("Failed to read auth.json: {error}"),
        pre_read_stamp: Some(stamp_before.clone()),
    })?;
    let stamp_after = auth_file_stamp(&auth_file).map_err(|message| StampedAuthReadError {
        message,
        pre_read_stamp: Some(stamp_before.clone()),
    })?;
    if stamp_before != stamp_after {
        return Err(StampedAuthReadError {
            message: "auth.json changed while it was being read".to_string(),
            pre_read_stamp: None,
        });
    }

    serde_json::from_str(&content)
        .map(|auth_json| (auth_json, stamp_after))
        .map_err(|error| StampedAuthReadError {
            message: format!("Failed to parse auth.json: {error}"),
            pre_read_stamp: None,
        })
}

fn read_auth_json(profile: &CodexProfile) -> Result<serde_json::Value, String> {
    let auth_file = auth_file_path(profile)?;
    let content =
        fs::read_to_string(&auth_file).map_err(|e| format!("Failed to read auth.json: {e}"))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse auth.json: {e}"))
}

fn parse_used_percent(window: &serde_json::Value) -> Option<f64> {
    window
        .get("used_percent")
        .and_then(|value| value.as_f64().or_else(|| value.as_i64().map(|v| v as f64)))
        .map(|value| value.clamp(0.0, 100.0))
}

fn window_minutes_from_seconds(seconds: i64) -> i64 {
    seconds.saturating_add(59) / 60
}

fn parse_rate_limit_window(window: &serde_json::Value) -> Option<CodexRateLimitWindow> {
    if window.is_null() || !window.is_object() {
        return None;
    }

    Some(CodexRateLimitWindow {
        used_percent: parse_used_percent(window)?,
        window_minutes: window["limit_window_seconds"]
            .as_i64()
            .map(window_minutes_from_seconds),
        resets_at: window["reset_at"].as_i64(),
    })
}

fn retain_last_good_info(
    cached: Option<&LastGoodInfo>,
    stamp: Option<&AuthFileStamp>,
    error: String,
    transient: bool,
) -> CodexData {
    if transient {
        if let (Some(stale), Some(current_stamp)) = (cached, stamp) {
            if stale.stamp == *current_stamp {
                let mut data = stale.data.clone();
                data.error = Some(error);
                return data;
            }
        }
    }
    CodexData::disconnected(error)
}

fn fallback_or_disconnected_info(profile: &CodexProfile, error: StampedAuthReadError) -> CodexData {
    let transient = is_transient_os_error(&error.message);
    if let Ok(guard) = last_good_info().lock() {
        return retain_last_good_info(
            guard.get(profile.route()),
            error.pre_read_stamp.as_ref(),
            error.message,
            transient,
        );
    }
    CodexData::disconnected(error.message)
}

pub async fn fetch_codex_info() -> CodexData {
    fetch_codex_info_for(&default_codex_profile()).await
}

pub(crate) async fn fetch_codex_info_for(profile: &CodexProfile) -> CodexData {
    let (auth_json, auth_stamp) = match read_auth_json_with_stamp(profile) {
        Ok(auth) => auth,
        Err(error) => return fallback_or_disconnected_info(profile, error),
    };
    info_from_auth(profile, auth_json, auth_stamp)
}

fn info_from_auth(
    profile: &CodexProfile,
    auth_json: serde_json::Value,
    auth_stamp: AuthFileStamp,
) -> CodexData {
    let id_token = match auth_json["tokens"]["id_token"].as_str() {
        Some(token) => token,
        None => return CodexData::disconnected("No id_token found in auth.json"),
    };

    let payload = match decode_jwt_payload(id_token) {
        Some(payload) => payload,
        None => return CodexData::disconnected("Failed to decode JWT token"),
    };

    let auth_info = &payload["https://api.openai.com/auth"];

    let info = CodexData {
        connected: true,
        plan_type: auth_info["chatgpt_plan_type"]
            .as_str()
            .map(ToString::to_string),
        account_id: auth_info["chatgpt_account_id"]
            .as_str()
            .map(ToString::to_string),
        subscription_until: auth_info["chatgpt_subscription_active_until"]
            .as_str()
            .map(ToString::to_string),
        email: payload["email"].as_str().map(ToString::to_string),
        error: None,
    };

    if let Ok(mut guard) = last_good_info().lock() {
        guard.insert(
            profile.route().clone(),
            LastGoodInfo {
                stamp: auth_stamp,
                data: info.clone(),
            },
        );
    }
    info
}

fn transient_failure_limits(
    account_key: Option<&AccountCacheKey>,
    error: String,
) -> CodexRateLimits {
    match codex_cache::retain_for_account(account_key, error.clone()) {
        Ok(limits) => limits,
        Err(lock_error) => {
            log_msg(&format!("[RateLimits] {lock_error}"));
            CodexRateLimits::disconnected(error)
        }
    }
}

fn transient_auth_failure_limits(
    profile: &CodexProfile,
    auth_stamp: Option<&AuthFileStamp>,
    error: String,
) -> CodexRateLimits {
    match codex_cache::retain_for_auth_stamp(profile.route(), auth_stamp, error.clone()) {
        Ok(limits) => limits,
        Err(lock_error) => {
            log_msg(&format!("[RateLimits] {lock_error}"));
            CodexRateLimits::disconnected(error)
        }
    }
}

fn should_preserve_for_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

fn should_preserve_transport_failure(
    is_timeout: bool,
    is_connect: bool,
    is_transient_os_error: bool,
) -> bool {
    is_timeout || is_connect || is_transient_os_error
}

pub async fn fetch_codex_rate_limits() -> CodexRateLimits {
    fetch_codex_rate_limits_for(&default_codex_profile()).await
}

pub(crate) async fn fetch_codex_rate_limits_for(profile: &CodexProfile) -> CodexRateLimits {
    let (auth_json, auth_stamp) = match read_auth_json_with_stamp(profile) {
        Ok(auth) => auth,
        Err(error) => {
            log_msg(&format!("[RateLimits] auth read failed: {}", error.message));
            return if is_transient_os_error(&error.message) {
                transient_auth_failure_limits(profile, error.pre_read_stamp.as_ref(), error.message)
            } else {
                CodexRateLimits::disconnected(error.message)
            };
        }
    };
    fetch_codex_rate_limits_from_auth(profile, auth_json, auth_stamp).await
}

async fn fetch_codex_rate_limits_from_auth(
    profile: &CodexProfile,
    auth_json: serde_json::Value,
    auth_stamp: AuthFileStamp,
) -> CodexRateLimits {
    let access_token = match auth_json["tokens"]["access_token"].as_str() {
        Some(token) => token,
        None => {
            let error = "No access_token found in auth.json";
            log_msg(&format!("[RateLimits] {error}"));
            return CodexRateLimits::disconnected(error);
        }
    };

    let account_id = auth_json["tokens"]["id_token"]
        .as_str()
        .and_then(decode_jwt_payload)
        .and_then(|payload| {
            payload["https://api.openai.com/auth"]["chatgpt_account_id"]
                .as_str()
                .map(ToString::to_string)
        });
    let account_key = account_id.as_deref().map(|id| profile.cache_key(id));
    let request_sequence = codex_cache::next_request_sequence();

    let client = shared_http_client();
    let mut request = client
        .get("https://chatgpt.com/backend-api/wham/usage")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", "codex-cli")
        .timeout(std::time::Duration::from_secs(10));

    if let Some(account_id) = account_id.as_deref() {
        request = request.header("ChatGPT-Account-Id", account_id);
    }

    let started_at = Instant::now();
    let response = match request.send().await {
        Ok(resp) => resp,
        Err(err) => {
            let error = format!("Network error: {err}");
            let should_preserve = should_preserve_transport_failure(
                err.is_timeout(),
                err.is_connect(),
                error_is_transient(&err),
            );
            log_msg(&format!(
                "[RateLimits] request failed: latency={:.1}s, preservable={should_preserve}, error={error}",
                started_at.elapsed().as_secs_f64(),
            ));
            return if should_preserve {
                transient_failure_limits(account_key.as_ref(), error)
            } else {
                CodexRateLimits::disconnected(error)
            };
        }
    };

    let status = response.status();
    log_msg(&format!(
        "[RateLimits] response: status={status}, latency={:.1}s",
        started_at.elapsed().as_secs_f64()
    ));

    if should_preserve_for_status(status) {
        let error = format!("API error: {status}");
        log_msg("[RateLimits] rate limited; retaining last successful quota if available");
        return transient_failure_limits(account_key.as_ref(), error);
    }

    if status.as_u16() == 401 || status.as_u16() == 403 {
        let error = "Token expired. Please run 'codex' to re-login.";
        log_msg(&format!("[RateLimits] auth failure: status={status}"));
        if let Err(cache_error) =
            codex_cache::invalidate(profile.route(), account_key.as_ref(), request_sequence)
        {
            log_msg(&format!(
                "[RateLimits] failed to invalidate last-good cache: {cache_error}"
            ));
        }
        return CodexRateLimits::disconnected(error);
    }

    if !status.is_success() {
        let error = format!("API error: {status}");
        log_msg(&format!("[RateLimits] non-success response: {status}"));
        return CodexRateLimits::disconnected(error);
    }

    let data = match response.json::<serde_json::Value>().await {
        Ok(data) => data,
        Err(err) => {
            let should_preserve = should_preserve_transport_failure(
                err.is_timeout(),
                err.is_connect(),
                error_is_transient(&err),
            );
            let error = if should_preserve {
                format!("Failed to read response body: {err}")
            } else {
                format!("Failed to parse response: {err}")
            };
            log_msg(&format!(
                "[RateLimits] body read failed: preservable={should_preserve}, error={error}"
            ));
            return if should_preserve {
                transient_failure_limits(account_key.as_ref(), error)
            } else {
                CodexRateLimits::disconnected(error)
            };
        }
    };

    let primary = data["rate_limit"]
        .get("primary_window")
        .and_then(parse_rate_limit_window);

    let secondary = data["rate_limit"]
        .get("secondary_window")
        .and_then(parse_rate_limit_window);

    if primary.is_none() && secondary.is_none() {
        let error = "Failed to parse response: no numeric Codex rate limit usage fields";
        log_msg(&format!("[RateLimits] {error}"));
        return CodexRateLimits::disconnected(error);
    }

    let credits = data["credits"].as_object().map(|credits| CodexCredits {
        has_credits: credits
            .get("has_credits")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        unlimited: credits
            .get("unlimited")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        balance: credits
            .get("balance")
            .and_then(|v| v.as_str())
            .map(ToString::to_string),
    });

    let limits = CodexRateLimits {
        connected: true,
        plan_type: data["plan_type"].as_str().map(ToString::to_string),
        primary,
        secondary,
        credits,
        error: None,
    };

    match account_key {
        Some(account_key) => {
            match codex_cache::store(account_key, auth_stamp, request_sequence, limits.clone()) {
                Ok(true) => {}
                Ok(false) => log_msg("[RateLimits] ignored out-of-order response"),
                Err(error) => log_msg(&format!(
                    "[RateLimits] failed to update last-good cache: {error}"
                )),
            }
        }
        None => log_msg("[RateLimits] account ID missing; last-good cache not updated"),
    }
    log_msg(&format!(
        "[RateLimits] parsed: primary_used={:?}, secondary_used={:?}",
        limits.primary.as_ref().map(|window| window.used_percent),
        limits.secondary.as_ref().map(|window| window.used_percent)
    ));
    limits
}

fn parse_reset_credit(credit: &serde_json::Value) -> Option<CodexResetCredit> {
    Some(CodexResetCredit {
        status: credit["status"].as_str()?.to_string(),
        title: credit["title"].as_str().map(ToString::to_string),
        granted_at: credit["granted_at"].as_str().map(ToString::to_string),
        expires_at: credit["expires_at"].as_str().map(ToString::to_string),
    })
}

pub async fn fetch_codex_reset_credits() -> CodexResetCredits {
    fetch_codex_reset_credits_for(&default_codex_profile()).await
}

pub(crate) async fn fetch_codex_reset_credits_for(profile: &CodexProfile) -> CodexResetCredits {
    let auth_json = match read_auth_json(profile) {
        Ok(v) => v,
        Err(error) => return CodexResetCredits::disconnected(error),
    };
    fetch_codex_reset_credits_from_auth(profile, auth_json).await
}

async fn fetch_codex_reset_credits_from_auth(
    _profile: &CodexProfile,
    auth_json: serde_json::Value,
) -> CodexResetCredits {
    let access_token = match auth_json["tokens"]["access_token"].as_str() {
        Some(token) => token,
        None => return CodexResetCredits::disconnected("No access_token found in auth.json"),
    };

    let account_id = auth_json["tokens"]["id_token"]
        .as_str()
        .and_then(decode_jwt_payload)
        .and_then(|payload| {
            payload["https://api.openai.com/auth"]["chatgpt_account_id"]
                .as_str()
                .map(ToString::to_string)
        });

    let client = shared_http_client();
    let mut request = client
        .get("https://chatgpt.com/backend-api/wham/rate-limit-reset-credits")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", "codex-cli")
        .timeout(std::time::Duration::from_secs(10));

    if let Some(account_id) = account_id {
        request = request.header("ChatGPT-Account-Id", account_id);
    }

    let response = match request.send().await {
        Ok(resp) => resp,
        Err(err) => return CodexResetCredits::disconnected(format!("Network error: {err}")),
    };

    if response.status().as_u16() == 401 || response.status().as_u16() == 403 {
        return CodexResetCredits::disconnected("Token expired. Please run 'codex' to re-login.");
    }

    if !response.status().is_success() {
        return CodexResetCredits::disconnected(format!("API error: {}", response.status()));
    }

    let data = match response.json::<serde_json::Value>().await {
        Ok(data) => data,
        Err(err) => {
            return CodexResetCredits::disconnected(format!("Failed to parse response: {err}"))
        }
    };

    let credits: Vec<CodexResetCredit> = data["credits"]
        .as_array()
        .map(|items| items.iter().filter_map(parse_reset_credit).collect())
        .unwrap_or_default();

    let available_count = data["available_count"]
        .as_u64()
        .map(|count| count.min(u32::MAX as u64) as u32)
        .unwrap_or_else(|| {
            credits
                .iter()
                .filter(|credit| credit.status == "available")
                .count() as u32
        });

    CodexResetCredits {
        connected: true,
        available_count,
        credits,
        error: None,
    }
}

/// The one credential snapshot that a batch row is allowed to use.  Keeping the
/// fan-out here makes the no-second-read rule testable without making a network
/// request: every downstream request receives a clone of this exact value.
struct BatchAuthSnapshot {
    auth_json: serde_json::Value,
    stamp: AuthFileStamp,
}

#[cfg(test)]
struct BatchSnapshotProbe {
    rotate_path: PathBuf,
    replacement: String,
    observed_id_tokens: Vec<Option<String>>,
}

#[cfg(test)]
thread_local! {
    static BATCH_SNAPSHOT_PROBE: std::cell::RefCell<Option<BatchSnapshotProbe>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn record_batch_snapshot_use(auth_json: &serde_json::Value, rotate: bool) {
    BATCH_SNAPSHOT_PROBE.with(|probe| {
        let mut probe = probe.borrow_mut();
        let Some(probe) = probe.as_mut() else {
            return;
        };
        probe.observed_id_tokens.push(
            auth_json["tokens"]["id_token"]
                .as_str()
                .map(ToString::to_string),
        );
        if rotate {
            fs::write(&probe.rotate_path, &probe.replacement).unwrap();
        }
    });
}

impl BatchAuthSnapshot {
    fn from_read(auth_json: serde_json::Value, stamp: AuthFileStamp) -> Self {
        Self { auth_json, stamp }
    }

    fn for_info(&self) -> (serde_json::Value, AuthFileStamp) {
        #[cfg(test)]
        record_batch_snapshot_use(&self.auth_json, true);
        (self.auth_json.clone(), self.stamp.clone())
    }

    fn for_rate_limits(&self) -> (serde_json::Value, AuthFileStamp) {
        #[cfg(test)]
        record_batch_snapshot_use(&self.auth_json, false);
        (self.auth_json.clone(), self.stamp.clone())
    }

    fn for_reset_credits(&self) -> serde_json::Value {
        #[cfg(test)]
        record_batch_snapshot_use(&self.auth_json, false);
        self.auth_json.clone()
    }
}

/// Sequential by design: profile reads are independent and P2 must not multiply login API traffic.
pub(crate) async fn fetch_codex_profiles(profiles: Vec<CodexProfile>) -> Vec<CodexProfileQuota> {
    let mut results = Vec::with_capacity(profiles.len());
    for profile in profiles {
        // Read one immutable credential snapshot before any awaited request. A rotating auth.json
        // can therefore never combine metadata from one account with quota from another.
        match read_auth_json_with_stamp(&profile) {
            Ok((auth_json, stamp)) => {
                let snapshot = BatchAuthSnapshot::from_read(auth_json, stamp);
                let (info_auth, info_stamp) = snapshot.for_info();
                let info = info_from_auth(&profile, info_auth, info_stamp);
                let (limits_auth, limits_stamp) = snapshot.for_rate_limits();
                let rate_limits =
                    fetch_codex_rate_limits_from_auth(&profile, limits_auth, limits_stamp).await;
                let reset_credits =
                    fetch_codex_reset_credits_from_auth(&profile, snapshot.for_reset_credits())
                        .await;
                results.push(CodexProfileQuota {
                    profile_id: profile.profile_id().to_string(),
                    account_id: info.account_id.clone(),
                    info,
                    rate_limits,
                    reset_credits,
                });
            }
            Err(error) => results.push(CodexProfileQuota::disconnected(
                profile.profile_id().to_string(),
                error.message,
            )),
        }
    }
    results
}

/// Convert P2's private result after it has completed. Never serialize the
/// private aggregate: it contains account metadata and credential route IDs.
pub(crate) async fn fetch_public_profile(
    alias: String,
    profile: CodexProfile,
) -> CodexProfilePublicQuota {
    let mut rows = fetch_codex_profiles(vec![profile]).await;
    let Some(row) = rows.pop() else {
        return CodexProfilePublicQuota::unavailable(alias);
    };
    public_profile_from_quota(alias, row)
}

fn public_profile_from_quota(alias: String, row: CodexProfileQuota) -> CodexProfilePublicQuota {
    // Only a connected quota response with retained usage windows is stale. Account
    // metadata can remain connected when a fresh rate-limit request failed, but it
    // is not itself a quota snapshot and must not be presented as one.
    let retained_quota = row.rate_limits.connected
        && (row.rate_limits.primary.is_some() || row.rate_limits.secondary.is_some());
    let status = if retained_quota && row.rate_limits.error.is_some() {
        "stale"
    } else if row.rate_limits.connected && row.rate_limits.error.is_none() {
        "connected"
    } else {
        "offline"
    };
    CodexProfilePublicQuota {
        alias,
        status: status.to_string(),
        plan_type: row.rate_limits.plan_type.or(row.info.plan_type),
        primary: row.rate_limits.primary,
        secondary: row.rate_limits.secondary,
        available_reset_credits: row.reset_credits.available_count,
        // Existing errors can include transport details. Do not relay them.
        error: (status != "connected").then_some("Profile unavailable".to_string()),
    }
}

/// Invalid descriptors are represented in place so one bad route never suppresses valid rows.
fn aliases_default_home(profile: &CodexProfile, default_home: Option<&PathBuf>) -> bool {
    profile
        .home()
        .zip(default_home)
        .is_some_and(|(home, default)| home == default)
}

pub(crate) async fn fetch_codex_profile_inputs(
    inputs: Vec<CodexProfileInput>,
) -> Vec<CodexProfileQuota> {
    use std::collections::HashSet;
    // Resolve once. A custom descriptor that names this exact directory is not
    // a second credential route; reporting it in place avoids split cache state.
    let default_home = get_codex_home().and_then(|home| home.canonicalize().ok());
    let mut routes = HashSet::new();
    let mut rows = Vec::with_capacity(inputs.len());
    for input in inputs {
        let visible_id = if input.profile_id.starts_with("codex/") {
            input.profile_id.clone()
        } else {
            "codex/invalid".to_string()
        };
        match CodexProfile::from_input(input) {
            Ok(profile) if aliases_default_home(&profile, default_home.as_ref()) => {
                rows.push(CodexProfileQuota::disconnected(
                    visible_id,
                    "Duplicate Codex credential route",
                ));
            }
            Ok(profile) if routes.insert(profile.route().clone()) => {
                rows.extend(fetch_codex_profiles(vec![profile]).await);
            }
            Ok(_) => rows.push(CodexProfileQuota::disconnected(
                visible_id,
                "Duplicate Codex credential route",
            )),
            Err(error) => rows.push(CodexProfileQuota::disconnected(visible_id, error)),
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::{
        aliases_default_home, fetch_codex_info_for, fetch_codex_profile_inputs,
        fetch_codex_profiles, parse_rate_limit_window, parse_reset_credit,
        public_profile_from_quota, read_auth_json_with_stamp, retain_last_good_info,
        should_preserve_for_status, should_preserve_transport_failure, window_minutes_from_seconds,
        AuthFileStamp, BatchSnapshotProbe, CodexData, LastGoodInfo, BATCH_SNAPSHOT_PROBE,
    };
    use crate::domain::account::{CodexProfile, CodexProfileInput, CodexProfileQuota};
    use crate::domain::models::{CodexRateLimitWindow, CodexRateLimits, CodexResetCredits};
    use base64::Engine as _;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    #[test]
    fn only_rate_limiting_is_a_preservable_http_failure() {
        assert!(should_preserve_for_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(!should_preserve_for_status(
            reqwest::StatusCode::UNAUTHORIZED
        ));
        assert!(!should_preserve_for_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    #[test]
    fn only_explicitly_transient_transport_failures_are_preservable() {
        assert!(should_preserve_transport_failure(true, false, false));
        assert!(should_preserve_transport_failure(false, true, false));
        assert!(should_preserve_transport_failure(false, false, true));
        assert!(!should_preserve_transport_failure(false, false, false));
    }

    #[test]
    fn parse_reset_credit_requires_status() {
        assert!(parse_reset_credit(&json!({ "title": "Full reset" })).is_none());
    }

    #[test]
    fn parse_reset_credit_maps_summary_fields_only() {
        let parsed = parse_reset_credit(&json!({
            "id": "credit-secret-id",
            "status": "available",
            "title": "Full reset (Weekly + 5 hr)",
            "granted_at": "2026-06-18T00:41:07.776451Z",
            "expires_at": "2026-07-18T00:41:07.776451Z"
        }));
        let credit = match parsed {
            Some(credit) => credit,
            None => panic!("credit with status should parse"),
        };

        assert_eq!(credit.status, "available");
        assert_eq!(credit.title.as_deref(), Some("Full reset (Weekly + 5 hr)"));
        assert_eq!(
            credit.granted_at.as_deref(),
            Some("2026-06-18T00:41:07.776451Z")
        );
        assert_eq!(
            credit.expires_at.as_deref(),
            Some("2026-07-18T00:41:07.776451Z")
        );
    }

    #[test]
    fn parse_rate_limit_window_requires_numeric_used_percent() {
        assert!(parse_rate_limit_window(&json!({ "limit_window_seconds": 18_000 })).is_none());
        assert!(parse_rate_limit_window(&json!({ "used_percent": "0" })).is_none());
    }

    #[test]
    fn parse_rate_limit_window_maps_numeric_used_percent() {
        let parsed = parse_rate_limit_window(&json!({
            "used_percent": 61.8,
            "limit_window_seconds": 18_000,
            "reset_at": 1_781_000_000
        }));
        let window = match parsed {
            Some(window) => window,
            None => panic!("numeric used_percent should parse"),
        };

        assert_eq!(window.used_percent, 61.8);
        assert_eq!(window.window_minutes, Some(300));
        assert_eq!(window.resets_at, Some(1_781_000_000));
    }

    #[test]
    fn window_minutes_saturate_instead_of_overflowing() {
        assert_eq!(window_minutes_from_seconds(18_000), 300);
        assert_eq!(window_minutes_from_seconds(1), 1);
        assert_eq!(window_minutes_from_seconds(i64::MAX), i64::MAX / 60);
        let parsed = parse_rate_limit_window(&json!({
            "used_percent": 10,
            "limit_window_seconds": i64::MAX
        }));
        let window = match parsed {
            Some(window) => window,
            None => panic!("max limit_window_seconds should parse"),
        };
        assert!(window.window_minutes.is_some_and(|minutes| minutes > 0));
    }

    #[test]
    fn parse_rate_limit_window_clamps_numeric_used_percent() {
        let high = match parse_rate_limit_window(&json!({ "used_percent": 120 })) {
            Some(window) => window,
            None => panic!("numeric used_percent should parse"),
        };
        let low = match parse_rate_limit_window(&json!({ "used_percent": -5 })) {
            Some(window) => window,
            None => panic!("numeric used_percent should parse"),
        };

        assert_eq!(high.used_percent, 100.0);
        assert_eq!(low.used_percent, 0.0);
    }

    fn auth_stamp(len: u64, secs: u64) -> AuthFileStamp {
        AuthFileStamp {
            len,
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
        }
    }

    fn connected_info(email: &str) -> CodexData {
        CodexData {
            connected: true,
            plan_type: Some("plus".to_string()),
            account_id: Some("acct-a".to_string()),
            subscription_until: None,
            email: Some(email.to_string()),
            error: None,
        }
    }

    fn public_quota(info_connected: bool, rate_limits: CodexRateLimits) -> CodexProfileQuota {
        CodexProfileQuota {
            profile_id: "codex/private-profile".into(),
            account_id: Some("private-account".into()),
            info: if info_connected {
                connected_info("private@example.test")
            } else {
                CodexData::disconnected("offline")
            },
            rate_limits,
            reset_credits: CodexResetCredits::disconnected("offline"),
        }
    }

    fn connected_limits(error: Option<&str>) -> CodexRateLimits {
        CodexRateLimits {
            connected: true,
            plan_type: Some("pro".into()),
            primary: Some(CodexRateLimitWindow {
                used_percent: 25.0,
                window_minutes: Some(60),
                resets_at: None,
            }),
            secondary: None,
            credits: None,
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn public_profile_status_uses_current_or_retained_rate_limits_not_info() {
        let connected =
            public_profile_from_quota("work".into(), public_quota(true, connected_limits(None)));
        assert_eq!(connected.status, "connected");
        assert_eq!(connected.error, None);

        let stale = public_profile_from_quota(
            "work".into(),
            public_quota(true, connected_limits(Some("Network error"))),
        );
        assert_eq!(stale.status, "stale");
        assert_eq!(stale.error.as_deref(), Some("Profile unavailable"));

        let no_quota_failure = public_profile_from_quota(
            "work".into(),
            public_quota(
                true,
                CodexRateLimits {
                    connected: true,
                    plan_type: Some("pro".into()),
                    primary: None,
                    secondary: None,
                    credits: None,
                    error: Some("Network error".into()),
                },
            ),
        );
        assert_eq!(no_quota_failure.status, "offline");
        assert_eq!(
            no_quota_failure.error.as_deref(),
            Some("Profile unavailable")
        );

        for error in ["Network error: timeout", "Token expired", "offline"] {
            let offline = public_profile_from_quota(
                "work".into(),
                public_quota(true, CodexRateLimits::disconnected(error)),
            );
            assert_eq!(offline.status, "offline", "{error}");
            assert_eq!(offline.error.as_deref(), Some("Profile unavailable"));
        }
    }

    #[test]
    fn last_good_codex_info_stays_on_the_same_auth_file() {
        let cached = LastGoodInfo {
            stamp: auth_stamp(128, 10),
            data: connected_info("one@example.com"),
        };
        let retained = retain_last_good_info(
            Some(&cached),
            Some(&auth_stamp(128, 10)),
            "Failed to read auth.json: Too many open files (os error 24)".to_string(),
            true,
        );
        assert!(retained.connected);
        assert_eq!(retained.email.as_deref(), Some("one@example.com"));
        assert!(retained
            .error
            .as_deref()
            .unwrap()
            .contains("Too many open files"));
    }

    #[test]
    fn last_good_codex_info_is_dropped_after_account_file_changes() {
        let cached = LastGoodInfo {
            stamp: auth_stamp(128, 10),
            data: connected_info("one@example.com"),
        };
        let switched = retain_last_good_info(
            Some(&cached),
            Some(&auth_stamp(256, 11)),
            "Failed to read auth.json: Too many open files (os error 24)".to_string(),
            true,
        );
        assert!(!switched.connected);
        assert_eq!(switched.email, None);
        assert!(switched
            .error
            .as_deref()
            .unwrap()
            .contains("Too many open files"));
    }

    fn temporary_profile(name: &str) -> (CodexProfile, PathBuf) {
        let path = std::env::temp_dir().join(format!("quotabar-p2-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        let profile = CodexProfile::from_input(CodexProfileInput {
            profile_id: format!("codex/{name}"),
            home: Some(path.clone()),
        })
        .unwrap();
        (profile, path)
    }

    fn synthetic_jwt(account: &str) -> String {
        let payload = json!({ "email": "synthetic@example.invalid", "https://api.openai.com/auth": { "chatgpt_account_id": account, "chatgpt_plan_type": "pro" } });
        let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(payload.to_string());
        format!("synthetic.{encoded}.signature")
    }

    #[test]
    fn explicit_profiles_read_independent_auth_paths_and_keep_default_route_name() {
        let (first, first_dir) = temporary_profile("first");
        let (second, second_dir) = temporary_profile("second");
        fs::write(
            first_dir.join("auth.json"),
            format!(
                r#"{{"tokens":{{"id_token":"{}"}}}}"#,
                synthetic_jwt("acct-a")
            ),
        )
        .unwrap();
        fs::write(
            second_dir.join("auth.json"),
            format!(
                r#"{{"tokens":{{"id_token":"{}"}}}}"#,
                synthetic_jwt("acct-b")
            ),
        )
        .unwrap();
        assert_eq!(first.profile_id(), "codex/first");
        assert_eq!(super::default_codex_profile().profile_id(), "codex/default");
        assert_eq!(
            super::profile_home(&super::default_codex_profile()),
            super::get_codex_home()
        );
        assert!(read_auth_json_with_stamp(&first).is_ok());
        assert!(read_auth_json_with_stamp(&second).is_ok());
        let first_info = tauri::async_runtime::block_on(fetch_codex_info_for(&first));
        let second_info = tauri::async_runtime::block_on(fetch_codex_info_for(&second));
        assert_eq!(first_info.account_id.as_deref(), Some("acct-a"));
        assert_eq!(second_info.account_id.as_deref(), Some("acct-b"));
        let _ = fs::remove_dir_all(first_dir);
        let _ = fs::remove_dir_all(second_dir);
    }

    #[test]
    fn custom_default_home_alias_is_rejected_but_a_distinct_route_is_accepted() {
        let (alias, alias_dir) = temporary_profile("default-alias");
        let (distinct, distinct_dir) = temporary_profile("distinct-route");
        let default_home = alias.home().cloned();
        assert!(aliases_default_home(&alias, default_home.as_ref()));
        assert!(!aliases_default_home(&distinct, default_home.as_ref()));
        let _ = fs::remove_dir_all(alias_dir);
        let _ = fs::remove_dir_all(distinct_dir);
    }

    #[test]
    fn malformed_one_profile_does_not_suppress_another_profiles_credentials() {
        let (good, good_dir) = temporary_profile("good");
        let (bad, bad_dir) = temporary_profile("bad");
        fs::write(
            good_dir.join("auth.json"),
            format!(
                r#"{{"tokens":{{"id_token":"{}"}}}}"#,
                synthetic_jwt("acct-good")
            ),
        )
        .unwrap();
        fs::write(bad_dir.join("auth.json"), "{").unwrap();
        assert!(read_auth_json_with_stamp(&good).is_ok());
        assert!(read_auth_json_with_stamp(&bad).is_err());
        let info = tauri::async_runtime::block_on(fetch_codex_info_for(&good));
        assert!(info.connected);
        assert_eq!(info.account_id.as_deref(), Some("acct-good"));
        let _ = fs::remove_dir_all(good_dir);
        let _ = fs::remove_dir_all(bad_dir);
    }

    #[test]
    fn batch_preserves_order_and_continues_after_invalid_missing_and_malformed_profiles() {
        let (good, good_dir) = temporary_profile("batch-good");
        fs::write(
            good_dir.join("auth.json"),
            format!(
                r#"{{"tokens":{{"id_token":"{}"}}}}"#,
                synthetic_jwt("acct-good")
            ),
        )
        .unwrap();
        let malformed_dir =
            std::env::temp_dir().join(format!("quotabar-p2-batch-bad-{}", std::process::id()));
        fs::create_dir_all(&malformed_dir).unwrap();
        fs::write(malformed_dir.join("auth.json"), "{").unwrap();
        let rows = tauri::async_runtime::block_on(fetch_codex_profile_inputs(vec![
            CodexProfileInput {
                profile_id: "bad".into(),
                home: None,
            },
            CodexProfileInput {
                profile_id: "codex/missing".into(),
                home: Some(good_dir.join("missing")),
            },
            CodexProfileInput {
                profile_id: "codex/bad".into(),
                home: Some(malformed_dir.clone()),
            },
            CodexProfileInput {
                profile_id: good.profile_id().into(),
                home: good.home().cloned(),
            },
        ]));
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].profile_id, "codex/invalid");
        assert!(!rows[0].info.connected);
        assert!(!rows[1].info.connected);
        assert!(!rows[2].info.connected);
        assert_eq!(rows[3].account_id.as_deref(), Some("acct-good"));
        let _ = fs::remove_dir_all(good_dir);
        let _ = fs::remove_dir_all(malformed_dir);
    }

    #[test]
    fn batch_snapshot_fans_one_immutable_auth_read_to_info_rate_and_reset() {
        let (profile, dir) = temporary_profile("rotation");
        let first_token = synthetic_jwt("acct-first");
        let first = serde_json::json!({
            "tokens": {
                "id_token": first_token.clone()
            }
        });
        let second = serde_json::json!({
            "tokens": {
                "id_token": synthetic_jwt("acct-second")
            }
        });
        let auth_path = dir.join("auth.json");
        fs::write(&auth_path, first.to_string()).unwrap();
        {
            BATCH_SNAPSHOT_PROBE.with(|probe| {
                *probe.borrow_mut() = Some(BatchSnapshotProbe {
                    rotate_path: auth_path.clone(),
                    replacement: second.to_string(),
                    observed_id_tokens: Vec::new(),
                });
            });
        }

        // This invokes the production batch seam. The probe rotates auth.json as
        // its first downstream consumer begins; rate and reset have no access
        // token and therefore return before any HTTP request.
        let rows = tauri::async_runtime::block_on(fetch_codex_profiles(vec![profile]));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].account_id.as_deref(), Some("acct-first"));
        let observed = BATCH_SNAPSHOT_PROBE
            .with(|probe| probe.borrow_mut().take().unwrap().observed_id_tokens);
        assert_eq!(observed.len(), 3);
        assert!(observed
            .iter()
            .all(|token| token.as_deref() == Some(first_token.as_str())));
        assert_eq!(fs::read_to_string(auth_path).unwrap(), second.to_string());
        let _ = fs::remove_dir_all(dir);
    }
}

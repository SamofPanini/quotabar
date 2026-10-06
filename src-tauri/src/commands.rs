use tauri::{AppHandle, Emitter, Manager, State};

use crate::{
    domain::account::CodexProfilesResponse,
    domain::models::{
        AntigravityData, CodexData, CodexRateLimits, CodexResetCredits, CodexWeeklyQuotaData,
        CursorData, GrokData, QuotaData,
    },
    services::{
        antigravity, claude, claude_snapshot, codex, codex_profiles, codex_weekly, cost, cursor,
        grok, link, tray, tray_icon, window, window_ping,
    },
};

#[tauri::command]
pub async fn get_quota() -> Result<QuotaData, String> {
    Ok(claude::fetch_quota().await)
}

#[tauri::command]
pub async fn refresh_claude_login() -> Result<claude::ClaudeLoginRefreshResult, String> {
    Ok(claude::refresh_claude_login().await)
}

fn profile_for_ping(
    alias: &str,
    primary_dir: &std::path::Path,
    legacy_dir: &std::path::Path,
) -> Result<crate::domain::account::CodexProfile, window_ping::PingOutcome> {
    codex_profiles::resolve_alias_with_locations(
        alias,
        primary_dir,
        legacy_dir,
        codex::get_codex_home().as_deref(),
    )
    .ok_or(window_ping::PingOutcome::ProfileUnavailable)
}

async fn resolve_and_ping_codex<F, Fut>(
    alias: &str,
    primary_dir: &std::path::Path,
    legacy_dir: &std::path::Path,
    force: bool,
    ping: F,
) -> window_ping::PingOutcome
where
    F: FnOnce(crate::domain::account::CodexProfile, bool) -> Fut,
    Fut: std::future::Future<Output = window_ping::PingOutcome>,
{
    let profile = match profile_for_ping(alias, primary_dir, legacy_dir) {
        Ok(profile) => profile,
        Err(outcome) => return outcome,
    };
    ping(profile, force).await
}

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PingConfirmationEvent {
    provider: &'static str,
    alias: String,
    outcome: window_ping::PingOutcome,
}

type ConfirmingCallback = Box<dyn FnMut(window_ping::PingOutcome) + Send>;

async fn route_ping_outcome<Run, Fut, Emit>(run: Run, emit: Emit) -> window_ping::PingOutcome
where
    Run: FnOnce(ConfirmingCallback) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = window_ping::PingOutcome> + Send + 'static,
    Emit: FnOnce(window_ping::PingOutcome) + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let sender = std::sync::Arc::new(std::sync::Mutex::new(Some(sender)));
    let sender_for_run = std::sync::Arc::clone(&sender);
    let run_task = tauri::async_runtime::spawn(async move {
        let sender_for_callback = std::sync::Arc::clone(&sender_for_run);
        let on_confirming: ConfirmingCallback = Box::new(move |outcome| {
            if let Some(sender) = sender_for_callback.lock().unwrap().take() {
                let _ = sender.send(outcome);
            }
        });
        run(on_confirming).await
    });
    tauri::async_runtime::spawn(async move {
        let outcome = match run_task.await {
            Ok(outcome) => outcome,
            Err(_) => window_ping::PingOutcome::CliFailed {
                code: "confirmationAborted",
            },
        };
        if let Some(sender) = sender.lock().unwrap().take() {
            let _ = sender.send(outcome);
        } else {
            emit(outcome);
        }
    });
    match tauri::async_runtime::spawn_blocking(move || receiver.recv()).await {
        Ok(Ok(outcome)) => outcome,
        _ => window_ping::PingOutcome::CliFailed {
            code: "spawnFailed",
        },
    }
}

#[tauri::command]
pub async fn ping_codex_window(
    app: AppHandle,
    alias: String,
    force: bool,
) -> Result<window_ping::PingOutcome, String> {
    let legacy_dir = app
        .path()
        .app_config_dir()
        .map_err(|_| "Profile configuration is unavailable")?;
    let primary_dir = crate::services::state_location::primary_state_dir()
        .map_err(|_| "Profile configuration is unavailable")?;
    let event_app = app.clone();
    let event_alias = alias.clone();
    Ok(route_ping_outcome(
        move |mut on_confirming| async move {
            resolve_and_ping_codex(
                &alias,
                &primary_dir,
                &legacy_dir,
                force,
                move |profile, force| async move {
                    window_ping::ping_codex(profile, force, &mut *on_confirming).await
                },
            )
            .await
        },
        move |outcome| {
            let _ = event_app.emit(
                "ping-confirmation",
                PingConfirmationEvent {
                    provider: "codex",
                    alias: event_alias,
                    outcome,
                },
            );
        },
    )
    .await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> std::path::PathBuf {
        // Canonicalize first: macOS temp dirs sit under the /var -> /private/var
        // symlink, and the profile registry reader rejects symlinked ancestors.
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "quotabar-ping-command-{}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn ping_codex_window_profile_resolution_returns_profile_unavailable_for_unknown_and_invalid() {
        let root = temp_root();
        fs::write(
            root.join(codex_profiles::CONFIG_FILE),
            r#"{"version":1,"profiles":[{"alias":"broken"}]}"#,
        )
        .unwrap();
        assert!(matches!(
            profile_for_ping("unknown", &root, &root),
            Err(window_ping::PingOutcome::ProfileUnavailable)
        ));
        assert!(matches!(
            profile_for_ping("broken", &root, &root),
            Err(window_ping::PingOutcome::ProfileUnavailable)
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ping_codex_window_resolves_alias_before_calling_ping() {
        let root = temp_root();
        let work_home = root.join("work-home");
        fs::create_dir_all(&work_home).unwrap();
        fs::write(
            root.join(codex_profiles::CONFIG_FILE),
            serde_json::json!({
                "version": 1,
                "profiles": [
                    { "alias": "work", "home": work_home },
                    { "alias": "broken" },
                ],
            })
            .to_string(),
        )
        .unwrap();

        for alias in ["unknown", "broken", "contains/path"] {
            let calls = Arc::new(AtomicUsize::new(0));
            let ping_calls = Arc::clone(&calls);
            let outcome = tauri::async_runtime::block_on(resolve_and_ping_codex(
                alias,
                &root,
                &root,
                false,
                move |_, _| async move {
                    ping_calls.fetch_add(1, Ordering::Relaxed);
                    window_ping::PingOutcome::Busy
                },
            ));
            assert_eq!(outcome, window_ping::PingOutcome::ProfileUnavailable);
            assert_eq!(calls.load(Ordering::Relaxed), 0, "{alias}");
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let seen_alias = Arc::new(Mutex::new(None));
        let ping_calls = Arc::clone(&calls);
        let ping_alias = Arc::clone(&seen_alias);
        let outcome = tauri::async_runtime::block_on(resolve_and_ping_codex(
            "work",
            &root,
            &root,
            true,
            move |profile, force| async move {
                ping_calls.fetch_add(1, Ordering::Relaxed);
                *ping_alias.lock().unwrap() = Some(profile.profile_id().to_string());
                assert!(force);
                window_ping::PingOutcome::Busy
            },
        ));
        assert_eq!(outcome, window_ping::PingOutcome::Busy);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(seen_alias.lock().unwrap().as_deref(), Some("codex/work"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn route_ping_outcome_returns_confirming_then_emits_final_outcome() {
        let (event_sender, event_receiver) = std::sync::mpsc::sync_channel(1);
        let command_outcome = tauri::async_runtime::block_on(route_ping_outcome(
            |mut on_confirming| async move {
                on_confirming(window_ping::PingOutcome::Confirming {
                    tokens: Some(3),
                    expected_resets_at: 20_000,
                });
                window_ping::PingOutcome::Opened {
                    resets_at: 20_000,
                    tokens: Some(3),
                    confirmed_after_secs: 30,
                }
            },
            move |outcome| event_sender.send(outcome).unwrap(),
        ));

        assert_eq!(
            command_outcome,
            window_ping::PingOutcome::Confirming {
                tokens: Some(3),
                expected_resets_at: 20_000,
            },
        );
        assert_eq!(
            event_receiver.recv().unwrap(),
            window_ping::PingOutcome::Opened {
                resets_at: 20_000,
                tokens: Some(3),
                confirmed_after_secs: 30,
            },
        );
    }

    #[test]
    fn route_ping_outcome_returns_final_outcome_without_event_when_not_confirming() {
        let event_count = Arc::new(AtomicUsize::new(0));
        let emitted = Arc::clone(&event_count);
        let command_outcome = tauri::async_runtime::block_on(route_ping_outcome(
            |_| async { window_ping::PingOutcome::Busy },
            move |_| {
                emitted.fetch_add(1, Ordering::Relaxed);
            },
        ));

        assert_eq!(command_outcome, window_ping::PingOutcome::Busy);
        assert_eq!(event_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn route_ping_outcome_emits_aborted_failure_after_confirming_run_panics() {
        let (event_sender, event_receiver) = std::sync::mpsc::sync_channel(1);
        let command_outcome = tauri::async_runtime::block_on(route_ping_outcome(
            |mut on_confirming| async move {
                on_confirming(window_ping::PingOutcome::Confirming {
                    tokens: Some(3),
                    expected_resets_at: 20_000,
                });
                panic!("test confirming run panic");
            },
            move |outcome| event_sender.send(outcome).unwrap(),
        ));

        assert_eq!(
            command_outcome,
            window_ping::PingOutcome::Confirming {
                tokens: Some(3),
                expected_resets_at: 20_000,
            },
        );
        assert_eq!(
            event_receiver.recv().unwrap(),
            window_ping::PingOutcome::CliFailed {
                code: "confirmationAborted",
            },
        );
    }

    #[test]
    fn ping_confirmation_event_serializes_only_the_public_fields() {
        let event = PingConfirmationEvent {
            provider: "codex",
            alias: "work".to_string(),
            outcome: window_ping::PingOutcome::Confirming {
                tokens: Some(3),
                expected_resets_at: 20_000,
            },
        };
        let value = serde_json::to_value(event).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 3);
        assert!(object.contains_key("provider"));
        assert!(object.contains_key("alias"));
        assert!(object.contains_key("outcome"));
        assert_eq!(value["provider"], "codex");
        assert_eq!(value["alias"], "work");
        assert_eq!(value["outcome"]["kind"], "confirming");
    }
}

#[tauri::command]
pub async fn ping_claude_window(
    app: AppHandle,
    force: bool,
) -> Result<window_ping::PingOutcome, String> {
    Ok(route_ping_outcome(
        move |mut on_confirming| async move {
            window_ping::ping_claude(force, &mut *on_confirming).await
        },
        move |outcome| {
            let _ = app.emit(
                "ping-confirmation",
                PingConfirmationEvent {
                    provider: "claude",
                    alias: "default".to_string(),
                    outcome,
                },
            );
        },
    )
    .await)
}

/// Read-only Claude current-state projection. No frontend mutation command exists.
#[tauri::command]
pub fn get_claude_current_snapshots(
    app: AppHandle,
) -> Result<claude_snapshot::ClaudeCurrentSnapshotsDto, String> {
    let legacy_config_dir = app
        .path()
        .app_config_dir()
        .map_err(|_| "Claude snapshot unavailable")?;
    let primary_config_dir = crate::services::state_location::primary_state_dir()
        .map_err(|_| "Claude snapshot unavailable")?;
    claude_snapshot::ClaudeSnapshotStore::project_from_locations(
        primary_config_dir.join("claude-current-state"),
        legacy_config_dir.join("claude-current-state"),
        chrono::Utc::now(),
    )
    .map_err(|_| "Claude snapshot unavailable".to_string())
}

#[tauri::command]
pub async fn get_codex_info() -> Result<CodexData, String> {
    Ok(codex::fetch_codex_info().await)
}

#[tauri::command]
pub async fn get_codex_rate_limits() -> Result<CodexRateLimits, String> {
    Ok(codex::fetch_codex_rate_limits().await)
}

#[tauri::command]
pub async fn get_codex_reset_credits() -> Result<CodexResetCredits, String> {
    Ok(codex::fetch_codex_reset_credits().await)
}

/// Reads the QuotaBar-owned registry on every existing refresh invocation.
/// The webview cannot provide profile descriptors or credential paths.
#[tauri::command]
pub async fn get_codex_profiles(app: AppHandle) -> Result<CodexProfilesResponse, String> {
    let legacy_dir = app
        .path()
        .app_config_dir()
        .map_err(|_| "Profile configuration is unavailable")?;
    let primary_dir = crate::services::state_location::primary_state_dir()
        .map_err(|_| "Profile configuration is unavailable")?;
    Ok(codex_profiles::fetch_from_locations(
        &primary_dir,
        &legacy_dir,
        codex::get_codex_home().as_deref(),
    )
    .await)
}

#[tauri::command]
pub async fn get_codex_weekly_quota() -> Result<CodexWeeklyQuotaData, String> {
    let Some(codex_home) = codex::get_codex_home() else {
        return Ok(CodexWeeklyQuotaData::unavailable(
            "Could not find the Codex home directory",
        ));
    };
    let fetched_official = codex::fetch_codex_rate_limits().await;
    let official = if fetched_official.error.is_none() {
        codex_weekly::OfficialWeeklySnapshot::from_limits(&fetched_official, chrono::Utc::now())
    } else {
        None
    };
    let data = tauri::async_runtime::spawn_blocking(move || {
        let quota =
            ccstats::load_codex_weekly_quota(Some(&codex_home)).map_err(|error| error.to_string());
        let value_estimate = match fetched_official.error {
            Some(error) => Err(error),
            None => codex_weekly::estimate_codex_weekly_value(&codex_home, official.as_ref()),
        };
        CodexWeeklyQuotaData::from_results(quota, value_estimate)
    })
    .await
    .map_err(|err| format!("Codex weekly quota task failed: {err}"))?;
    Ok(data)
}

#[tauri::command]
pub async fn get_cursor_info() -> Result<CursorData, String> {
    Ok(cursor::fetch_cursor_info().await)
}

#[tauri::command]
pub async fn get_antigravity_info() -> Result<AntigravityData, String> {
    Ok(antigravity::fetch_antigravity_info().await)
}

#[tauri::command]
pub async fn get_grok_info() -> Result<GrokData, String> {
    Ok(grok::fetch_grok_info().await)
}

#[tauri::command]
pub async fn get_cost_overview(
    source: String,
    currency: Option<String>,
    timezone: Option<String>,
    force: Option<bool>,
) -> Result<cost::CostOverview, String> {
    cost::get_cost_overview(source, currency, timezone, force).await
}

#[tauri::command]
pub async fn get_cost_daily(
    source: String,
    days: u32,
    currency: Option<String>,
    timezone: Option<String>,
    force: Option<bool>,
) -> Result<cost::CostDailySeries, String> {
    cost::get_cost_daily(source, days, currency, timezone, force).await
}

#[tauri::command]
pub fn open_claude_dashboard() -> Result<(), String> {
    link::open_claude_dashboard()
}

#[tauri::command]
pub fn open_codex_dashboard() -> Result<(), String> {
    link::open_codex_dashboard()
}

#[tauri::command]
pub fn open_cursor_dashboard() -> Result<(), String> {
    link::open_cursor_dashboard()
}

#[tauri::command]
pub fn open_antigravity_dashboard() -> Result<(), String> {
    link::open_antigravity_dashboard()
}

#[tauri::command]
pub fn open_grok_dashboard() -> Result<(), String> {
    link::open_grok_dashboard()
}

#[tauri::command]
pub async fn resize_window(app: AppHandle, height: f64) -> Result<(), String> {
    window::resize_window(app, height).await
}

#[tauri::command]
pub async fn set_dock_visibility(app: AppHandle, visible: bool) -> Result<(), String> {
    window::set_dock_visibility(app, visible).await
}

#[tauri::command]
pub async fn update_tray_icon(
    app: AppHandle,
    tray_state: State<'_, tray::TrayState>,
    service: tray::TrayService,
    percentage: Option<u8>,
    visible: bool,
    force: Option<bool>,
    style: Option<tray_icon::TrayIconStyle>,
) -> Result<(), String> {
    tray::update_tray_icon(
        app,
        tray_state,
        service,
        percentage,
        visible,
        force.unwrap_or(false),
        style,
    )
    .await
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

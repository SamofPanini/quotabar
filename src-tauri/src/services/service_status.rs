//! Public service-status polling.  This module deliberately has no account or
//! credential inputs: requests only go to the two public status pages.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

use super::{
    http::shared_http_client,
    tray::{self, TrayService, TrayState},
};

const CLAUDE_SUMMARY_URL: &str = "https://status.claude.com/api/v2/summary.json";
const OPENAI_SUMMARY_URL: &str = "https://status.openai.com/api/v2/summary.json";
const OPENAI_PROXY_URL: &str = "https://status.openai.com/proxy/status.openai.com";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceLevel {
    Operational,
    Maintenance,
    Degraded,
    PartialOutage,
    MajorOutage,
    Unknown,
}

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAlert {
    #[default]
    None,
    Warning,
    Critical,
}

impl From<bool> for ServiceAlert {
    fn from(incident: bool) -> Self {
        if incident {
            Self::Critical
        } else {
            Self::None
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceComponent {
    pub name: String,
    pub status: String,
    pub tier: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceUpdate {
    pub id: String,
    pub status: String,
    pub body: String,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceEvent {
    pub id: String,
    pub name: String,
    pub status: String,
    pub impact: Option<String>,
    pub url: Option<String>,
    pub latest_update: Option<ServiceUpdate>,
    pub started_at: Option<String>,
    pub affected_components: Vec<String>,
    pub alert: ServiceAlert,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub provider: String,
    pub level: ServiceLevel,
    pub components: Vec<ServiceComponent>,
    pub incidents: Vec<ServiceEvent>,
    pub maintenances: Vec<ServiceEvent>,
    pub fetched_at: Option<String>,
    pub error: Option<String>,
    pub alert: ServiceAlert,
}

impl ServiceStatus {
    fn unavailable(provider: &str, error: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            level: ServiceLevel::Unknown,
            components: vec![],
            incidents: vec![],
            maintenances: vec![],
            fetched_at: None,
            error: Some(error.into()),
            alert: ServiceAlert::None,
        }
    }
    fn is_incident(&self) -> bool {
        matches!(
            self.level,
            ServiceLevel::Degraded | ServiceLevel::PartialOutage | ServiceLevel::MajorOutage
        ) || !self.incidents.is_empty()
    }
    fn is_active(&self) -> bool {
        self.is_incident()
            || self.level == ServiceLevel::Maintenance
            || !self.maintenances.is_empty()
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatusSnapshot {
    pub claude: ServiceStatus,
    pub codex: ServiceStatus,
}

#[derive(Clone)]
pub struct ServiceStatusState {
    inner: Arc<Mutex<Runtime>>,
}

#[derive(Clone)]
struct Runtime {
    enabled: bool,
    notify: bool,
    poll_epoch: u64,
    polling: bool,
    claude: ServiceStatus,
    codex: ServiceStatus,
    failures: [u8; 2],
    next_due: [std::time::Instant; 2],
    last_success: [Option<std::time::Instant>; 2],
    notifications: [NotificationMemory; 2],
}

#[derive(Clone, Default)]
struct NotificationMemory {
    fingerprint: Option<String>,
    event_fingerprints: Vec<String>,
    was_active: bool,
    last_sent: Option<std::time::Instant>,
    pending: Option<PendingNotification>,
    pending_in_flight: bool,
}
#[derive(Clone)]
struct PendingNotification {
    fingerprint: String,
    title: String,
    body: String,
}

impl Default for ServiceStatusState {
    fn default() -> Self {
        let now = std::time::Instant::now();
        Self {
            inner: Arc::new(Mutex::new(Runtime {
                enabled: true,
                notify: true,
                poll_epoch: 0,
                polling: false,
                claude: ServiceStatus::unavailable("claude", "Status has not been fetched."),
                codex: ServiceStatus::unavailable("codex", "Status has not been fetched."),
                failures: [0; 2],
                next_due: [now; 2],
                last_success: [None; 2],
                notifications: [NotificationMemory::default(), NotificationMemory::default()],
            })),
        }
    }
}

impl ServiceStatusState {
    pub fn snapshot(&self) -> ServiceStatusSnapshot {
        let runtime = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ServiceStatusSnapshot {
            claude: runtime.claude.clone(),
            codex: runtime.codex.clone(),
        }
    }
    pub fn set_prefs(&self, enabled: bool, notify: bool) -> bool {
        let mut runtime = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let became_enabled = enabled && !runtime.enabled;
        runtime.poll_epoch = runtime.poll_epoch.saturating_add(1);
        runtime.enabled = enabled;
        runtime.notify = notify;
        runtime.next_due = [std::time::Instant::now(); 2];
        for memory in &mut runtime.notifications {
            memory.pending_in_flight = false;
        }
        if !enabled {
            runtime.claude =
                ServiceStatus::unavailable("claude", "Service status checking is disabled.");
            runtime.codex =
                ServiceStatus::unavailable("codex", "Service status checking is disabled.");
            runtime.failures = [0; 2];
        }
        became_enabled
    }
    pub fn incident_flags(&self) -> [ServiceAlert; 2] {
        let runtime = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        [runtime.claude.alert, runtime.codex.alert]
    }
    fn enabled(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enabled
    }
    fn is_current_epoch(&self, epoch: u64) -> bool {
        let runtime = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.enabled && runtime.poll_epoch == epoch
    }
    fn next_delay(&self) -> Duration {
        let runtime = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let failures = runtime.failures.into_iter().max().unwrap_or(0);
        if failures > 0 {
            return Duration::from_secs(
                [60, 120, 300][usize::from(failures.saturating_sub(1).min(2))],
            );
        }
        if runtime.claude.is_active() || runtime.codex.is_active() {
            Duration::from_secs(60)
        } else {
            Duration::from_secs(300)
        }
    }
    fn next_due_delay(&self) -> Duration {
        let runtime = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !runtime.enabled {
            return Duration::from_secs(15);
        }
        runtime
            .next_due
            .into_iter()
            .map(|due| due.saturating_duration_since(std::time::Instant::now()))
            .min()
            .unwrap_or(Duration::from_secs(15))
            .max(Duration::from_secs(1))
            .min(Duration::from_secs(15))
    }
}

pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        sleep_without_blocking_runtime(Duration::from_secs(10)).await;
        loop {
            let state = app.state::<ServiceStatusState>();
            if state.enabled() {
                poll_once(&app).await;
            }
            sleep_without_blocking_runtime(app.state::<ServiceStatusState>().next_due_delay())
                .await;
        }
    });
}

pub async fn poll_once(app: &AppHandle) {
    let state = app.state::<ServiceStatusState>();
    let Some(outcome) = poll_once_with_fetchers(&state, fetch_claude, fetch_openai).await else {
        return;
    };
    let tray_state = app.state::<TrayState>();
    finish_poll_side_effects(
        &state,
        outcome,
        |index, alert| match index {
            0 => tray::set_service_alert(app, &tray_state, TrayService::Claude, alert),
            1 => tray::set_service_alert(app, &tray_state, TrayService::Codex, alert),
            _ => unreachable!("service status only has two providers"),
        },
        |snapshot| {
            let _ = app.emit("service-status-changed", snapshot);
        },
        |notification| {
            app.notification()
                .builder()
                .title(&notification.title)
                .body(&notification.body)
                .show()
                .map_err(|error| error.to_string())
        },
    );
}

struct PollOutcome {
    changed: bool,
    snapshot: ServiceStatusSnapshot,
    notifications: Vec<NotificationDecision>,
    epoch: u64,
}

/// Runs every effect after a poll commit.  The final epoch check deliberately
/// compensates any just-applied stale badges/event with the current state.
fn finish_poll_side_effects<SetIncident, EmitSnapshot, ShowNotification>(
    state: &ServiceStatusState,
    outcome: PollOutcome,
    mut set_incident: SetIncident,
    mut emit_snapshot: EmitSnapshot,
    mut show_notification: ShowNotification,
) where
    SetIncident: FnMut(usize, ServiceAlert),
    EmitSnapshot: FnMut(ServiceStatusSnapshot),
    ShowNotification: FnMut(&NotificationDecision) -> Result<(), String>,
{
    let PollOutcome {
        changed,
        snapshot,
        notifications,
        epoch,
    } = outcome;
    if !state.is_current_epoch(epoch) {
        mark_notifications_failed(state, &notifications);
        return;
    }

    set_incident(0, snapshot.claude.alert);
    set_incident(1, snapshot.codex.alert);
    if changed {
        emit_snapshot(snapshot.clone());
    }
    if !state.is_current_epoch(epoch) {
        mark_notifications_failed(state, &notifications);
        compensate_stale_poll_effects(state, &mut set_incident, &mut emit_snapshot);
        return;
    }

    for (position, notification) in notifications.iter().enumerate() {
        if !state.is_current_epoch(epoch) {
            mark_notifications_failed(state, &notifications[position..]);
            compensate_stale_poll_effects(state, &mut set_incident, &mut emit_snapshot);
            return;
        }
        match show_notification(notification) {
            Ok(()) => {
                mark_notification_delivered(state, notification.index, &notification.fingerprint)
            }
            Err(error) => {
                mark_notification_failed(state, notification.index, &notification.fingerprint);
                eprintln!("[ServiceStatus] notification delivery failed: {error}");
            }
        }
    }
    if !state.is_current_epoch(epoch) {
        compensate_stale_poll_effects(state, &mut set_incident, &mut emit_snapshot);
    }
}

fn mark_notifications_failed(state: &ServiceStatusState, notifications: &[NotificationDecision]) {
    for notification in notifications {
        mark_notification_failed(state, notification.index, &notification.fingerprint);
    }
}

fn compensate_stale_poll_effects<SetIncident, EmitSnapshot>(
    state: &ServiceStatusState,
    set_incident: &mut SetIncident,
    emit_snapshot: &mut EmitSnapshot,
) where
    SetIncident: FnMut(usize, ServiceAlert),
    EmitSnapshot: FnMut(ServiceStatusSnapshot),
{
    for (index, incident) in state.incident_flags().into_iter().enumerate() {
        set_incident(index, incident);
    }
    emit_snapshot(state.snapshot());
}

/// The production polling single-step with injectable fetchers. Keeping the
/// preference checks here makes it possible to prove that an in-flight request
/// cannot commit state or schedule follow-on side effects after being disabled.
async fn poll_once_with_fetchers<ClaudeFetcher, OpenAiFetcher, ClaudeFuture, OpenAiFuture>(
    state: &ServiceStatusState,
    fetch_claude: ClaudeFetcher,
    fetch_openai: OpenAiFetcher,
) -> Option<PollOutcome>
where
    ClaudeFetcher: FnOnce() -> ClaudeFuture,
    OpenAiFetcher: FnOnce() -> OpenAiFuture,
    ClaudeFuture: Future<Output = ServiceStatus>,
    OpenAiFuture: Future<Output = ServiceStatus>,
{
    poll_once_with_fetchers_at(state, fetch_claude, fetch_openai, std::time::Instant::now()).await
}

async fn poll_once_with_fetchers_at<ClaudeFetcher, OpenAiFetcher, ClaudeFuture, OpenAiFuture>(
    state: &ServiceStatusState,
    fetch_claude: ClaudeFetcher,
    fetch_openai: OpenAiFetcher,
    now: std::time::Instant,
) -> Option<PollOutcome>
where
    ClaudeFetcher: FnOnce() -> ClaudeFuture,
    OpenAiFetcher: FnOnce() -> OpenAiFuture,
    ClaudeFuture: Future<Output = ServiceStatus>,
    OpenAiFuture: Future<Output = ServiceStatus>,
{
    let (epoch, due, mut claude, mut codex) = {
        let mut runtime = state
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !runtime.enabled || runtime.polling {
            return None;
        }
        let due = [runtime.next_due[0] <= now, runtime.next_due[1] <= now];
        if !due[0] && !due[1] {
            return None;
        }
        runtime.polling = true;
        (
            runtime.poll_epoch,
            due,
            runtime.claude.clone(),
            runtime.codex.clone(),
        )
    };
    if due[0] {
        claude = fetch_claude().await;
    }
    // Do not even begin the second provider request after a disable/re-enable.
    if !state.is_current_epoch(epoch) {
        state
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .polling = false;
        return None;
    }
    if due[1] {
        codex = fetch_openai().await;
    }
    let mut runtime = state
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !runtime.enabled || runtime.poll_epoch != epoch {
        runtime.polling = false;
        return None;
    }
    for (index, result) in [&claude, &codex]
        .into_iter()
        .enumerate()
        .filter(|(index, _)| due[*index])
    {
        if result.error.is_none() {
            runtime.failures[index] = 0;
            runtime.last_success[index] = Some(std::time::Instant::now());
        } else {
            runtime.failures[index] = runtime.failures[index].saturating_add(1);
        }
        runtime.next_due[index] = now + provider_delay(result, runtime.failures[index]);
    }
    mark_stale_failed_statuses(&runtime, &mut claude, &mut codex);
    let changed = runtime.claude != claude || runtime.codex != codex;
    runtime.claude = claude;
    runtime.codex = codex;
    let snapshot = ServiceStatusSnapshot {
        claude: runtime.claude.clone(),
        codex: runtime.codex.clone(),
    };
    let notifications = notification_decisions(&mut runtime, &snapshot);
    runtime.polling = false;
    Some(PollOutcome {
        changed,
        snapshot,
        notifications,
        epoch,
    })
}

fn provider_delay(status: &ServiceStatus, failures: u8) -> Duration {
    if failures > 0 {
        Duration::from_secs([60, 120, 300][usize::from(failures.saturating_sub(1).min(2))])
    } else if status.is_active() {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(300)
    }
}

fn mark_stale_failed_statuses(
    runtime: &Runtime,
    claude: &mut ServiceStatus,
    codex: &mut ServiceStatus,
) {
    if claude.error.is_some()
        && runtime.last_success[0].is_some_and(|at| at.elapsed() > Duration::from_secs(30 * 60))
    {
        *claude = ServiceStatus::unavailable("claude", "No successful status fetch in 30 minutes.");
    }
    if codex.error.is_some()
        && runtime.last_success[1].is_some_and(|at| at.elapsed() > Duration::from_secs(30 * 60))
    {
        *codex = ServiceStatus::unavailable("codex", "No successful status fetch in 30 minutes.");
    }
}

async fn sleep_without_blocking_runtime(delay: Duration) {
    let _ = tauri::async_runtime::spawn_blocking(move || std::thread::sleep(delay)).await;
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NotificationDecision {
    index: usize,
    fingerprint: String,
    title: String,
    body: String,
}

fn notification_decisions(
    runtime: &mut Runtime,
    snapshot: &ServiceStatusSnapshot,
) -> Vec<NotificationDecision> {
    notification_decisions_at(runtime, snapshot, std::time::Instant::now())
}

fn notification_decisions_at(
    runtime: &mut Runtime,
    snapshot: &ServiceStatusSnapshot,
    now: std::time::Instant,
) -> Vec<NotificationDecision> {
    if !runtime.notify {
        return vec![];
    }
    let mut output = Vec::new();
    for (index, status) in [&snapshot.claude, &snapshot.codex].into_iter().enumerate() {
        if status.level == ServiceLevel::Unknown {
            continue;
        }
        let active = status.is_active();
        let fingerprint = notification_fingerprint(status);
        let provider = if index == 0 { "Anthropic" } else { "OpenAI" };
        let memory = &mut runtime.notifications[index];
        let pending = if active
            && (!memory.was_active || memory.fingerprint.as_ref() != Some(&fingerprint))
        {
            let title = format!("{provider}: {}", notification_level(status));
            Some(PendingNotification {
                fingerprint: fingerprint.clone(),
                title,
                body: notification_body(changed_event(status, &memory.event_fingerprints)),
            })
        } else if !active && memory.was_active {
            Some(PendingNotification {
                fingerprint: fingerprint.clone(),
                title: format!("{provider}: resolved"),
                body: "All watched services are operational.".into(),
            })
        } else {
            None
        };
        memory.was_active = active;
        memory.fingerprint = Some(fingerprint);
        memory.event_fingerprints = event_fingerprints(status);
        if let Some(next) = pending {
            memory.pending = Some(next);
            memory.pending_in_flight = false;
        }
        if let Some(next) = memory.pending.clone() {
            if !memory.pending_in_flight
                && memory
                    .last_sent
                    .is_none_or(|time| now.duration_since(time) >= Duration::from_secs(60))
            {
                memory.pending_in_flight = true;
                output.push(NotificationDecision {
                    index,
                    fingerprint: next.fingerprint,
                    title: next.title,
                    body: next.body,
                });
            }
        }
    }
    output
}

fn mark_notification_delivered(state: &ServiceStatusState, index: usize, fingerprint: &str) {
    let mut runtime = state
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    mark_notification_delivered_matching_at(
        &mut runtime,
        index,
        fingerprint,
        std::time::Instant::now(),
    );
}
fn mark_notification_delivered_matching_at(
    runtime: &mut Runtime,
    index: usize,
    fingerprint: &str,
    now: std::time::Instant,
) {
    let memory = &mut runtime.notifications[index];
    if memory
        .pending
        .as_ref()
        .is_some_and(|pending| pending.fingerprint == fingerprint)
    {
        memory.last_sent = Some(now);
        memory.pending = None;
        memory.pending_in_flight = false;
    }
}
fn mark_notification_delivered_at(runtime: &mut Runtime, index: usize, now: std::time::Instant) {
    let Some(fingerprint) = runtime.notifications[index]
        .pending
        .as_ref()
        .map(|pending| pending.fingerprint.clone())
    else {
        return;
    };
    mark_notification_delivered_matching_at(runtime, index, &fingerprint, now);
}
fn mark_notification_failed(state: &ServiceStatusState, index: usize, fingerprint: &str) {
    let mut runtime = state
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let memory = &mut runtime.notifications[index];
    if memory
        .pending
        .as_ref()
        .is_some_and(|pending| pending.fingerprint == fingerprint)
    {
        memory.pending_in_flight = false;
    }
}

fn notification_fingerprint(status: &ServiceStatus) -> String {
    let mut parts = vec![format!("{:?}", status.level)];
    for item in status.incidents.iter().chain(status.maintenances.iter()) {
        parts.push(format!(
            "{}:{}",
            item.id,
            item.latest_update
                .as_ref()
                .map(|u| u.id.as_str())
                .unwrap_or("")
        ));
    }
    parts.sort();
    parts.join("|")
}
fn notification_level(status: &ServiceStatus) -> &'static str {
    match status.level {
        ServiceLevel::Maintenance => "maintenance",
        ServiceLevel::Degraded => "degraded",
        ServiceLevel::PartialOutage => "partial outage",
        ServiceLevel::MajorOutage => "major outage",
        _ if !status.maintenances.is_empty() => "maintenance",
        _ => "degraded",
    }
}
fn event_fingerprints(status: &ServiceStatus) -> Vec<String> {
    status
        .incidents
        .iter()
        .chain(status.maintenances.iter())
        .map(|event| {
            format!(
                "{}:{}",
                event.id,
                event
                    .latest_update
                    .as_ref()
                    .map(|u| u.id.as_str())
                    .unwrap_or("")
            )
        })
        .collect()
}

fn changed_event<'a>(status: &'a ServiceStatus, previous: &[String]) -> Option<&'a ServiceEvent> {
    status
        .incidents
        .iter()
        .chain(status.maintenances.iter())
        .find(|event| {
            let fingerprint = format!(
                "{}:{}",
                event.id,
                event
                    .latest_update
                    .as_ref()
                    .map(|u| u.id.as_str())
                    .unwrap_or("")
            );
            !previous.contains(&fingerprint)
        })
        .or_else(|| {
            status
                .incidents
                .first()
                .or_else(|| status.maintenances.first())
        })
}
fn notification_body(item: Option<&ServiceEvent>) -> String {
    item.map(|event| match &event.latest_update {
        Some(update) => format!(
            "{}{} — {}: {}",
            event.name,
            if event.affected_components.is_empty() {
                String::new()
            } else {
                format!(" ({})", event.affected_components.join(", "))
            },
            title_case(&update.status),
            update.body
        ),
        None => event.name.clone(),
    })
    .unwrap_or_else(|| "A watched service is affected.".into())
}
fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

async fn fetch_claude() -> ServiceStatus {
    match fetch_json(CLAUDE_SUMMARY_URL)
        .await
        .and_then(|json| parse_statuspage("claude", &json, watched_claude))
    {
        Ok(status) => status,
        Err(error) => ServiceStatus::unavailable("claude", error),
    }
}
async fn fetch_openai() -> ServiceStatus {
    let v2 = match fetch_json(OPENAI_SUMMARY_URL).await {
        Ok(value) => value,
        Err(error) => return ServiceStatus::unavailable("codex", error),
    };
    let proxy = match fetch_json(OPENAI_PROXY_URL).await {
        Ok(value) => Some(value),
        Err(error) => {
            eprintln!("[ServiceStatus] OpenAI supplemental status unavailable: {error}");
            None
        }
    };
    match parse_openai(&v2, proxy.as_ref()) {
        Ok(status) => status,
        Err(error) => ServiceStatus::unavailable("codex", error),
    }
}

async fn fetch_json(url: &str) -> Result<Value, String> {
    let response = shared_http_client()
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err("Status response exceeds 1 MB.".into());
    }
    let mut body = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        append_response_chunk(&mut body, &chunk)?;
    }
    parse_json_body(&body)
}
fn append_response_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), String> {
    if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
        return Err("Status response exceeds 1 MB.".into());
    }
    body.extend_from_slice(chunk);
    Ok(())
}
fn parse_json_body(body: &[u8]) -> Result<Value, String> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err("Status response exceeds 1 MB.".into());
    }
    serde_json::from_slice(body).map_err(|error| format!("Invalid status response: {error}"))
}

fn watched_claude(component: &Component) -> bool {
    [
        "Claude API",
        "Claude Code",
        "Claude Console",
        "Claude Cowork",
        "claude.ai",
    ]
    .iter()
    .any(|prefix| component.name.starts_with(prefix))
}
fn watched_codex(component: &Component) -> bool {
    matches!(
        component.name.as_str(),
        "Responses" | "Login" | "Codex in ChatGPT Desktop"
    )
}

fn component_tier(
    provider: &str,
    component: &Component,
    codex_core_ids: &HashSet<String>,
) -> &'static str {
    match provider {
        "claude"
            if component.name.starts_with("claude.ai")
                || component.name.starts_with("Claude Code") =>
        {
            "core"
        }
        "claude" => "other",
        "codex"
            if codex_core_ids.contains(&component.id)
                || component.name == "Codex in ChatGPT Desktop" =>
        {
            "core"
        }
        _ => "other",
    }
}

#[derive(Deserialize)]
struct Summary {
    #[serde(default)]
    components: Vec<Component>,
    #[serde(default)]
    incidents: Vec<RawEvent>,
    #[serde(default)]
    scheduled_maintenances: Vec<RawEvent>,
}
#[derive(Clone, Deserialize)]
struct Component {
    id: String,
    name: String,
    #[serde(default = "operational_status")]
    status: String,
}
fn operational_status() -> String {
    "operational".into()
}
#[derive(Clone, Deserialize)]
struct RawEvent {
    id: String,
    name: String,
    #[serde(default)]
    status: String,
    impact: Option<String>,
    #[serde(default)]
    shortlink: Option<String>,
    #[serde(default)]
    scheduled_for: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    components: Vec<Component>,
    #[serde(default)]
    incident_updates: Vec<RawUpdate>,
}

struct RelevantEvent {
    event: ServiceEvent,
    affected_component_ids: HashSet<String>,
}
#[derive(Clone, Deserialize)]
struct RawUpdate {
    id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    body: String,
    updated_at: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    display_at: Option<String>,
}

fn parse_statuspage(
    provider: &str,
    value: &Value,
    watched: fn(&Component) -> bool,
) -> Result<ServiceStatus, String> {
    let summary: Summary = serde_json::from_value(value.clone())
        .map_err(|error| format!("Invalid summary shape: {error}"))?;
    let watched_ids: HashSet<String> = summary
        .components
        .iter()
        .filter(|component| watched(component))
        .map(|component| component.id.clone())
        .collect();
    if watched_ids.is_empty() {
        return Err("Summary did not contain watched components.".into());
    }
    let watched_refs: HashSet<&str> = watched_ids.iter().map(String::as_str).collect();
    Ok(build_status(
        provider,
        summary.components.into_iter().filter(watched).collect(),
        summary.incidents,
        summary.scheduled_maintenances,
        &watched_refs,
        &HashSet::new(),
    ))
}

fn parse_openai(v2: &Value, proxy: Option<&Value>) -> Result<ServiceStatus, String> {
    let summary: Summary = serde_json::from_value(v2.clone())
        .map_err(|error| format!("Invalid OpenAI summary shape: {error}"))?;
    let v2_names: HashMap<String, String> = summary
        .components
        .iter()
        .map(|component| (component.id.clone(), component.name.clone()))
        .collect();
    let mut watched_ids: HashSet<String> = summary
        .components
        .iter()
        .filter(|component| watched_codex(component))
        .map(|component| component.id.clone())
        .collect();
    let mut components: Vec<Component> = summary
        .components
        .iter()
        .filter(|component| watched_codex(component))
        .cloned()
        .collect();
    let mut incidents = summary.incidents;
    let mut maintenance = summary.scheduled_maintenances;
    let mut codex_core_ids = HashSet::new();
    if let Some(proxy) = proxy {
        match parse_openai_supplement(proxy, &v2_names) {
            Ok((
                supplemental_components,
                supplemental_incidents,
                supplemental_maintenance,
                proxy_core_ids,
            )) => {
                for component in supplemental_components {
                    watched_ids.insert(component.id.clone());
                    if !components
                        .iter()
                        .any(|existing| existing.id == component.id)
                    {
                        components.push(component);
                    }
                }
                codex_core_ids = proxy_core_ids;
                let retained_proxy_ids: HashSet<_> = supplemental_incidents
                    .iter()
                    .filter(|event| {
                        event
                            .components
                            .iter()
                            .any(|component| watched_ids.contains(&component.id))
                    })
                    .map(|event| event.id.as_str())
                    .collect();
                incidents.retain(|event| !retained_proxy_ids.contains(event.id.as_str()));
                incidents.extend(supplemental_incidents);
                maintenance.extend(supplemental_maintenance);
            }
            Err(error) => {
                eprintln!("[ServiceStatus] OpenAI supplemental malformed ({error}); using v2 only")
            }
        }
    }
    let ids: HashSet<&str> = watched_ids.iter().map(String::as_str).collect();
    Ok(build_status(
        "codex",
        components,
        incidents,
        maintenance,
        &ids,
        &codex_core_ids,
    ))
}

fn parse_openai_supplement(
    proxy: &Value,
    v2_names: &HashMap<String, String>,
) -> Result<
    (
        Vec<Component>,
        Vec<RawEvent>,
        Vec<RawEvent>,
        HashSet<String>,
    ),
    String,
> {
    let items = proxy
        .pointer("/summary/structure/items")
        .and_then(Value::as_array)
        .ok_or("missing structure items")?;
    let affected = proxy
        .pointer("/summary/affected_components")
        .and_then(Value::as_array)
        .ok_or("missing affected_components")?;
    let mut names = v2_names.clone();
    let mut core_ids = HashSet::new();
    let mut watched_ids = HashSet::new();
    for item in items {
        let group = item.get("group");
        let is_codex = group
            .and_then(|group| group.get("name"))
            .and_then(Value::as_str)
            == Some("Codex");
        let list = group
            .and_then(|group| group.get("components"))
            .and_then(Value::as_array)
            .or_else(|| item.get("components").and_then(Value::as_array));
        if let Some(list) = list {
            for component in list {
                insert_proxy_component(
                    component,
                    is_codex,
                    &mut names,
                    &mut core_ids,
                    &mut watched_ids,
                );
            }
        } else if let Some(component) = item.get("component") {
            insert_proxy_component(
                component,
                false,
                &mut names,
                &mut core_ids,
                &mut watched_ids,
            );
        }
    }
    if core_ids.is_empty() {
        return Err("missing Codex group".into());
    }
    let mut components = Vec::new();
    let mut ordered_ids: Vec<&String> = watched_ids.iter().collect();
    ordered_ids.sort();
    for id in ordered_ids {
        let name = names
            .get(id)
            .cloned()
            .unwrap_or_else(|| "Unknown component".into());
        let matching = affected.iter().find(|item| component_matches(item, id));
        let status = matching
            .and_then(status_from_value)
            .unwrap_or("operational");
        components.push(Component {
            id: id.clone(),
            name,
            status: status.into(),
        });
    }
    let ongoing = proxy
        .pointer("/summary/ongoing_incidents")
        .and_then(Value::as_array)
        .ok_or("missing ongoing_incidents")?;
    let mut incidents = Vec::new();
    for item in ongoing {
        incidents.push(proxy_event(item, &names).ok_or("malformed ongoing_incidents item")?);
    }
    let mut maintenance = Vec::new();
    for item in proxy
        .pointer("/summary/scheduled_maintenances")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match proxy_event(item, &names) {
            Some(event) => maintenance.push(event),
            None => {
                eprintln!("[ServiceStatus] OpenAI supplemental maintenance malformed; skipping")
            }
        }
    }
    Ok((components, incidents, maintenance, core_ids))
}

fn insert_proxy_component(
    component: &Value,
    is_codex: bool,
    names: &mut HashMap<String, String>,
    core_ids: &mut HashSet<String>,
    watched_ids: &mut HashSet<String>,
) {
    let Some(id) = component
        .get("component_id")
        .or_else(|| component.get("id"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let name = component.get("name").and_then(Value::as_str);
    if let Some(name) = name {
        names.insert(id.to_string(), name.to_string());
    }
    if is_codex {
        core_ids.insert(id.to_string());
        watched_ids.insert(id.to_string());
    }
    if matches!(
        name,
        Some("Responses" | "Login" | "Codex in ChatGPT Desktop")
    ) {
        watched_ids.insert(id.to_string());
    }
}

fn proxy_event(item: &Value, names: &HashMap<String, String>) -> Option<RawEvent> {
    let id = item.get("id")?.as_str()?.to_string();
    let name = item.get("name")?.as_str()?.to_string();
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let ids: HashSet<String> = ["affected_components", "component_impacts"]
        .into_iter()
        .flat_map(|key| {
            item.get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|part| {
            part.get("component_id")
                .or_else(|| part.get("id"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
        .collect();
    let components = ids
        .into_iter()
        .map(|id| Component {
            name: names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| "Unknown component".into()),
            id,
            status: "operational".into(),
        })
        .collect();
    let incident_updates = item
        .get("updates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|update| {
            Some(RawUpdate {
                id: update.get("id")?.as_str()?.into(),
                status: update
                    .get("to_status")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                body: update
                    .get("message_string")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                updated_at: update
                    .get("published_at")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                created_at: None,
                display_at: None,
            })
        })
        .collect();
    Some(RawEvent {
        id: id.clone(),
        name,
        status,
        impact: None,
        shortlink: Some(format!("https://status.openai.com/incidents/{id}")),
        scheduled_for: None,
        started_at: item
            .get("published_at")
            .and_then(Value::as_str)
            .map(str::to_string),
        created_at: None,
        components,
        incident_updates,
    })
}

fn component_matches(value: &Value, id: &str) -> bool {
    value.get("component_id").and_then(Value::as_str) == Some(id)
        || value.get("id").and_then(Value::as_str) == Some(id)
}
fn status_from_value(value: &Value) -> Option<&str> {
    ["status", "component_status"]
        .iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
}

fn build_status(
    provider: &str,
    components: Vec<Component>,
    incidents: Vec<RawEvent>,
    maintenance: Vec<RawEvent>,
    watched_ids: &HashSet<&str>,
    codex_core_ids: &HashSet<String>,
) -> ServiceStatus {
    let component_by_id: HashMap<String, Component> = components
        .iter()
        .cloned()
        .map(|component| (component.id.clone(), component))
        .collect();
    let relevant = |event: RawEvent, unresolved: bool| -> Option<RelevantEvent> {
        if unresolved && matches!(event.status.as_str(), "resolved" | "postmortem") {
            return None;
        }
        if !event
            .components
            .iter()
            .any(|component| watched_ids.contains(component.id.as_str()))
        {
            return None;
        }
        let affected_component_ids: HashSet<String> = event
            .components
            .iter()
            .filter(|component| watched_ids.contains(component.id.as_str()))
            .map(|component| component.id.clone())
            .collect();
        if affected_component_ids.is_empty() {
            return None;
        }
        let mut affected: Vec<(bool, String)> = affected_component_ids
            .iter()
            .map(|id| {
                let item = component_by_id.get(id);
                (
                    item.is_some_and(|item| {
                        component_tier(provider, item, codex_core_ids) == "core"
                    }),
                    item.map(|item| item.name.clone())
                        .unwrap_or_else(|| "Unknown component".into()),
                )
            })
            .collect();
        // Core services first, then by name, so titles and snapshots stay stable.
        affected.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        let affected_components: Vec<String> = affected.into_iter().map(|(_, name)| name).collect();
        let core = affected_component_ids.iter().any(|id| {
            component_by_id
                .get(id)
                .is_some_and(|item| component_tier(provider, item, codex_core_ids) == "core")
        });
        let alert = if !unresolved {
            ServiceAlert::None
        } else if core && event.status != "monitoring" {
            ServiceAlert::Critical
        } else {
            ServiceAlert::Warning
        };
        let latest_update = event
            .incident_updates
            .into_iter()
            .max_by(
                |left, right| match (raw_update_instant(left), raw_update_instant(right)) {
                    (Some(left), Some(right)) => left.cmp(&right),
                    (Some(_), None) => std::cmp::Ordering::Greater,
                    (None, Some(_)) => std::cmp::Ordering::Less,
                    (None, None) => std::cmp::Ordering::Equal,
                },
            )
            .map(|update| ServiceUpdate {
                id: update.id,
                status: update.status,
                body: truncate(&update.body, 140),
                updated_at: update
                    .display_at
                    .or(update.created_at)
                    .or(update.updated_at),
            });
        Some(RelevantEvent {
            event: ServiceEvent {
                id: event.id,
                name: event.name,
                status: event.status,
                impact: event.impact,
                url: event.shortlink,
                latest_update,
                started_at: event.started_at.or(event.created_at),
                affected_components,
                alert,
            },
            affected_component_ids,
        })
    };
    let incidents: Vec<_> = incidents
        .into_iter()
        .filter_map(|event| relevant(event, true))
        .collect();
    let maintenances: Vec<_> = maintenance
        .into_iter()
        .filter(maintenance_is_in_progress)
        .filter_map(|event| relevant(event, false))
        .collect();
    let mut level = components
        .iter()
        .map(|component| level_for_status(&component.status))
        .max_by_key(level_rank)
        .unwrap_or(ServiceLevel::Unknown);
    if !incidents.is_empty() && level_rank(&level) < level_rank(&ServiceLevel::Degraded) {
        level = ServiceLevel::Degraded;
    } else if !maintenances.is_empty() && level == ServiceLevel::Operational {
        level = ServiceLevel::Maintenance;
    }
    let incident_alert = incidents
        .iter()
        .map(|event| event.event.alert)
        .max()
        .unwrap_or(ServiceAlert::None);
    let component_alert = components
        .iter()
        .filter_map(|component| {
            let abnormal = matches!(
                component.status.as_str(),
                "degraded_performance" | "partial_outage" | "major_outage" | "full_outage"
            );
            if !abnormal {
                return None;
            }
            if component_tier(provider, component, codex_core_ids) == "other" {
                return Some(ServiceAlert::Warning);
            }
            let covering: Vec<_> = incidents
                .iter()
                .filter(|event| event.affected_component_ids.contains(&component.id))
                .collect();
            Some(
                if !covering.is_empty()
                    && covering
                        .iter()
                        .all(|event| event.event.status == "monitoring")
                {
                    ServiceAlert::Warning
                } else {
                    ServiceAlert::Critical
                },
            )
        })
        .max()
        .unwrap_or(ServiceAlert::None);
    ServiceStatus {
        provider: provider.into(),
        level,
        components: components
            .into_iter()
            .map(|component| ServiceComponent {
                tier: component_tier(provider, &component, codex_core_ids).into(),
                name: component.name,
                status: component.status,
            })
            .collect(),
        incidents: incidents.into_iter().map(|event| event.event).collect(),
        maintenances: maintenances.into_iter().map(|event| event.event).collect(),
        fetched_at: Some(Utc::now().to_rfc3339()),
        error: None,
        alert: incident_alert.max(component_alert),
    }
}
fn raw_update_instant(update: &RawUpdate) -> Option<chrono::DateTime<Utc>> {
    update
        .display_at
        .as_deref()
        .or(update.created_at.as_deref())
        .or(update.updated_at.as_deref())
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|time| time.with_timezone(&Utc))
}
fn maintenance_is_in_progress(event: &RawEvent) -> bool {
    !matches!(event.status.as_str(), "completed" | "resolved")
        && !(event.status == "scheduled" && event_is_in_future(event))
}

fn event_is_in_future(event: &RawEvent) -> bool {
    event
        .scheduled_for
        .as_deref()
        .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
        .is_some_and(|time| time.with_timezone(&Utc) > Utc::now())
}
fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.into()
    } else {
        value
            .chars()
            .take(limit.saturating_sub(1))
            .collect::<String>()
            + "…"
    }
}
fn level_for_status(status: &str) -> ServiceLevel {
    match status {
        "operational" => ServiceLevel::Operational,
        "under_maintenance" => ServiceLevel::Maintenance,
        "degraded_performance" => ServiceLevel::Degraded,
        "partial_outage" => ServiceLevel::PartialOutage,
        "major_outage" | "full_outage" => ServiceLevel::MajorOutage,
        _ => ServiceLevel::Unknown,
    }
}
fn level_rank(level: &ServiceLevel) -> u8 {
    match level {
        ServiceLevel::Operational => 0,
        ServiceLevel::Maintenance => 1,
        ServiceLevel::Degraded => 2,
        ServiceLevel::PartialOutage => 3,
        ServiceLevel::MajorOutage => 4,
        ServiceLevel::Unknown => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn json(source: &str) -> Value {
        serde_json::from_str(source).unwrap()
    }
    #[test]
    fn claude_fixture_is_degraded_and_relevant() {
        let status = parse_statuspage("claude", &json(include_str!("../../tests/fixtures/service_status/claude_summary_2026-10-09_console_degraded.json")), watched_claude).unwrap();
        assert_eq!(status.level, ServiceLevel::Degraded);
        assert_eq!(status.incidents.len(), 1);
        assert!(status
            .components
            .iter()
            .any(|component| component.name.contains("Console")));
    }

    #[test]
    fn operational_components_with_fixture_monitoring_incident_stay_degraded_and_maintenance_stays_unbadged(
    ) {
        let mut monitoring = json(include_str!(
            "../../tests/fixtures/service_status/claude_summary_2026-10-09_console_degraded.json"
        ));
        for component in monitoring["components"].as_array_mut().unwrap() {
            component["status"] = Value::String("operational".into());
        }
        let monitoring_status = parse_statuspage("claude", &monitoring, watched_claude).unwrap();
        assert!(monitoring_status
            .components
            .iter()
            .all(|component| component.status == "operational"));
        assert_eq!(monitoring_status.level, ServiceLevel::Degraded);
        assert!(monitoring_status.is_incident());

        let mut maintenance = monitoring;
        maintenance["scheduled_maintenances"] = maintenance["incidents"].clone();
        maintenance["incidents"] = Value::Array(vec![]);
        maintenance["scheduled_maintenances"][0]["status"] = Value::String("in_progress".into());
        let maintenance_status = parse_statuspage("claude", &maintenance, watched_claude).unwrap();
        assert_eq!(maintenance_status.level, ServiceLevel::Maintenance);
        assert!(!maintenance_status.is_incident());
    }
    #[test]
    fn openai_fixtures_find_all_codex_components() {
        let status = parse_openai(
            &json(include_str!(
                "../../tests/fixtures/service_status/openai_v2_summary_2026-10-09_operational.json"
            )),
            Some(&json(include_str!(
                "../../tests/fixtures/service_status/openai_proxy_2026-10-09_operational.json"
            ))),
        )
        .unwrap();
        assert_eq!(status.level, ServiceLevel::Operational);
        assert_eq!(
            status
                .components
                .iter()
                .filter(
                    |component| ["Codex Web", "Codex API", "CLI", "VS Code extension"]
                        .contains(&component.name.as_str())
                )
                .count(),
            4
        );
    }
    #[test]
    fn unknown_and_full_outage_are_not_operational() {
        assert_eq!(level_for_status("future_state"), ServiceLevel::Unknown);
        assert_eq!(level_for_status("full_outage"), ServiceLevel::MajorOutage);
    }
    #[test]
    fn malformed_proxy_falls_back_to_v2() {
        let status = parse_openai(
            &json(include_str!(
                "../../tests/fixtures/service_status/openai_v2_summary_2026-10-09_operational.json"
            )),
            Some(&json("{}")),
        )
        .unwrap();
        assert_eq!(status.level, ServiceLevel::Operational);
    }
    #[test]
    fn response_limit_is_enforced() {
        assert!(parse_json_body(&vec![b' '; MAX_RESPONSE_BYTES + 1]).is_err());
    }

    #[test]
    fn streaming_response_limit_rejects_a_late_oversized_chunk() {
        let mut body = vec![b' '; MAX_RESPONSE_BYTES - 2];
        assert!(append_response_chunk(&mut body, b"123").is_err());
        let source = include_str!("service_status.rs");
        let start = source
            .find("async fn fetch_json")
            .expect("fetch_json production function");
        let end = source[start..]
            .find("\nfn append_response_chunk")
            .expect("fetch_json closing boundary")
            + start;
        let fetch_json = &source[start..end];
        assert!(fetch_json.contains("response.chunk().await"));
        assert!(!fetch_json.contains("response.bytes().await"));
    }

    #[test]
    fn verifying_maintenance_is_active_but_completed_resolved_and_future_scheduled_are_filtered() {
        let watched: HashSet<&str> = ["api"].into_iter().collect();
        let event = |status: &str| RawEvent {
            id: status.into(),
            name: status.into(),
            status: status.into(),
            impact: None,
            shortlink: None,
            scheduled_for: (status == "scheduled").then(|| "2999-01-01T00:00:00Z".into()),
            started_at: None,
            created_at: None,
            components: vec![Component {
                id: "api".into(),
                name: "Claude API".into(),
                status: "operational".into(),
            }],
            incident_updates: vec![],
        };
        let status = build_status(
            "claude",
            vec![Component {
                id: "api".into(),
                name: "Claude API".into(),
                status: "under_maintenance".into(),
            }],
            vec![],
            vec![
                event("completed"),
                event("resolved"),
                event("scheduled"),
                event("verifying"),
                event("in_progress"),
            ],
            &watched,
            &HashSet::new(),
        );
        assert!(status.is_active());
        assert_eq!(status.maintenances.len(), 2);
        assert!(status
            .maintenances
            .iter()
            .any(|event| event.status == "verifying"));
    }

    #[test]
    fn malformed_supplemental_affected_components_discards_all_supplemental_data() {
        let v2 = json(include_str!(
            "../../tests/fixtures/service_status/openai_v2_summary_2026-10-09_operational.json"
        ));
        let malformed = json(
            r#"{"summary":{"structure":{"items":[{"group":{"name":"Codex","components":[{"component_id":"extra","name":"Extra"}]}}]},"affected_components":{},"ongoing_incidents":[],"scheduled_maintenances":[]}}"#,
        );
        let status = parse_openai(&v2, Some(&malformed)).unwrap();
        assert!(!status
            .components
            .iter()
            .any(|component| component.name == "Extra"));
    }

    #[test]
    fn malformed_supplemental_event_discards_all_supplemental_data() {
        let v2 = json(include_str!(
            "../../tests/fixtures/service_status/openai_v2_summary_2026-10-09_operational.json"
        ));
        let malformed = json(
            r#"{"summary":{"structure":{"items":[{"group":{"name":"Codex","components":[{"component_id":"extra","name":"Extra"}]}}]},"affected_components":[],"ongoing_incidents":[{}],"scheduled_maintenances":[]}}"#,
        );
        let status = parse_openai(&v2, Some(&malformed)).unwrap();
        assert!(!status
            .components
            .iter()
            .any(|component| component.name == "Extra"));
    }

    #[test]
    fn disabling_during_second_provider_fetch_is_rejected_at_commit() {
        let state = ServiceStatusState::default();
        let close_during_fetch = state.clone();
        let outcome = tauri::async_runtime::block_on(poll_once_with_fetchers(
            &state,
            || async { active_status(ServiceLevel::Degraded, "u1") },
            move || async move {
                close_during_fetch.set_prefs(false, true);
                operational_status("codex")
            },
        ));
        assert!(outcome.is_none());
        assert_eq!(state.snapshot().claude.level, ServiceLevel::Unknown);
        assert_eq!(
            state.incident_flags(),
            [ServiceAlert::None, ServiceAlert::None]
        );
    }

    #[test]
    fn reopening_during_second_provider_fetch_drops_the_old_epoch_at_commit() {
        let state = ServiceStatusState::default();
        let changing_prefs = state.clone();
        let outcome = tauri::async_runtime::block_on(poll_once_with_fetchers(
            &state,
            || async { active_status(ServiceLevel::Degraded, "old") },
            move || async move {
                changing_prefs.set_prefs(false, true);
                changing_prefs.set_prefs(true, true);
                operational_status("codex")
            },
        ));
        assert!(outcome.is_none());
        assert_eq!(state.snapshot().claude.level, ServiceLevel::Unknown);
    }

    #[test]
    fn production_poll_schedules_each_provider_from_its_own_failure_count() {
        let state = ServiceStatusState::default();
        let now = std::time::Instant::now();
        {
            let mut runtime = state.inner.lock().unwrap();
            runtime.failures = [2, 0];
            runtime.next_due = [now, now];
        }
        let outcome = tauri::async_runtime::block_on(poll_once_with_fetchers_at(
            &state,
            || async { ServiceStatus::unavailable("claude", "claude down") },
            || async { ServiceStatus::unavailable("codex", "openai down") },
            now,
        ));
        assert!(outcome.is_some());
        let runtime = state.inner.lock().unwrap();
        assert_eq!(runtime.failures, [3, 1]);
        assert_eq!(runtime.next_due[0], now + Duration::from_secs(300));
        assert_eq!(runtime.next_due[1], now + Duration::from_secs(60));
    }

    #[test]
    fn provider_failures_back_off_independently() {
        let state = ServiceStatusState::default();
        let now = std::time::Instant::now();
        {
            let mut runtime = state.inner.lock().unwrap();
            runtime.failures = [3, 1];
            runtime.next_due = [
                now + Duration::from_secs(300),
                now + Duration::from_secs(60),
            ];
        }
        assert!(state.next_due_delay() <= Duration::from_secs(60));
        let claude_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let codex_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let claude_count = claude_calls.clone();
        let codex_count = codex_calls.clone();
        let result = tauri::async_runtime::block_on(poll_once_with_fetchers_at(
            &state,
            move || async move {
                claude_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                operational_status("claude")
            },
            move || async move {
                codex_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                operational_status("codex")
            },
            now + Duration::from_secs(60),
        ));
        assert!(result.is_some());
        assert_eq!(claude_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(codex_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn next_due_delay_sleeps_while_disabled_and_is_bounded_while_enabled() {
        let state = ServiceStatusState::default();
        state.set_prefs(false, true);
        assert_eq!(state.next_due_delay(), Duration::from_secs(15));

        let now = std::time::Instant::now();
        {
            let mut runtime = state.inner.lock().unwrap();
            runtime.enabled = true;
            runtime.next_due = [now - Duration::from_secs(1); 2];
        }
        assert_eq!(state.next_due_delay(), Duration::from_secs(1));
        state.inner.lock().unwrap().next_due = [now + Duration::from_secs(300); 2];
        assert_eq!(state.next_due_delay(), Duration::from_secs(15));
    }

    #[test]
    fn notification_retry_is_retained_until_delivery_is_recorded() {
        let start = std::time::Instant::now();
        let mut runtime = fresh_runtime();
        let snapshot = notification_snapshot(active_status(ServiceLevel::Degraded, "u1"));
        assert_eq!(
            notification_decisions_at(&mut runtime, &snapshot, start).len(),
            1
        );
        assert!(runtime.notifications[0].pending.is_some());
        assert!(runtime.notifications[0].last_sent.is_none());
    }

    #[test]
    fn stale_epoch_before_notification_releases_in_flight_and_retries_after_reenable() {
        let state = ServiceStatusState::default();
        let outcome = tauri::async_runtime::block_on(poll_once_with_fetchers(
            &state,
            || async { active_status(ServiceLevel::Degraded, "same-update") },
            || async { operational_status("codex") },
        ))
        .unwrap();
        assert_eq!(outcome.notifications.len(), 1);
        state.inner.lock().unwrap().poll_epoch += 1;
        finish_poll_side_effects(
            &state,
            outcome,
            |_, _| {},
            |_| {},
            |_| -> Result<(), String> { panic!("stale notification must not be shown") },
        );
        assert!(!state.inner.lock().unwrap().notifications[0].pending_in_flight);

        state.set_prefs(false, true);
        state.set_prefs(true, true);
        let retry = tauri::async_runtime::block_on(poll_once_with_fetchers(
            &state,
            || async { active_status(ServiceLevel::Degraded, "same-update") },
            || async { operational_status("codex") },
        ))
        .unwrap();
        assert_eq!(retry.notifications.len(), 1);
    }

    #[test]
    fn set_prefs_releases_both_provider_pending_notifications() {
        let state = ServiceStatusState::default();
        {
            let mut runtime = state.inner.lock().unwrap();
            runtime.notifications[0].pending_in_flight = true;
            runtime.notifications[1].pending_in_flight = true;
        }
        state.set_prefs(false, true);
        let runtime = state.inner.lock().unwrap();
        assert!(!runtime.notifications[0].pending_in_flight);
        assert!(!runtime.notifications[1].pending_in_flight);
    }

    #[test]
    fn notification_loop_epoch_change_releases_each_unshown_decision() {
        use std::cell::Cell;

        let state = ServiceStatusState::default();
        let outcome = tauri::async_runtime::block_on(poll_once_with_fetchers(
            &state,
            || async { active_status(ServiceLevel::Degraded, "claude-update") },
            || async {
                let mut codex = active_status(ServiceLevel::Degraded, "codex-update");
                codex.provider = "codex".into();
                codex
            },
        ))
        .unwrap();
        assert_eq!(outcome.notifications.len(), 2);
        let shown = Cell::new(0);
        let changing_state = state.clone();
        finish_poll_side_effects(
            &state,
            outcome,
            |_, _| {},
            |_| {},
            |notification| {
                shown.set(shown.get() + 1);
                if notification.index == 0 {
                    changing_state.inner.lock().unwrap().poll_epoch += 1;
                }
                Ok(())
            },
        );
        assert_eq!(shown.get(), 1);
        let runtime = state.inner.lock().unwrap();
        assert!(runtime.notifications[1].pending.is_some());
        assert!(!runtime.notifications[1].pending_in_flight);
    }

    #[test]
    fn closing_after_the_first_tray_effect_compensates_badges_and_emits_current_snapshot() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let state = ServiceStatusState::default();
        let outcome = tauri::async_runtime::block_on(poll_once_with_fetchers(
            &state,
            || async { active_status(ServiceLevel::Degraded, "incident") },
            || async { operational_status("codex") },
        ))
        .unwrap();
        let tray_calls = Rc::new(RefCell::new(Vec::new()));
        let emitted = Rc::new(RefCell::new(Vec::new()));
        let notification_calls = Rc::new(RefCell::new(0));
        let closing_state = state.clone();
        let tray_calls_for_effect = tray_calls.clone();
        let emitted_for_effect = emitted.clone();
        let notification_calls_for_effect = notification_calls.clone();
        finish_poll_side_effects(
            &state,
            outcome,
            move |index, incident| {
                tray_calls_for_effect.borrow_mut().push((index, incident));
                if index == 0 {
                    closing_state.set_prefs(false, true);
                }
            },
            move |snapshot| emitted_for_effect.borrow_mut().push(snapshot),
            move |_| {
                *notification_calls_for_effect.borrow_mut() += 1;
                Ok(())
            },
        );
        assert_eq!(
            &tray_calls.borrow()[tray_calls.borrow().len() - 2..],
            &[(0, ServiceAlert::None), (1, ServiceAlert::None)]
        );
        let last_snapshot = emitted.borrow().last().cloned().unwrap();
        assert_eq!(last_snapshot.claude.level, ServiceLevel::Unknown);
        assert_eq!(last_snapshot.codex.level, ServiceLevel::Unknown);
        assert_eq!(*notification_calls.borrow(), 0);
        assert_eq!(
            state.incident_flags(),
            [ServiceAlert::None, ServiceAlert::None]
        );
    }

    #[test]
    fn failed_notification_retries_and_old_success_cannot_clear_new_pending() {
        let state = ServiceStatusState::default();
        let now = std::time::Instant::now();
        let mut runtime = state.inner.lock().unwrap();
        let first = notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::Degraded, "u1")),
            now,
        );
        let first_fingerprint = first[0].fingerprint.clone();
        drop(runtime);
        mark_notification_failed(&state, 0, &first_fingerprint);
        let mut runtime = state.inner.lock().unwrap();
        let retry = notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::Degraded, "u1")),
            now + Duration::from_secs(1),
        );
        assert_eq!(retry.len(), 1);
        let _new = notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::PartialOutage, "u2")),
            now + Duration::from_secs(2),
        );
        mark_notification_delivered_matching_at(
            &mut runtime,
            0,
            &first_fingerprint,
            now + Duration::from_secs(2),
        );
        assert_eq!(
            runtime.notifications[0]
                .pending
                .as_ref()
                .map(|item| item.fingerprint.as_str()),
            Some(
                notification_fingerprint(&active_status(ServiceLevel::PartialOutage, "u2"))
                    .as_str()
            )
        );
    }

    #[test]
    fn missing_summary_fields_fail_closed_and_non_watched_events_do_not_alert() {
        assert!(parse_statuspage("claude", &json("{}"), watched_claude).is_err());
        let summary = json(
            r#"{"components":[{"id":"images","name":"Images","status":"operational"},{"id":"api","name":"Claude API","status":"operational"}],"incidents":[{"id":"i","name":"Images only","status":"investigating","components":[{"id":"images","name":"Images","status":"partial_outage"}]}]}"#,
        );
        let status = parse_statuspage("claude", &summary, watched_claude).unwrap();
        assert_eq!(status.level, ServiceLevel::Operational);
        assert!(status.incidents.is_empty());
    }

    #[test]
    fn status_mapping_covers_maintenance_partial_and_unknown() {
        assert_eq!(
            level_for_status("under_maintenance"),
            ServiceLevel::Maintenance
        );
        assert_eq!(
            level_for_status("partial_outage"),
            ServiceLevel::PartialOutage
        );
        assert_eq!(level_for_status("unannounced"), ServiceLevel::Unknown);
    }

    #[test]
    fn polling_intervals_follow_health_failure_and_recovery() {
        let state = ServiceStatusState::default();
        assert_eq!(state.next_delay(), Duration::from_secs(300));
        {
            let mut runtime = state.inner.lock().unwrap();
            runtime.claude.level = ServiceLevel::Degraded;
        }
        assert_eq!(state.next_delay(), Duration::from_secs(60));
        {
            let mut runtime = state.inner.lock().unwrap();
            runtime.claude.level = ServiceLevel::Operational;
            runtime.failures = [1, 0];
        }
        assert_eq!(state.next_delay(), Duration::from_secs(60));
        {
            let mut runtime = state.inner.lock().unwrap();
            runtime.failures = [2, 0];
        }
        assert_eq!(state.next_delay(), Duration::from_secs(120));
        {
            let mut runtime = state.inner.lock().unwrap();
            runtime.failures = [3, 0];
        }
        assert_eq!(state.next_delay(), Duration::from_secs(300));
    }

    fn operational_status(provider: &str) -> ServiceStatus {
        ServiceStatus {
            provider: provider.into(),
            level: ServiceLevel::Operational,
            components: vec![],
            incidents: vec![],
            maintenances: vec![],
            fetched_at: None,
            error: None,
            alert: ServiceAlert::None,
        }
    }

    #[test]
    fn injected_production_poll_step_uses_incident_and_healthy_intervals() {
        let state = ServiceStatusState::default();
        let now = std::time::Instant::now();
        state.inner.lock().unwrap().next_due = [now; 2];
        let incident = active_status(ServiceLevel::Degraded, "u1");
        let first = tauri::async_runtime::block_on(poll_once_with_fetchers_at(
            &state,
            || async move { incident },
            || async { operational_status("codex") },
            now,
        ));
        assert!(first.is_some());
        assert_eq!(state.next_delay(), Duration::from_secs(60));

        let recovered = tauri::async_runtime::block_on(poll_once_with_fetchers_at(
            &state,
            || async { operational_status("claude") },
            || async { operational_status("codex") },
            now + Duration::from_secs(60),
        ));
        assert!(recovered.is_some());
        assert_eq!(state.next_delay(), Duration::from_secs(300));
    }

    #[test]
    fn disabling_during_in_flight_production_poll_drops_state_notifications_and_badges() {
        let state = ServiceStatusState::default();
        let close_during_fetch = state.clone();
        let openai_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let openai_count = openai_calls.clone();
        let outcome = tauri::async_runtime::block_on(poll_once_with_fetchers(
            &state,
            move || async move {
                close_during_fetch.set_prefs(false, true);
                active_status(ServiceLevel::Degraded, "u1")
            },
            move || async move {
                openai_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                operational_status("codex")
            },
        ));
        // `None` is the only path that reaches neither poll_once's tray/event
        // work nor its notification delivery loop.
        assert!(outcome.is_none());
        let snapshot = state.snapshot();
        assert_eq!(snapshot.claude.level, ServiceLevel::Unknown);
        assert_eq!(
            state.incident_flags(),
            [ServiceAlert::None, ServiceAlert::None]
        );
        assert_eq!(openai_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn closing_and_reopening_during_fetch_drops_the_old_epoch() {
        let state = ServiceStatusState::default();
        let changing_prefs = state.clone();
        let result = tauri::async_runtime::block_on(poll_once_with_fetchers(
            &state,
            move || async move {
                changing_prefs.set_prefs(false, true);
                changing_prefs.set_prefs(true, true);
                active_status(ServiceLevel::Degraded, "old")
            },
            || async { operational_status("codex") },
        ));
        assert!(result.is_none());
        assert_eq!(state.snapshot().claude.level, ServiceLevel::Unknown);
    }

    #[test]
    fn concurrent_poll_is_single_flight() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;
        let state = ServiceStatusState::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_state = state.clone();
        let first_calls = calls.clone();
        let first = std::thread::spawn(move || {
            tauri::async_runtime::block_on(poll_once_with_fetchers(
                &first_state,
                move || async move {
                    first_calls.fetch_add(1, Ordering::SeqCst);
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    operational_status("claude")
                },
                || async { operational_status("codex") },
            ))
        });
        started_rx.recv().unwrap();
        let skipped = tauri::async_runtime::block_on(poll_once_with_fetchers(
            &state,
            || async { panic!("second Claude fetch must not start") },
            || async { panic!("second OpenAI fetch must not start") },
        ));
        assert!(skipped.is_none());
        release_tx.send(()).unwrap();
        assert!(first.join().unwrap().is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn disabled_production_poll_does_not_invoke_fetchers_and_clears_badges() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let state = ServiceStatusState::default();
        state.set_prefs(false, true);
        let calls = Arc::new(AtomicUsize::new(0));
        let claude_calls = calls.clone();
        let codex_calls = calls.clone();
        let outcome = tauri::async_runtime::block_on(poll_once_with_fetchers(
            &state,
            move || async move {
                claude_calls.fetch_add(1, Ordering::SeqCst);
                operational_status("claude")
            },
            move || async move {
                codex_calls.fetch_add(1, Ordering::SeqCst);
                operational_status("codex")
            },
        ));
        assert!(outcome.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            state.incident_flags(),
            [ServiceAlert::None, ServiceAlert::None]
        );
    }

    #[test]
    fn maintenance_is_not_a_production_tray_incident() {
        let status = ServiceStatus {
            level: ServiceLevel::Maintenance,
            maintenances: vec![ServiceEvent {
                id: "maintenance".into(),
                name: "Maintenance".into(),
                status: "in_progress".into(),
                impact: None,
                url: None,
                latest_update: None,
                started_at: None,
                affected_components: vec![],
                alert: ServiceAlert::None,
            }],
            ..operational_status("claude")
        };
        assert!(status.is_active());
        assert!(!status.is_incident());
    }

    fn active_status(level: ServiceLevel, update: &str) -> ServiceStatus {
        ServiceStatus {
            provider: "claude".into(),
            level,
            components: vec![],
            maintenances: vec![],
            fetched_at: None,
            error: None,
            alert: ServiceAlert::None,
            incidents: vec![ServiceEvent {
                id: "incident".into(),
                name: "Affected service".into(),
                status: "monitoring".into(),
                impact: None,
                url: None,
                latest_update: Some(ServiceUpdate {
                    id: update.into(),
                    status: "monitoring".into(),
                    body: "A public update".into(),
                    updated_at: None,
                }),
                started_at: None,
                affected_components: vec![],
                alert: ServiceAlert::Critical,
            }],
        }
    }
    fn notification_snapshot(claude: ServiceStatus) -> ServiceStatusSnapshot {
        ServiceStatusSnapshot {
            claude,
            codex: ServiceStatus::unavailable("codex", "test"),
        }
    }

    #[test]
    fn notification_decisions_dedupe_defer_and_resolve_with_injected_clock() {
        let state = ServiceStatusState::default();
        let mut runtime = state.inner.lock().unwrap();
        let start = std::time::Instant::now();
        let first = notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::Degraded, "u1")),
            start,
        );
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].title, "Anthropic: degraded");
        assert!(first[0].body.contains("Affected service"));
        mark_notification_delivered_at(&mut runtime, 0, start);
        assert!(notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::Degraded, "u1")),
            start + Duration::from_secs(1)
        )
        .is_empty());
        assert!(notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::PartialOutage, "u2")),
            start + Duration::from_secs(2)
        )
        .is_empty());
        let deferred = notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::PartialOutage, "u2")),
            start + Duration::from_secs(60),
        );
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].title, "Anthropic: partial outage");
        let resolved = ServiceStatus {
            level: ServiceLevel::Operational,
            incidents: vec![],
            ..active_status(ServiceLevel::Operational, "unused")
        };
        let result = notification_decisions_at(
            &mut runtime,
            &notification_snapshot(resolved),
            start + Duration::from_secs(120),
        );
        assert_eq!(result[0].title, "Anthropic: resolved");
        assert_eq!(result[0].body, "All watched services are operational.");
    }

    #[test]
    fn notification_body_uses_the_changed_event_instead_of_the_first_event() {
        let start = std::time::Instant::now();
        let mut runtime = fresh_runtime();
        let old = active_status(ServiceLevel::Degraded, "old-update");
        let _ = notification_decisions_at(&mut runtime, &notification_snapshot(old.clone()), start);
        mark_notification_delivered_at(&mut runtime, 0, start);
        let mut changed = old;
        changed.incidents.push(ServiceEvent {
            id: "new-incident".into(),
            name: "Newly affected service".into(),
            status: "investigating".into(),
            impact: None,
            url: None,
            latest_update: Some(ServiceUpdate {
                id: "new-update".into(),
                status: "investigating".into(),
                body: "The new event body".into(),
                updated_at: None,
            }),
            started_at: None,
            affected_components: vec![],
            alert: ServiceAlert::Critical,
        });
        let output = notification_decisions_at(
            &mut runtime,
            &notification_snapshot(changed),
            start + Duration::from_secs(61),
        );
        assert_eq!(output.len(), 1);
        assert!(output[0].body.contains("Newly affected service"));
        assert!(!output[0].body.contains("Affected service —"));
    }

    #[test]
    fn unknown_or_disabled_notifications_never_send_and_prefs_clear_status() {
        let state = ServiceStatusState::default();
        let mut runtime = state.inner.lock().unwrap();
        let now = std::time::Instant::now();
        assert!(notification_decisions_at(
            &mut runtime,
            &notification_snapshot(ServiceStatus::unavailable("claude", "no")),
            now
        )
        .is_empty());
        runtime.notify = false;
        assert!(notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::Degraded, "u1")),
            now
        )
        .is_empty());
        drop(runtime);
        assert!(!state.set_prefs(false, false));
        let snapshot = state.snapshot();
        assert_eq!(snapshot.claude.level, ServiceLevel::Unknown);
        assert!(state.set_prefs(true, true));
    }

    #[test]
    fn openai_v2_and_proxy_fixtures_are_operational() {
        let v2 = json(include_str!(
            "../../tests/fixtures/service_status/openai_v2_summary_2026-10-09_operational.json"
        ));
        assert_eq!(
            parse_openai(&v2, None).unwrap().level,
            ServiceLevel::Operational
        );
        let proxy = json(include_str!(
            "../../tests/fixtures/service_status/openai_proxy_2026-10-09_operational.json"
        ));
        assert_eq!(
            parse_openai(&v2, Some(&proxy)).unwrap().level,
            ServiceLevel::Operational
        );
    }

    #[test]
    fn unknown_component_status_maps_to_unknown() {
        assert_eq!(level_for_status("not_published_yet"), ServiceLevel::Unknown);
    }

    #[test]
    fn missing_summary_fields_fail_closed_without_panicking() {
        let result =
            std::panic::catch_unwind(|| parse_statuspage("claude", &json("{}"), watched_claude));
        assert!(result.is_ok());
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn non_watched_component_incident_does_not_alert() {
        let summary = json(
            r#"{"components":[{"id":"images","name":"Images","status":"operational"},{"id":"api","name":"Claude API","status":"operational"}],"incidents":[{"id":"i","name":"Images only","status":"investigating","components":[{"id":"images","name":"Images","status":"partial_outage"}]}]}"#,
        );
        let status = parse_statuspage("claude", &summary, watched_claude).unwrap();
        assert!(!status.is_incident());
        assert!(status.incidents.is_empty());
    }

    #[test]
    fn full_outage_maps_to_major_outage() {
        assert_eq!(level_for_status("full_outage"), ServiceLevel::MajorOutage);
    }

    #[test]
    fn normal_polling_interval_is_five_minutes() {
        assert_eq!(
            ServiceStatusState::default().next_delay(),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn incident_polling_interval_is_one_minute() {
        let state = ServiceStatusState::default();
        state.inner.lock().unwrap().claude.level = ServiceLevel::Degraded;
        assert_eq!(state.next_delay(), Duration::from_secs(60));
    }

    #[test]
    fn recovery_returns_polling_interval_to_five_minutes() {
        let state = ServiceStatusState::default();
        {
            let mut runtime = state.inner.lock().unwrap();
            runtime.claude.level = ServiceLevel::Degraded;
        }
        assert_eq!(state.next_delay(), Duration::from_secs(60));
        state.inner.lock().unwrap().claude.level = ServiceLevel::Operational;
        assert_eq!(state.next_delay(), Duration::from_secs(300));
    }

    #[test]
    fn consecutive_failures_back_off_one_two_then_five_minutes() {
        let state = ServiceStatusState::default();
        for (failures, expected) in [(1, 60), (2, 120), (3, 300), (4, 300)] {
            state.inner.lock().unwrap().failures = [failures, 0];
            assert_eq!(state.next_delay(), Duration::from_secs(expected));
        }
    }

    #[test]
    fn thirty_minutes_without_success_marks_failed_providers_unknown() {
        let state = ServiceStatusState::default();
        let mut runtime = state.inner.lock().unwrap().clone();
        runtime.last_success =
            [Some(std::time::Instant::now() - Duration::from_secs(30 * 60 + 1)); 2];
        let mut claude = ServiceStatus {
            error: Some("request failed".into()),
            ..active_status(ServiceLevel::Degraded, "u1")
        };
        let mut codex = ServiceStatus {
            provider: "codex".into(),
            error: Some("request failed".into()),
            ..active_status(ServiceLevel::Degraded, "u1")
        };
        mark_stale_failed_statuses(&runtime, &mut claude, &mut codex);
        assert_eq!(claude.level, ServiceLevel::Unknown);
        assert_eq!(codex.level, ServiceLevel::Unknown);
    }

    fn fresh_runtime() -> Runtime {
        ServiceStatusState::default().inner.lock().unwrap().clone()
    }

    #[test]
    fn first_incident_notification_has_public_title_and_body() {
        let start = std::time::Instant::now();
        let output = notification_decisions_at(
            &mut fresh_runtime(),
            &notification_snapshot(active_status(ServiceLevel::Degraded, "u1")),
            start,
        );
        assert_eq!(output[0].title, "Anthropic: degraded");
        assert_eq!(
            output[0].body,
            "Affected service — Monitoring: A public update"
        );
    }

    #[test]
    fn unchanged_notification_fingerprint_does_not_send_again() {
        let start = std::time::Instant::now();
        let mut runtime = fresh_runtime();
        let snapshot = notification_snapshot(active_status(ServiceLevel::Degraded, "u1"));
        assert_eq!(
            notification_decisions_at(&mut runtime, &snapshot, start).len(),
            1
        );
        mark_notification_delivered_at(&mut runtime, 0, start);
        assert!(notification_decisions_at(
            &mut runtime,
            &snapshot,
            start + Duration::from_secs(61)
        )
        .is_empty());
    }

    #[test]
    fn new_incident_update_sends_one_notification() {
        let start = std::time::Instant::now();
        let mut runtime = fresh_runtime();
        assert_eq!(
            notification_decisions_at(
                &mut runtime,
                &notification_snapshot(active_status(ServiceLevel::Degraded, "u1")),
                start
            )
            .len(),
            1
        );
        let output = notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::Degraded, "u2")),
            start + Duration::from_secs(61),
        );
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].title, "Anthropic: degraded");
    }

    #[test]
    fn changes_inside_one_minute_keep_only_the_latest_notification() {
        let start = std::time::Instant::now();
        let mut runtime = fresh_runtime();
        let _ = notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::Degraded, "u1")),
            start,
        );
        mark_notification_delivered_at(&mut runtime, 0, start);
        assert!(notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::PartialOutage, "u2")),
            start + Duration::from_secs(1)
        )
        .is_empty());
        assert!(notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::MajorOutage, "u3")),
            start + Duration::from_secs(2)
        )
        .is_empty());
        let output = notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::MajorOutage, "u3")),
            start + Duration::from_secs(60),
        );
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].title, "Anthropic: major outage");
    }

    #[test]
    fn recovery_notification_uses_resolved_title() {
        let start = std::time::Instant::now();
        let mut runtime = fresh_runtime();
        let _ = notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::Degraded, "u1")),
            start,
        );
        mark_notification_delivered_at(&mut runtime, 0, start);
        let resolved = ServiceStatus {
            level: ServiceLevel::Operational,
            incidents: vec![],
            ..active_status(ServiceLevel::Operational, "unused")
        };
        let output = notification_decisions_at(
            &mut runtime,
            &notification_snapshot(resolved),
            start + Duration::from_secs(61),
        );
        assert_eq!(output[0].title, "Anthropic: resolved");
        assert_eq!(output[0].body, "All watched services are operational.");
    }

    #[test]
    fn unknown_status_never_sends_notification() {
        let now = std::time::Instant::now();
        assert!(notification_decisions_at(
            &mut fresh_runtime(),
            &notification_snapshot(ServiceStatus::unavailable("claude", "fetch failed")),
            now
        )
        .is_empty());
    }

    #[test]
    fn disabled_service_status_notifications_never_send() {
        let now = std::time::Instant::now();
        let mut runtime = fresh_runtime();
        runtime.notify = false;
        assert!(notification_decisions_at(
            &mut runtime,
            &notification_snapshot(active_status(ServiceLevel::Degraded, "u1")),
            now
        )
        .is_empty());
    }

    #[test]
    fn startup_with_active_incident_sends_current_notification() {
        let output = notification_decisions_at(
            &mut fresh_runtime(),
            &notification_snapshot(active_status(ServiceLevel::Degraded, "u1")),
            std::time::Instant::now(),
        );
        assert_eq!(output.len(), 1);
    }

    #[test]
    fn disabling_preferences_clears_status_and_reenabling_requests_poll() {
        let state = ServiceStatusState::default();
        assert!(!state.set_prefs(false, true));
        let snapshot = state.snapshot();
        assert_eq!(snapshot.claude.level, ServiceLevel::Unknown);
        assert_eq!(snapshot.codex.level, ServiceLevel::Unknown);
        assert!(state.set_prefs(true, true));
    }

    #[test]
    fn claude_monitoring_console_fixture_is_warning() {
        let status = parse_statuspage("claude", &json(include_str!("../../tests/fixtures/service_status/claude_summary_2026-10-11_console_monitoring.json")), watched_claude).unwrap();
        assert_eq!(status.alert, ServiceAlert::Warning);
        assert_eq!(status.incidents.len(), 1);
        assert_eq!(status.incidents[0].status, "monitoring");
        assert!(status.incidents[0]
            .started_at
            .as_deref()
            .unwrap()
            .starts_with("2026-10-07T13:25:04"));
        assert!(status.incidents[0]
            .latest_update
            .as_ref()
            .unwrap()
            .updated_at
            .as_deref()
            .unwrap()
            .starts_with("2026-10-07T21:44:41"));
    }

    #[test]
    fn claude_core_incident_investigating_is_critical() {
        let mut value = json(include_str!(
            "../../tests/fixtures/service_status/claude_summary_2026-10-11_console_monitoring.json"
        ));
        let code_id = value["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|component| component["name"] == "Claude Code")
            .unwrap()["id"]
            .clone();
        let event = &mut value["incidents"][0];
        event["status"] = Value::String("investigating".into());
        event["components"][0]["id"] = code_id;
        let critical = parse_statuspage("claude", &value, watched_claude).unwrap();
        assert_eq!(critical.alert, ServiceAlert::Critical);
        value["incidents"][0]["status"] = Value::String("monitoring".into());
        assert_eq!(
            parse_statuspage("claude", &value, watched_claude)
                .unwrap()
                .alert,
            ServiceAlert::Warning
        );
    }

    #[test]
    fn codex_proxy_incident_real_shape_is_parsed_and_unwatched_is_ignored() {
        let v2 = json(include_str!("../../tests/fixtures/service_status/openai_v2_summary_2026-10-11_compliance_monitoring.json"));
        let proxy = json(include_str!("../../tests/fixtures/service_status/openai_proxy_2026-10-11_compliance_monitoring.json"));
        let ignored = parse_openai(&v2, Some(&proxy)).unwrap();
        assert!(ignored.incidents.is_empty());
        assert_eq!(ignored.alert, ServiceAlert::None);
        let mut proxy = proxy;
        let cli = proxy
            .pointer("/summary/structure/items/2/group/components/2/component_id")
            .unwrap()
            .clone();
        proxy["summary"]["ongoing_incidents"][0]["affected_components"][0]["component_id"] =
            cli.clone();
        proxy["summary"]["ongoing_incidents"][0]["component_impacts"][0]["component_id"] = cli;
        let status = parse_openai(&v2, Some(&proxy)).unwrap();
        assert_eq!(status.incidents.len(), 1);
        assert_eq!(status.incidents[0].status, "monitoring");
        assert!(status.incidents[0]
            .affected_components
            .iter()
            .any(|name| name == "CLI"));
        assert_eq!(
            status.incidents[0].url.as_deref(),
            Some("https://status.openai.com/incidents/01M4GV1M2A3K38T73JCVCD9G0R")
        );
        assert!(status.incidents[0]
            .latest_update
            .as_ref()
            .unwrap()
            .updated_at
            .as_deref()
            .unwrap()
            .starts_with("2026-10-09T22:03:59"));
        assert_eq!(status.alert, ServiceAlert::Warning);
        proxy["summary"]["ongoing_incidents"][0]["status"] = Value::String("identified".into());
        assert_eq!(
            parse_openai(&v2, Some(&proxy)).unwrap().alert,
            ServiceAlert::Critical
        );
    }

    fn claude_component(id: &str, name: &str, status: &str) -> Component {
        Component {
            id: id.into(),
            name: name.into(),
            status: status.into(),
        }
    }

    fn claude_event(id: &str, status: &str, component: &Component) -> RawEvent {
        RawEvent {
            id: id.into(),
            name: id.into(),
            status: status.into(),
            impact: None,
            shortlink: None,
            scheduled_for: None,
            started_at: None,
            created_at: None,
            components: vec![component.clone()],
            incident_updates: vec![],
        }
    }

    fn claude_status_with(
        components: Vec<Component>,
        incidents: Vec<RawEvent>,
        maintenances: Vec<RawEvent>,
    ) -> ServiceStatus {
        let watched_ids: HashSet<String> = components
            .iter()
            .map(|component| component.id.clone())
            .collect();
        let watched_refs = watched_ids.iter().map(String::as_str).collect();
        build_status(
            "claude",
            components,
            incidents,
            maintenances,
            &watched_refs,
            &HashSet::new(),
        )
    }

    #[test]
    fn claude_console_investigating_is_warning() {
        let console = claude_component("console", "Claude Console", "operational");
        let status = claude_status_with(
            vec![console.clone()],
            vec![claude_event("incident", "investigating", &console)],
            vec![],
        );
        assert_eq!(status.alert, ServiceAlert::Warning);
    }

    #[test]
    fn core_component_degraded_without_incident_is_critical() {
        let status = claude_status_with(
            vec![claude_component(
                "code",
                "Claude Code",
                "degraded_performance",
            )],
            vec![],
            vec![],
        );
        assert_eq!(status.alert, ServiceAlert::Critical);
    }

    #[test]
    fn core_component_degraded_covered_only_by_monitoring_is_warning() {
        let code = claude_component("code", "Claude Code", "degraded_performance");
        let status = claude_status_with(
            vec![code.clone()],
            vec![claude_event("monitoring", "monitoring", &code)],
            vec![],
        );
        assert_eq!(status.alert, ServiceAlert::Warning);
    }

    #[test]
    fn core_component_degraded_with_one_identified_and_one_monitoring_is_critical() {
        let code = claude_component("code", "Claude Code", "degraded_performance");
        let status = claude_status_with(
            vec![code.clone()],
            vec![
                claude_event("identified", "identified", &code),
                claude_event("monitoring", "monitoring", &code),
            ],
            vec![],
        );
        assert_eq!(status.alert, ServiceAlert::Critical);
    }

    #[test]
    fn unrecognized_phase_on_core_incident_is_critical() {
        let code = claude_component("code", "Claude Code", "operational");
        let status = claude_status_with(
            vec![code.clone()],
            vec![claude_event("future", "future_phase", &code)],
            vec![],
        );
        assert_eq!(status.alert, ServiceAlert::Critical);
    }

    #[test]
    fn codex_incident_in_both_endpoints_is_deduplicated() {
        let mut v2 = json(include_str!(
            "../../tests/fixtures/service_status/openai_v2_summary_2026-10-11_compliance_monitoring.json"
        ));
        let mut proxy = json(include_str!(
            "../../tests/fixtures/service_status/openai_proxy_2026-10-11_compliance_monitoring.json"
        ));
        let cli = proxy
            .pointer("/summary/structure/items/2/group/components/2/component_id")
            .unwrap()
            .clone();
        v2["incidents"][0]["components"] =
            serde_json::json!([{ "id": cli.clone(), "name": "CLI" }]);
        proxy["summary"]["ongoing_incidents"][0]["affected_components"][0]["component_id"] =
            cli.clone();
        proxy["summary"]["ongoing_incidents"][0]["component_impacts"][0]["component_id"] = cli;
        let status = parse_openai(&v2, Some(&proxy)).unwrap();
        assert_eq!(status.incidents.len(), 1);
        assert_eq!(status.incidents[0].affected_components, vec!["CLI"]);
    }

    #[test]
    fn codex_login_incident_is_warning_even_when_investigating() {
        let mut v2 = json(include_str!(
            "../../tests/fixtures/service_status/openai_v2_summary_2026-10-11_compliance_monitoring.json"
        ));
        let login_id = v2["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|component| component["name"] == "Login")
            .unwrap()["id"]
            .clone();
        v2["incidents"][0]["status"] = Value::String("investigating".into());
        v2["incidents"][0]["components"] = serde_json::json!([{ "id": login_id, "name": "Login" }]);
        assert_eq!(
            parse_openai(&v2, None).unwrap().alert,
            ServiceAlert::Warning
        );
    }

    #[test]
    fn claude_cowork_incident_is_warning() {
        let mut value = json(include_str!(
            "../../tests/fixtures/service_status/claude_summary_2026-10-11_console_monitoring.json"
        ));
        let id = value["incidents"][0]["components"][0]["id"].clone();
        let component = value["components"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|component| component["id"] == id)
            .unwrap();
        component["name"] = Value::String("Claude Cowork desktop".into());
        value["incidents"][0]["status"] = Value::String("investigating".into());
        let status = parse_statuspage("claude", &value, watched_claude).unwrap();
        assert_eq!(status.alert, ServiceAlert::Warning);
        assert_eq!(
            status
                .components
                .iter()
                .find(|component| component.name == "Claude Cowork desktop")
                .unwrap()
                .tier,
            "other"
        );
    }

    #[test]
    fn finish_poll_side_effects_passes_warning_to_tray() {
        use std::cell::RefCell;

        let state = ServiceStatusState::default();
        let outcome = tauri::async_runtime::block_on(poll_once_with_fetchers(
            &state,
            || async {
                let mut status = active_status(ServiceLevel::Degraded, "warning");
                status.alert = ServiceAlert::Warning;
                status
            },
            || async { operational_status("codex") },
        ))
        .unwrap();
        let calls = RefCell::new(Vec::new());
        finish_poll_side_effects(
            &state,
            outcome,
            |index, alert| calls.borrow_mut().push((index, alert)),
            |_| {},
            |_| Ok(()),
        );
        assert_eq!(
            calls.into_inner(),
            vec![(0, ServiceAlert::Warning), (1, ServiceAlert::None)]
        );
    }

    #[test]
    fn maintenance_and_unknown_do_not_contribute_alert() {
        let code = claude_component("code", "Claude Code", "under_maintenance");
        let maintenance = claude_status_with(
            vec![code.clone()],
            vec![],
            vec![claude_event("maintenance", "verifying", &code)],
        );
        assert_eq!(maintenance.alert, ServiceAlert::None);
        let unknown = claude_status_with(
            vec![claude_component("code", "Claude Code", "unannounced")],
            vec![],
            vec![],
        );
        assert_eq!(unknown.alert, ServiceAlert::None);
    }

    #[test]
    fn same_name_components_are_judged_by_id() {
        let component_a = claude_component("a", "Claude Code", "degraded_performance");
        let component_b = claude_component("b", "Claude Code", "operational");
        let status = claude_status_with(
            vec![component_a, component_b.clone()],
            vec![claude_event("monitoring", "monitoring", &component_b)],
            vec![],
        );
        assert_eq!(status.alert, ServiceAlert::Critical);
    }

    #[test]
    fn standalone_proxy_component_is_mapped() {
        let v2 = json(include_str!(
            "../../tests/fixtures/service_status/openai_v2_summary_2026-10-11_compliance_monitoring.json"
        ));
        let mut proxy = json(include_str!(
            "../../tests/fixtures/service_status/openai_proxy_2026-10-11_compliance_monitoring.json"
        ));
        proxy["summary"]["structure"]["items"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "component": { "component_id": "standalone-login", "name": "Login" } }));
        proxy["summary"]["ongoing_incidents"][0]["affected_components"][0]["component_id"] =
            Value::String("standalone-login".into());
        proxy["summary"]["ongoing_incidents"][0]["component_impacts"][0]["component_id"] =
            Value::String("standalone-login".into());
        let status = parse_openai(&v2, Some(&proxy)).unwrap();
        assert_eq!(status.incidents.len(), 1);
        assert_eq!(status.incidents[0].affected_components, vec!["Login"]);
    }

    #[test]
    fn proxy_incident_with_unnamed_watched_id_is_kept() {
        let v2 = json(include_str!(
            "../../tests/fixtures/service_status/openai_v2_summary_2026-10-11_compliance_monitoring.json"
        ));
        let response_id = v2["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|component| component["name"] == "Responses")
            .unwrap()["id"]
            .clone();
        let mut proxy = json(include_str!(
            "../../tests/fixtures/service_status/openai_proxy_2026-10-11_compliance_monitoring.json"
        ));
        proxy["summary"]["structure"]["items"][0]["group"]["components"]
            .as_array_mut()
            .unwrap()
            .retain(|component| component["component_id"] != response_id);
        proxy["summary"]["ongoing_incidents"][0]["affected_components"][0]["component_id"] =
            response_id.clone();
        proxy["summary"]["ongoing_incidents"][0]["component_impacts"][0]["component_id"] =
            response_id;
        let status = parse_openai(&v2, Some(&proxy)).unwrap();
        assert_eq!(status.incidents.len(), 1);
        assert_eq!(status.incidents[0].affected_components, vec!["Responses"]);
    }

    #[test]
    fn v2_incident_kept_when_proxy_version_is_dropped() {
        let mut v2 = json(include_str!(
            "../../tests/fixtures/service_status/openai_v2_summary_2026-10-11_compliance_monitoring.json"
        ));
        let response_id = v2["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|component| component["name"] == "Responses")
            .unwrap()["id"]
            .clone();
        v2["incidents"][0]["components"] =
            serde_json::json!([{ "id": response_id, "name": "Responses" }]);
        let proxy = json(include_str!(
            "../../tests/fixtures/service_status/openai_proxy_2026-10-11_compliance_monitoring.json"
        ));
        let status = parse_openai(&v2, Some(&proxy)).unwrap();
        assert_eq!(status.incidents.len(), 1);
        assert_eq!(status.incidents[0].affected_components, vec!["Responses"]);
    }

    #[test]
    fn latest_update_uses_instant_not_string() {
        let code = claude_component("code", "Claude Code", "operational");
        let mut incident = claude_event("incident", "identified", &code);
        incident.incident_updates = vec![
            RawUpdate {
                id: "plus-ten".into(),
                status: "identified".into(),
                body: "earlier instant".into(),
                updated_at: Some("2026-10-07T10:00:00+10:00".into()),
                created_at: None,
                display_at: None,
            },
            RawUpdate {
                id: "utc".into(),
                status: "identified".into(),
                body: "later instant".into(),
                updated_at: Some("2026-10-07T01:00:00Z".into()),
                created_at: None,
                display_at: None,
            },
        ];
        let status = claude_status_with(vec![code], vec![incident], vec![]);
        assert_eq!(
            status.incidents[0].latest_update.as_ref().unwrap().id,
            "utc"
        );
    }

    #[test]
    fn malformed_proxy_maintenance_is_skipped() {
        let v2 = json(include_str!(
            "../../tests/fixtures/service_status/openai_v2_summary_2026-10-11_compliance_monitoring.json"
        ));
        let mut proxy = json(include_str!(
            "../../tests/fixtures/service_status/openai_proxy_2026-10-11_compliance_monitoring.json"
        ));
        proxy["summary"]["scheduled_maintenances"] = serde_json::json!([{}]);
        assert!(parse_openai(&v2, Some(&proxy)).is_ok());
    }
    #[test]
    fn unnamed_codex_group_component_is_core_and_shown_as_unknown() {
        let v2 = json(include_str!(
            "../../tests/fixtures/service_status/openai_v2_summary_2026-10-11_compliance_monitoring.json"
        ));
        let mut proxy = json(include_str!(
            "../../tests/fixtures/service_status/openai_proxy_2026-10-11_compliance_monitoring.json"
        ));
        proxy["summary"]["structure"]["items"][2]["group"]["components"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "component_id": "core-x" }));
        let incident = &mut proxy["summary"]["ongoing_incidents"][0];
        incident["status"] = Value::String("investigating".into());
        incident["affected_components"][0]["component_id"] = Value::String("core-x".into());
        incident["component_impacts"][0]["component_id"] = Value::String("core-x".into());
        let status = parse_openai(&v2, Some(&proxy)).unwrap();
        assert_eq!(status.incidents.len(), 1);
        assert_eq!(
            status.incidents[0].affected_components,
            vec!["Unknown component"]
        );
        assert_eq!(status.alert, ServiceAlert::Critical);
    }

    #[test]
    fn proxy_resolved_version_supersedes_v2_investigating() {
        let mut v2 = json(include_str!(
            "../../tests/fixtures/service_status/openai_v2_summary_2026-10-11_compliance_monitoring.json"
        ));
        let mut proxy = json(include_str!(
            "../../tests/fixtures/service_status/openai_proxy_2026-10-11_compliance_monitoring.json"
        ));
        let response_id = v2["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|component| component["name"] == "Responses")
            .unwrap()["id"]
            .clone();
        v2["incidents"][0]["status"] = Value::String("investigating".into());
        v2["incidents"][0]["components"] =
            serde_json::json!([{ "id": response_id.clone(), "name": "Responses" }]);
        let incident = &mut proxy["summary"]["ongoing_incidents"][0];
        incident["status"] = Value::String("resolved".into());
        incident["affected_components"][0]["component_id"] = response_id.clone();
        incident["component_impacts"][0]["component_id"] = response_id;
        let status = parse_openai(&v2, Some(&proxy)).unwrap();
        assert!(status.incidents.is_empty());
        assert_eq!(status.alert, ServiceAlert::None);
    }

    #[test]
    fn affected_component_names_are_ordered_core_first_then_by_name() {
        let v2 = json(include_str!(
            "../../tests/fixtures/service_status/openai_v2_summary_2026-10-11_compliance_monitoring.json"
        ));
        let mut proxy = json(include_str!(
            "../../tests/fixtures/service_status/openai_proxy_2026-10-11_compliance_monitoring.json"
        ));
        let id_of = |name: &str| {
            v2["components"]
                .as_array()
                .unwrap()
                .iter()
                .find(|component| component["name"] == name)
                .unwrap()["id"]
                .clone()
        };
        let cli = proxy
            .pointer("/summary/structure/items/2/group/components/2/component_id")
            .unwrap()
            .clone();
        let affected = serde_json::json!([
            { "component_id": id_of("Responses"), "status": "degraded_performance" },
            { "component_id": cli, "status": "degraded_performance" },
            { "component_id": id_of("Codex in ChatGPT Desktop"), "status": "degraded_performance" }
        ]);
        proxy["summary"]["ongoing_incidents"][0]["affected_components"] = affected;
        proxy["summary"]["ongoing_incidents"][0]["component_impacts"] = serde_json::json!([]);
        for _ in 0..5 {
            let status = parse_openai(&v2, Some(&proxy)).unwrap();
            assert_eq!(
                status.incidents[0].affected_components,
                vec!["CLI", "Codex in ChatGPT Desktop", "Responses"]
            );
        }
    }
}

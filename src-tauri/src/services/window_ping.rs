//! Narrow, user-initiated 5-hour window wake-up commands.
//!
//! This module intentionally never logs child output.  The only data it
//! returns is the small, serializable outcome below.

use crate::domain::account::{CodexProfile, RouteKey};
use crate::domain::models::CodexRateLimitWindow;
use crate::services::{claude, codex, state_location};
use once_cell::sync::Lazy;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::future::Future;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

pub const MARGIN_SECS: i64 = 60;
const WINDOW_SECS: i64 = 5 * 60 * 60;
const CODEX_MODEL: &str = "gpt-5.6-luna"; // gpt-5.6-* generation; change here and re-test argv when it changes.
const CLAUDE_MODEL: &str = "claude-haiku-4-5"; // Claude model selection is deliberately fixed; re-test argv when it changes.
const CLI_TIMEOUT: Duration = Duration::from_secs(120);
pub const CONFIRM_DELAYS_SECS: [u64; 5] = [0, 30, 60, 120, 300];
const CONFIRM_TOTAL_SECS: u64 = 0 + 30 + 60 + 120 + 300;
const _: () = assert!(CONFIRM_TOTAL_SECS > (2 * MARGIN_SECS) as u64);

const CODEX_CLI_CANDIDATES: [&str; 4] = [
    "bin/codex",
    ".local/bin/codex",
    "/usr/local/bin/codex",
    "/opt/homebrew/bin/codex",
];
const CLAUDE_CLI_CANDIDATES: [&str; 4] = [
    ".local/bin/claude",
    "bin/claude",
    "/usr/local/bin/claude",
    "/opt/homebrew/bin/claude",
];
const CHATGPT_APP_ROOT: &str = "/Applications/ChatGPT.app";

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PingOutcome {
    Opened {
        resets_at: i64,
        tokens: Option<u64>,
        confirmed_after_secs: u64,
    },
    SentUnconfirmed {
        tokens: Option<u64>,
        expected_resets_at: i64,
    },
    AlreadyOpen {
        resets_at: Option<i64>,
    },
    Blocked,
    Busy,
    CliNotFound {
        cli: &'static str,
    },
    CliFailed {
        code: &'static str,
    },
    ProfileUnavailable,
    QuotaUnreadable,
    ConfirmationRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowState {
    Open,
    Closed,
    Unknown,
}

pub fn codex_window_state(primary: Option<&CodexRateLimitWindow>, now: i64) -> WindowState {
    let Some(primary) = primary else {
        return WindowState::Unknown;
    };
    let (Some(resets_at), Some(minutes)) = (primary.resets_at, primary.window_minutes) else {
        return WindowState::Unknown;
    };
    if !primary.used_percent.is_finite()
        || primary.used_percent < 0.0
        || resets_at < 0
        || minutes <= 0
    {
        return WindowState::Unknown;
    }
    if primary.used_percent > 0.0 || resets_at - now < minutes.saturating_mul(60) - MARGIN_SECS {
        WindowState::Open
    } else {
        WindowState::Closed
    }
}

pub fn claude_window_state(
    session: Option<&crate::domain::models::UsageInfo>,
    now: i64,
) -> WindowState {
    let Some(session) = session else {
        return WindowState::Unknown;
    };
    if !session.used.is_finite()
        || !session.limit.is_finite()
        || !session.percentage.is_finite()
        || session.used < 0.0
        || session.limit < 0.0
        || session.percentage < 0.0
    {
        return WindowState::Unknown;
    }
    if session.percentage > 0.0 {
        return WindowState::Open;
    }
    let Some(reset) = session.reset_time.as_deref() else {
        return WindowState::Closed;
    };
    if !is_strict_rfc3339(reset) {
        return WindowState::Unknown;
    }
    let Ok(reset) = chrono::DateTime::parse_from_rfc3339(reset) else {
        return WindowState::Unknown;
    };
    let reset = reset.timestamp();
    if reset > now && reset - now < 5 * 60 * 60 - MARGIN_SECS {
        WindowState::Open
    } else {
        WindowState::Unknown
    }
}

fn is_strict_rfc3339(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || ![0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
            .iter()
            .all(|&index| bytes[index].is_ascii_digit())
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return false;
    }

    let number_at = |index| (bytes[index] - b'0') as u16;
    let month = number_at(5) * 10 + number_at(6);
    let hour = number_at(11) * 10 + number_at(12);
    let minute = number_at(14) * 10 + number_at(15);
    let second = number_at(17) * 10 + number_at(18);
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return false;
    }

    let mut timezone = 19;
    if bytes.get(timezone) == Some(&b'.') {
        timezone += 1;
        let fraction_start = timezone;
        while bytes.get(timezone).is_some_and(u8::is_ascii_digit) {
            timezone += 1;
        }
        if timezone == fraction_start {
            return false;
        }
    }
    match bytes.get(timezone) {
        Some(b'Z') => timezone + 1 == bytes.len(),
        Some(b'+' | b'-') => {
            timezone + 6 == bytes.len()
                && bytes[timezone + 1].is_ascii_digit()
                && bytes[timezone + 2].is_ascii_digit()
                && bytes[timezone + 3] == b':'
                && bytes[timezone + 4].is_ascii_digit()
                && bytes[timezone + 5].is_ascii_digit()
                && number_at(timezone + 1) * 10 + number_at(timezone + 2) <= 23
                && number_at(timezone + 4) * 10 + number_at(timezone + 5) <= 59
        }
        _ => false,
    }
}

fn staircase_is_safe(delays: &[u64]) -> bool {
    delays.iter().sum::<u64>() > (2 * MARGIN_SECS) as u64
}

#[derive(Debug, PartialEq, Eq)]
struct ConfirmationStep {
    delay_secs: u64,
    confirmed_after_secs: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct ConfirmationPlan {
    steps: Vec<ConfirmationStep>,
    expected_resets_at: i64,
}

fn confirmation_plan(completed_at: i64, delays: &[u64]) -> Option<ConfirmationPlan> {
    let expected_resets_at = completed_at.checked_add(WINDOW_SECS)?;
    let mut confirmed_after_secs: u64 = 0;
    let mut steps = Vec::with_capacity(delays.len());
    for &delay_secs in delays {
        confirmed_after_secs = confirmed_after_secs.checked_add(delay_secs)?;
        steps.push(ConfirmationStep {
            delay_secs,
            confirmed_after_secs,
        });
    }
    Some(ConfirmationPlan {
        steps,
        expected_resets_at,
    })
}

fn ping_send_decision(
    state: WindowState,
    force: bool,
    resets_at: Option<i64>,
    blocked: bool,
) -> Result<(), PingOutcome> {
    if blocked {
        return Err(PingOutcome::Blocked);
    }
    if force || state == WindowState::Closed {
        return Ok(());
    }
    match state {
        WindowState::Open => Err(PingOutcome::AlreadyOpen { resets_at }),
        WindowState::Unknown => Err(PingOutcome::ConfirmationRequired),
        WindowState::Closed => Ok(()),
    }
}

static CODEX_IN_FLIGHT: Lazy<Mutex<HashSet<RouteKey>>> = Lazy::new(|| Mutex::new(HashSet::new()));
static CLAUDE_IN_FLIGHT: Lazy<Mutex<bool>> = Lazy::new(|| Mutex::new(false));

struct CodexFlight(RouteKey);
impl CodexFlight {
    fn acquire(profile: &CodexProfile) -> Option<Self> {
        let mut set = CODEX_IN_FLIGHT.lock().ok()?;
        let key = profile.route().clone();
        if !set.insert(key.clone()) {
            return None;
        }
        Some(Self(key))
    }
}
impl Drop for CodexFlight {
    fn drop(&mut self) {
        if let Ok(mut set) = CODEX_IN_FLIGHT.lock() {
            set.remove(&self.0);
        }
    }
}

pub(crate) struct ClaudeFlight;
impl ClaudeFlight {
    pub(crate) fn acquire() -> Option<Self> {
        let mut busy = CLAUDE_IN_FLIGHT.lock().ok()?;
        if *busy {
            return None;
        }
        *busy = true;
        Some(Self)
    }
}
impl Drop for ClaudeFlight {
    fn drop(&mut self) {
        if let Ok(mut busy) = CLAUDE_IN_FLIGHT.lock() {
            *busy = false;
        }
    }
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn ping_cwd() -> Result<PathBuf, ()> {
    secure_ping_cwd(state_location::primary_state_dir()?.join("ping-cwd"))
}

fn secure_ping_cwd(path: PathBuf) -> Result<PathBuf, ()> {
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => return Err(()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(&path).map_err(|_| ())?;
        }
        Err(_) => return Err(()),
    }
    let metadata = fs::symlink_metadata(&path).map_err(|_| ())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(());
    }
    if fs::read_dir(&path).map_err(|_| ())?.next().is_some() {
        return Err(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(|_| ())?;
    }
    Ok(path)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn first_executable_with_forbidden(
    candidates: impl IntoIterator<Item = PathBuf>,
    forbidden_root: &Path,
) -> Option<PathBuf> {
    // Compare canonical paths on both sides: macOS temp and app paths may sit
    // behind symlinks (for example /var -> /private/var).
    let forbidden_root = forbidden_root
        .canonicalize()
        .unwrap_or_else(|_| forbidden_root.to_path_buf());
    candidates.into_iter().find_map(|candidate| {
        let canonical = candidate.canonicalize().ok()?;
        (!canonical.starts_with(&forbidden_root) && is_executable_file(&canonical))
            .then_some(canonical)
    })
}

fn first_executable(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    first_executable_with_forbidden(candidates, Path::new(CHATGPT_APP_ROOT))
}

fn cli_candidates(home: &Path, candidates: &[&str]) -> Vec<PathBuf> {
    candidates
        .iter()
        .map(|candidate| {
            let path = Path::new(candidate);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                home.join(path)
            }
        })
        .collect()
}

fn codex_cli() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    first_executable(cli_candidates(&home, &CODEX_CLI_CANDIDATES))
}

pub(crate) fn claude_cli() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    first_executable(cli_candidates(&home, &CLAUDE_CLI_CANDIDATES))
}

fn clean_command(binary: &Path, cwd: &Path, codex_home: Option<&Path>) -> Command {
    let mut command = Command::new(binary);
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear();
    if let Some(home) = dirs::home_dir() {
        command.env("HOME", home);
    }
    command.env("PATH", "/usr/bin:/bin").env("LANG", "C.UTF-8");
    if let Some(user) = std::env::var_os("USER") {
        command.env("USER", user);
    }
    if let Some(home) = codex_home {
        command.env("CODEX_HOME", home);
    }
    command
}

/// Builds the credential-refresh status check with the same CLI discovery and
/// sanitized environment as Claude Ping. Its output is deliberately discarded.
pub(crate) enum ClaudeAuthCommandError {
    CliNotFound,
    Setup,
}

pub(crate) fn build_claude_auth_status_command() -> Result<Command, ClaudeAuthCommandError> {
    let binary = claude_cli().ok_or(ClaudeAuthCommandError::CliNotFound)?;
    let cwd = ping_cwd().map_err(|_| ClaudeAuthCommandError::Setup)?;
    let mut command = clean_command(&binary, &cwd, None);
    command
        .args(["auth", "status", "--json"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

fn codex_args(cwd: &Path) -> Vec<String> {
    vec![
        "-C".into(),
        cwd.display().to_string(),
        "-m".into(),
        CODEX_MODEL.into(),
        "-c".into(),
        "model_reasoning_effort=\"none\"".into(),
        "-c".into(),
        "approval_policy=\"never\"".into(),
        "-s".into(),
        "read-only".into(),
        "--disable".into(),
        "plugins".into(),
        "--disable".into(),
        "memories".into(),
        "--disable".into(),
        "shell_tool".into(),
        "--disable".into(),
        "view_image".into(),
        "--disable".into(),
        "sleep_tool".into(),
        "--disable".into(),
        "tool_suggest".into(),
        "--disable".into(),
        "apps".into(),
        "exec".into(),
        "--skip-git-repo-check".into(),
        "--ephemeral".into(),
        "--ignore-user-config".into(),
        "--ignore-rules".into(),
        "--json".into(),
        "Reply 1".into(),
    ]
}

fn claude_args() -> Vec<String> {
    vec![
        "-p".into(),
        "1".into(),
        "--system-prompt".into(),
        "Reply with the single character 1.".into(),
        "--model".into(),
        CLAUDE_MODEL.into(),
        "--tools".into(),
        "".into(),
        "--strict-mcp-config".into(),
        "--setting-sources".into(),
        "".into(),
        "--no-session-persistence".into(),
        "--output-format".into(),
        "json".into(),
        "--max-turns".into(),
        "1".into(),
    ]
}

pub(crate) enum ChildResult {
    Success(Vec<u8>),
    Nonzero,
    Timeout,
    SpawnFailed,
}

fn terminate_process_group(pgid: u32) {
    let group = format!("-{pgid}");
    let _ = Command::new("/bin/kill").args(["-TERM", &group]).status();
    thread::sleep(Duration::from_secs(2));
    let _ = Command::new("/bin/kill").args(["-KILL", &group]).status();
}

fn run_child(command: Command, timeout: Duration) -> ChildResult {
    run_child_after_arm(command, timeout, || {})
}

fn run_child_after_arm(command: Command, timeout: Duration, arm: impl FnOnce()) -> ChildResult {
    run_child_with_output(command, timeout, arm, true)
}

fn run_child_with_output(
    mut command: Command,
    timeout: Duration,
    arm: impl FnOnce(),
    capture_stdout: bool,
) -> ChildResult {
    #[cfg(unix)]
    command.process_group(0);
    let Ok(mut child) = command.spawn() else {
        return ChildResult::SpawnFailed;
    };
    let pid = child.id();
    let stdout_receiver = if capture_stdout {
        let Some(mut stdout) = child.stdout.take() else {
            terminate_process_group(pid);
            let _ = child.wait();
            return ChildResult::SpawnFailed;
        };
        let (stdout_sender, stdout_receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let mut output = Vec::new();
            let _ = stdout_sender.send(stdout.read_to_end(&mut output).map(|_| output));
        });
        Some(stdout_receiver)
    } else {
        None
    };
    arm();
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    terminate_process_group(pid);
                    let _ = child.wait();
                    if let Some(receiver) = &stdout_receiver {
                        let _ = receiver.recv_timeout(Duration::from_secs(2));
                    }
                    return ChildResult::Nonzero;
                }
                let Some(stdout_receiver) = stdout_receiver else {
                    return ChildResult::Success(Vec::new());
                };
                let remaining = timeout.saturating_sub(started.elapsed());
                return match stdout_receiver.recv_timeout(remaining) {
                    Ok(Ok(output)) => ChildResult::Success(output),
                    Ok(Err(_)) => ChildResult::SpawnFailed,
                    Err(_) => {
                        terminate_process_group(pid);
                        let _ = child.wait();
                        let _ = stdout_receiver.recv_timeout(Duration::from_secs(2));
                        ChildResult::Timeout
                    }
                };
            }
            Ok(None) if started.elapsed() >= timeout => {
                terminate_process_group(pid);
                let _ = child.wait();
                if let Some(receiver) = &stdout_receiver {
                    let _ = receiver.recv_timeout(Duration::from_secs(2));
                }
                return ChildResult::Timeout;
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                terminate_process_group(pid);
                let _ = child.wait();
                if let Some(receiver) = &stdout_receiver {
                    let _ = receiver.recv_timeout(Duration::from_secs(2));
                }
                return ChildResult::SpawnFailed;
            }
        }
    }
}

async fn sleep_without_blocking_runtime(delay: Duration) {
    let _ = tauri::async_runtime::spawn_blocking(move || thread::sleep(delay)).await;
}

async fn run_child_without_blocking(command: Command, timeout: Duration) -> ChildResult {
    tauri::async_runtime::spawn_blocking(move || run_child(command, timeout))
        .await
        .unwrap_or(ChildResult::SpawnFailed)
}

/// Uses Ping's process-group timeout mechanism while deliberately discarding
/// both output streams. `auth status` is used only for Claude Code's own
/// credential refresh side effect; QuotaBar never consumes its output.
pub(crate) async fn run_child_discarding_output_without_blocking(
    command: Command,
    timeout: Duration,
) -> ChildResult {
    tauri::async_runtime::spawn_blocking(move || {
        run_child_with_output(command, timeout, || {}, false)
    })
    .await
    .unwrap_or(ChildResult::SpawnFailed)
}

fn codex_tokens(stdout: &[u8]) -> Option<u64> {
    for line in stdout.split(|byte| *byte == b'\n') {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|v| v.as_str()) != Some("turn.completed") {
            continue;
        }
        let usage = value.get("usage")?;
        let input = usage.get("input_tokens").and_then(|v| v.as_u64())?;
        let output = usage.get("output_tokens").and_then(|v| v.as_u64())?;
        return input.checked_add(output);
    }
    None
}

fn claude_tokens(stdout: &[u8]) -> Option<Option<u64>> {
    let value: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    if value.get("type").and_then(|v| v.as_str()) != Some("result")
        || value.get("is_error").and_then(|v| v.as_bool()) != Some(false)
        || value.get("subtype").and_then(|v| v.as_str()) != Some("success")
    {
        return None;
    }
    let tokens = value.get("usage").and_then(|usage| {
        Some(
            usage
                .get("input_tokens")?
                .as_u64()?
                .checked_add(usage.get("output_tokens")?.as_u64()?)?,
        )
    });
    Some(tokens)
}

type CoreFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

fn build_codex_command(profile: &CodexProfile) -> Result<Command, PingOutcome> {
    let Some(binary) = codex_cli() else {
        return Err(PingOutcome::CliNotFound { cli: "codex" });
    };
    let Ok(cwd) = ping_cwd() else {
        return Err(PingOutcome::CliFailed {
            code: "spawnFailed",
        });
    };
    let child_codex_home = profile.home().cloned().or_else(|| {
        std::env::var_os("CODEX_HOME")
            .map(|_| codex::get_codex_home())
            .flatten()
    });
    let mut command = clean_command(&binary, &cwd, child_codex_home.as_deref());
    command.args(codex_args(&cwd));
    Ok(command)
}

fn build_claude_command() -> Result<Command, PingOutcome> {
    let Some(binary) = claude_cli() else {
        return Err(PingOutcome::CliNotFound { cli: "claude" });
    };
    let Ok(cwd) = ping_cwd() else {
        return Err(PingOutcome::CliFailed {
            code: "spawnFailed",
        });
    };
    let mut command = clean_command(&binary, &cwd, None);
    // PING1: multi-account = CLAUDE_CONFIG_DIR per profile; gated on Claude multi-account acquisition
    command.args(claude_args());
    Ok(command)
}

async fn ping_codex_core(
    profile: Option<CodexProfile>,
    force: bool,
    quota_reader: &mut (dyn FnMut() -> CoreFuture<crate::domain::models::CodexRateLimits> + Send),
    clock: &(dyn Fn() -> i64 + Sync),
    runner: &mut (dyn FnMut(Command) -> CoreFuture<ChildResult> + Send),
    command_builder: &mut (dyn FnMut(&CodexProfile) -> Result<Command, PingOutcome> + Send),
    sleeper: &mut (dyn FnMut(Duration) -> CoreFuture<()> + Send),
) -> PingOutcome {
    let Some(profile) = profile else {
        return PingOutcome::ProfileUnavailable;
    };
    let limits = quota_reader().await;
    let Some(primary) = limits.primary.as_ref() else {
        return PingOutcome::QuotaUnreadable;
    };
    if limits.error.is_some() {
        return PingOutcome::QuotaUnreadable;
    }
    if let Err(outcome) = ping_send_decision(
        codex_window_state(Some(primary), clock()),
        force,
        primary.resets_at,
        limits.ordinary_usage_allowed == Some(false),
    ) {
        return outcome;
    }
    let command = match command_builder(&profile) {
        Ok(command) => command,
        Err(outcome) => return outcome,
    };
    let tokens = match runner(command).await {
        ChildResult::Success(stdout) => match codex_tokens(&stdout) {
            Some(tokens) => Some(tokens),
            None => {
                return PingOutcome::CliFailed {
                    code: "noCompletion",
                }
            }
        },
        ChildResult::Nonzero => {
            return PingOutcome::CliFailed {
                code: "nonzeroExit",
            }
        }
        ChildResult::Timeout => return PingOutcome::CliFailed { code: "timeout" },
        ChildResult::SpawnFailed => {
            return PingOutcome::CliFailed {
                code: "spawnFailed",
            }
        }
    };
    // This must remain after the successful child result: confirmation time is
    // measured from CLI completion, never from command startup.
    let completed_at = clock();
    let plan = confirmation_plan(completed_at, &CONFIRM_DELAYS_SECS)
        .expect("constant confirmation staircase and completed timestamp fit i64");
    for step in &plan.steps {
        if step.delay_secs > 0 {
            sleeper(Duration::from_secs(step.delay_secs)).await;
        }
        let limits = quota_reader().await;
        if limits.error.is_none()
            && codex_window_state(limits.primary.as_ref(), clock()) == WindowState::Open
        {
            if let Some(resets_at) = limits.primary.and_then(|primary| primary.resets_at) {
                return PingOutcome::Opened {
                    resets_at,
                    tokens,
                    confirmed_after_secs: step.confirmed_after_secs,
                };
            }
        }
    }
    PingOutcome::SentUnconfirmed {
        tokens,
        expected_resets_at: plan.expected_resets_at,
    }
}

async fn ping_claude_core(
    force: bool,
    quota_reader: &mut (dyn FnMut() -> CoreFuture<crate::domain::models::QuotaData> + Send),
    clock: &(dyn Fn() -> i64 + Sync),
    runner: &mut (dyn FnMut(Command) -> CoreFuture<ChildResult> + Send),
    command_builder: &mut (dyn FnMut() -> Result<Command, PingOutcome> + Send),
    sleeper: &mut (dyn FnMut(Duration) -> CoreFuture<()> + Send),
) -> PingOutcome {
    let quota = quota_reader().await;
    if quota.error.is_some() || !quota.connected || quota.session.is_none() {
        return PingOutcome::QuotaUnreadable;
    }
    if let Err(outcome) = ping_send_decision(
        claude_window_state(quota.session.as_ref(), clock()),
        force,
        quota.session.as_ref().and_then(session_reset_epoch),
        false,
    ) {
        return outcome;
    }
    let command = match command_builder() {
        Ok(command) => command,
        Err(outcome) => return outcome,
    };
    let tokens = match runner(command).await {
        ChildResult::Success(stdout) => match claude_tokens(&stdout) {
            Some(tokens) => tokens,
            None => {
                return PingOutcome::CliFailed {
                    code: "noCompletion",
                }
            }
        },
        ChildResult::Nonzero => {
            return PingOutcome::CliFailed {
                code: "nonzeroExit",
            }
        }
        ChildResult::Timeout => return PingOutcome::CliFailed { code: "timeout" },
        ChildResult::SpawnFailed => {
            return PingOutcome::CliFailed {
                code: "spawnFailed",
            }
        }
    };
    let completed_at = clock();
    let plan = confirmation_plan(completed_at, &CONFIRM_DELAYS_SECS)
        .expect("constant confirmation staircase and completed timestamp fit i64");
    for step in &plan.steps {
        if step.delay_secs > 0 {
            sleeper(Duration::from_secs(step.delay_secs)).await;
        }
        let quota = quota_reader().await;
        if quota.error.is_none()
            && claude_window_state(quota.session.as_ref(), clock()) == WindowState::Open
        {
            if let Some(resets_at) = quota.session.as_ref().and_then(session_reset_epoch) {
                return PingOutcome::Opened {
                    resets_at,
                    tokens,
                    confirmed_after_secs: step.confirmed_after_secs,
                };
            }
        }
    }
    PingOutcome::SentUnconfirmed {
        tokens,
        expected_resets_at: plan.expected_resets_at,
    }
}

pub async fn ping_codex(profile: CodexProfile, force: bool) -> PingOutcome {
    let Some(_flight) = CodexFlight::acquire(&profile) else {
        return PingOutcome::Busy;
    };
    let quota_profile = profile.clone();
    let mut quota_reader = move || {
        let profile = quota_profile.clone();
        Box::pin(async move { codex::fetch_codex_rate_limits_for(&profile).await }) as CoreFuture<_>
    };
    let clock = now_secs;
    let mut runner =
        |command| Box::pin(run_child_without_blocking(command, CLI_TIMEOUT)) as CoreFuture<_>;
    let mut command_builder = build_codex_command;
    let mut sleeper = |delay| Box::pin(sleep_without_blocking_runtime(delay)) as CoreFuture<_>;
    ping_codex_core(
        Some(profile),
        force,
        &mut quota_reader,
        &clock,
        &mut runner,
        &mut command_builder,
        &mut sleeper,
    )
    .await
}

pub async fn ping_claude(force: bool) -> PingOutcome {
    let Some(_flight) = ClaudeFlight::acquire() else {
        return PingOutcome::Busy;
    };
    let mut quota_reader = || Box::pin(claude::fetch_quota()) as CoreFuture<_>;
    let clock = now_secs;
    let mut runner =
        |command| Box::pin(run_child_without_blocking(command, CLI_TIMEOUT)) as CoreFuture<_>;
    let mut command_builder = build_claude_command;
    let mut sleeper = |delay| Box::pin(sleep_without_blocking_runtime(delay)) as CoreFuture<_>;
    ping_claude_core(
        force,
        &mut quota_reader,
        &clock,
        &mut runner,
        &mut command_builder,
        &mut sleeper,
    )
    .await
}

fn session_reset_epoch(session: &crate::domain::models::UsageInfo) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(session.reset_time.as_deref()?)
        .ok()
        .map(|value| value.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{CodexRateLimits, UsageInfo};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn primary(used: f64, resets: Option<i64>, minutes: Option<i64>) -> CodexRateLimitWindow {
        CodexRateLimitWindow {
            used_percent: used,
            resets_at: resets,
            window_minutes: minutes,
        }
    }
    fn session(used: f64, reset: Option<&str>) -> UsageInfo {
        UsageInfo {
            used,
            limit: 100.0,
            percentage: used,
            reset_time: reset.map(str::to_string),
        }
    }

    fn codex_limits(primary: CodexRateLimitWindow) -> CodexRateLimits {
        CodexRateLimits {
            connected: true,
            plan_type: None,
            primary: Some(primary),
            secondary: None,
            credits: None,
            ordinary_usage_allowed: Some(true),
            error: None,
        }
    }

    fn test_profile() -> CodexProfile {
        CodexProfile::from_registry("test".to_string(), PathBuf::from("/tmp/quotabar-ping-test"))
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "quotabar-ping-{name}-{}-{}-{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn fake_cli(root: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(root).unwrap();
        let path = root.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn codex_window_vectors_are_numbered_and_fail_closed() {
        let now = 1_000_000;
        // C01..C10 are duplicated verbatim in tests/ping_window.test.ts.
        for (case, input, expected) in [
            (
                "C01 threshold-minus-one",
                Some(primary(0.0, Some(now + 17_939), Some(300))),
                WindowState::Open,
            ),
            (
                "C02 threshold-equal",
                Some(primary(0.0, Some(now + 17_940), Some(300))),
                WindowState::Closed,
            ),
            (
                "C03 threshold-plus-one",
                Some(primary(0.0, Some(now + 17_941), Some(300))),
                WindowState::Closed,
            ),
            (
                "C04 used-positive",
                Some(primary(0.1, Some(now + 18_000), Some(300))),
                WindowState::Open,
            ),
            (
                "C05 expired-reset",
                Some(primary(0.0, Some(now - 1), Some(300))),
                WindowState::Open,
            ),
            (
                "C06 missing-reset",
                Some(primary(0.0, None, Some(300))),
                WindowState::Unknown,
            ),
            (
                "C07 missing-window",
                Some(primary(0.0, Some(now + 1), None)),
                WindowState::Unknown,
            ),
            (
                "C08 negative-used",
                Some(primary(-0.1, Some(now + 18_000), Some(300))),
                WindowState::Unknown,
            ),
            (
                "C09 non-finite-used",
                Some(primary(f64::NAN, Some(now + 18_000), Some(300))),
                WindowState::Unknown,
            ),
            (
                "C10 negative-reset",
                Some(primary(0.0, Some(-1), Some(300))),
                WindowState::Unknown,
            ),
        ] {
            assert_eq!(codex_window_state(input.as_ref(), now), expected, "{case}");
        }
        assert!(
            serde_json::from_str::<CodexRateLimitWindow>(
                r#"{"usedPercent":"wrong","resetsAt":1018000,"windowMinutes":300}"#,
            )
            .is_err(),
            "C11 wrong-type is rejected before the window function"
        );
    }

    #[test]
    fn claude_window_vectors_are_numbered_and_fail_closed() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-05T11:00:00Z")
            .unwrap()
            .timestamp();
        let placeholder = chrono::DateTime::from_timestamp(now + 18_000, 0)
            .unwrap()
            .to_rfc3339();
        let anchor = chrono::DateTime::from_timestamp(now + 7_200, 0)
            .unwrap()
            .to_rfc3339();
        // L01..L21 are duplicated verbatim in tests/ping_window.test.ts.
        for (case, input, expected) in [
            (
                "L01 null-reset",
                Some(session(0.0, None)),
                WindowState::Closed,
            ),
            (
                "L02 placeholder",
                Some(session(0.0, Some(&placeholder))),
                WindowState::Unknown,
            ),
            (
                "L03 anchor",
                Some(session(0.0, Some(&anchor))),
                WindowState::Open,
            ),
            (
                "L04 used-positive",
                Some(session(1.0, None)),
                WindowState::Open,
            ),
            (
                "L05 expired-reset",
                Some(session(0.0, Some("1970-01-01T00:00:00Z"))),
                WindowState::Unknown,
            ),
            (
                "L06 negative-utilization",
                Some(session(-1.0, None)),
                WindowState::Unknown,
            ),
            (
                "L07 non-finite-utilization",
                Some(session(f64::NAN, None)),
                WindowState::Unknown,
            ),
            (
                "L08 non-rfc-english",
                Some(session(0.0, Some("October 5, 2026 12:00:00 GMT"))),
                WindowState::Unknown,
            ),
            (
                "L09 non-rfc-slashes",
                Some(session(0.0, Some("2026/10/05 12:00:00"))),
                WindowState::Unknown,
            ),
            (
                "L10 invalid-date",
                Some(session(0.0, Some("not-a-date"))),
                WindowState::Unknown,
            ),
            ("L11 missing-session", None, WindowState::Unknown),
            (
                "L12 threshold-minus-one",
                Some(session(
                    0.0,
                    Some(
                        &chrono::DateTime::from_timestamp(now + 17_939, 0)
                            .unwrap()
                            .to_rfc3339(),
                    ),
                )),
                WindowState::Open,
            ),
            (
                "L13 threshold-equal",
                Some(session(
                    0.0,
                    Some(
                        &chrono::DateTime::from_timestamp(now + 17_940, 0)
                            .unwrap()
                            .to_rfc3339(),
                    ),
                )),
                WindowState::Unknown,
            ),
            (
                "L14 threshold-plus-one",
                Some(session(
                    0.0,
                    Some(
                        &chrono::DateTime::from_timestamp(now + 17_941, 0)
                            .unwrap()
                            .to_rfc3339(),
                    ),
                )),
                WindowState::Unknown,
            ),
            (
                "L15 lowercase-t-and-z",
                Some(session(0.0, Some("2026-10-05t12:00:00z"))),
                WindowState::Unknown,
            ),
            (
                "L16 space-separator",
                Some(session(0.0, Some("2026-10-05 12:00:00Z"))),
                WindowState::Unknown,
            ),
            (
                "L17 invalid-calendar-day",
                Some(session(0.0, Some("2026-09-31T12:00:00Z"))),
                WindowState::Unknown,
            ),
            (
                "L18 non-leap-february-29",
                Some(session(0.0, Some("2026-02-29T12:00:00Z"))),
                WindowState::Unknown,
            ),
            (
                "L19 leap-february-29",
                Some(session(0.0, Some("2028-02-29T12:00:00Z"))),
                WindowState::Open,
            ),
            (
                "L20 hour-24",
                Some(session(0.0, Some("2026-10-05T24:00:00Z"))),
                WindowState::Unknown,
            ),
            (
                "L21 leap-second",
                Some(session(0.0, Some("2026-10-05T12:00:60Z"))),
                WindowState::Unknown,
            ),
        ] {
            let vector_now = match case {
                "L17 invalid-calendar-day" => {
                    chrono::DateTime::parse_from_rfc3339("2026-10-01T10:00:00Z")
                        .unwrap()
                        .timestamp()
                }
                "L18 non-leap-february-29" => {
                    chrono::DateTime::parse_from_rfc3339("2026-03-01T10:00:00Z")
                        .unwrap()
                        .timestamp()
                }
                "L19 leap-february-29" => {
                    chrono::DateTime::parse_from_rfc3339("2028-02-29T10:00:00Z")
                        .unwrap()
                        .timestamp()
                }
                "L20 hour-24" => chrono::DateTime::parse_from_rfc3339("2026-10-05T22:00:00Z")
                    .unwrap()
                    .timestamp(),
                _ => now,
            };
            assert_eq!(
                claude_window_state(input.as_ref(), vector_now),
                expected,
                "{case}"
            );
        }
        assert!(
            serde_json::from_str::<UsageInfo>(r#"{"used":0,"limit":100,"percentage":"wrong"}"#,)
                .is_err(),
            "L11 wrong-type is rejected before the window function"
        );
    }
    #[test]
    fn confirmation_staircase_is_compile_time_safe_and_rejects_bad_values() {
        assert_eq!(CONFIRM_TOTAL_SECS, 510);
        assert!(staircase_is_safe(&CONFIRM_DELAYS_SECS));
        assert!(!staircase_is_safe(&[0, 30, 60]));
    }

    #[test]
    fn confirmation_plan_uses_post_exit_time_and_cumulative_staircase_offsets() {
        let plan = confirmation_plan(1_000_000, &CONFIRM_DELAYS_SECS).unwrap();
        assert_eq!(
            plan.steps
                .iter()
                .map(|step| step.confirmed_after_secs)
                .collect::<Vec<_>>(),
            [0, 30, 90, 210, 510],
        );
        assert_eq!(plan.expected_resets_at, 1_018_000);
    }

    #[test]
    fn codex_command_core_skips_runner_for_profile_unavailable_and_unknown_without_force() {
        let runner_calls = Arc::new(AtomicU64::new(0));
        let unavailable_calls = Arc::clone(&runner_calls);
        let mut unavailable_reader =
            || Box::pin(async { CodexRateLimits::disconnected("not read") }) as CoreFuture<_>;
        let clock = || 1_000;
        let mut unavailable_runner = move |_: Command| {
            unavailable_calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { ChildResult::SpawnFailed }) as CoreFuture<_>
        };
        let mut builder = |_: &CodexProfile| Ok(Command::new("unused"));
        let mut no_wait = |_: Duration| Box::pin(async {}) as CoreFuture<_>;
        assert_eq!(
            tauri::async_runtime::block_on(ping_codex_core(
                None,
                false,
                &mut unavailable_reader,
                &clock,
                &mut unavailable_runner,
                &mut builder,
                &mut no_wait,
            )),
            PingOutcome::ProfileUnavailable,
        );
        assert_eq!(runner_calls.load(Ordering::Relaxed), 0);

        let unknown_calls = Arc::new(AtomicU64::new(0));
        let mut unknown_reader = || {
            Box::pin(async { codex_limits(primary(-1.0, Some(20_000), Some(300))) })
                as CoreFuture<_>
        };
        let unknown_runner_calls = Arc::clone(&unknown_calls);
        let mut unknown_runner = move |_: Command| {
            unknown_runner_calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { ChildResult::SpawnFailed }) as CoreFuture<_>
        };
        assert_eq!(
            tauri::async_runtime::block_on(ping_codex_core(
                Some(test_profile()),
                false,
                &mut unknown_reader,
                &clock,
                &mut unknown_runner,
                &mut builder,
                &mut no_wait,
            )),
            PingOutcome::ConfirmationRequired,
        );
        assert_eq!(unknown_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn codex_command_core_reads_clock_only_after_a_successful_runner() {
        let clock_value = Arc::new(AtomicI64::new(1_000));
        let quota = codex_limits(primary(0.0, Some(20_000), Some(300)));
        let mut quota_reader = move || {
            let quota = quota.clone();
            Box::pin(async move { quota }) as CoreFuture<_>
        };
        let runner_clock = Arc::clone(&clock_value);
        let mut runner = move |_: Command| {
            runner_clock.store(2_000, Ordering::Relaxed);
            Box::pin(async {
                ChildResult::Success(
                    br#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}"#
                        .to_vec(),
                )
            }) as CoreFuture<_>
        };
        let clock_value_for_read = Arc::clone(&clock_value);
        let clock = move || clock_value_for_read.load(Ordering::Relaxed);
        let mut builder = |_: &CodexProfile| Ok(Command::new("unused"));
        let mut no_wait = |_: Duration| Box::pin(async {}) as CoreFuture<_>;
        assert_eq!(
            tauri::async_runtime::block_on(ping_codex_core(
                Some(test_profile()),
                false,
                &mut quota_reader,
                &clock,
                &mut runner,
                &mut builder,
                &mut no_wait,
            )),
            PingOutcome::SentUnconfirmed {
                tokens: Some(3),
                expected_resets_at: 20_000,
            },
        );
    }

    #[test]
    fn claude_command_core_reads_clock_only_after_a_successful_runner() {
        let clock_value = Arc::new(AtomicI64::new(1_000));
        let quota = crate::domain::models::QuotaData::connected(
            Some(session(0.0, None)),
            None,
            None,
            None,
            None,
            None,
        );
        let mut quota_reader = move || {
            let quota = quota.clone();
            Box::pin(async move { quota }) as CoreFuture<_>
        };
        let runner_clock = Arc::clone(&clock_value);
        let mut runner = move |_: Command| {
            runner_clock.store(2_000, Ordering::Relaxed);
            Box::pin(async {
                ChildResult::Success(
                    br#"{"type":"result","is_error":false,"subtype":"success","usage":{"input_tokens":1,"output_tokens":2}}"#
                        .to_vec(),
                )
            }) as CoreFuture<_>
        };
        let clock_value_for_read = Arc::clone(&clock_value);
        let clock = move || clock_value_for_read.load(Ordering::Relaxed);
        let mut builder = || Ok(Command::new("unused"));
        let mut no_wait = |_: Duration| Box::pin(async {}) as CoreFuture<_>;
        assert_eq!(
            tauri::async_runtime::block_on(ping_claude_core(
                false,
                &mut quota_reader,
                &clock,
                &mut runner,
                &mut builder,
                &mut no_wait,
            )),
            PingOutcome::SentUnconfirmed {
                tokens: Some(3),
                expected_resets_at: 20_000,
            },
        );
    }

    #[test]
    fn ping_send_decision_requires_confirmation_for_unknown_unless_forced() {
        assert_eq!(
            ping_send_decision(WindowState::Unknown, false, Some(1), false),
            Err(PingOutcome::ConfirmationRequired),
        );
        assert_eq!(
            ping_send_decision(WindowState::Unknown, true, Some(1), false),
            Ok(())
        );
        assert_eq!(
            ping_send_decision(WindowState::Open, false, Some(1), false),
            Err(PingOutcome::AlreadyOpen { resets_at: Some(1) }),
        );
        assert_eq!(
            ping_send_decision(WindowState::Closed, false, None, false),
            Ok(())
        );
        assert_eq!(
            ping_send_decision(WindowState::Closed, true, None, true),
            Err(PingOutcome::Blocked),
        );
    }

    #[test]
    fn locator_rejects_symlinked_chatgpt_and_injects_candidate_root() {
        let root = temp_root("locator");
        let home = root.join("home");
        let first = home.join("bin/codex");
        let second = home.join(".local/bin/codex");
        let bundle = root.join("Applications/ChatGPT.app/Contents/Resources/codex");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::create_dir_all(bundle.parent().unwrap()).unwrap();
        fs::write(&first, "x").unwrap();
        fs::set_permissions(&first, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&second, "x").unwrap();
        fs::set_permissions(&second, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&bundle, "x").unwrap();
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o755)).unwrap();
        let linked = home.join("bin/linked-codex");
        symlink(&bundle, &linked).unwrap();
        assert_eq!(
            first_executable_with_forbidden(
                [first.clone(), linked, second.clone()],
                root.join("Applications/ChatGPT.app").as_path(),
            ),
            Some(second.canonicalize().unwrap()),
        );
        assert_eq!(
            cli_candidates(&home, &CODEX_CLI_CANDIDATES)[..2],
            [home.join("bin/codex"), home.join(".local/bin/codex")],
        );
        assert_eq!(first_executable([root.join("none")]), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn secure_ping_cwd_rejects_symlinks_non_directories_and_nonempty_directories() {
        let root = temp_root("cwd");
        let directory = root.join("directory");
        assert_eq!(secure_ping_cwd(directory.clone()), Ok(directory.clone()));
        fs::write(directory.join("not-empty"), "x").unwrap();
        assert!(secure_ping_cwd(directory).is_err());
        let file = root.join("file");
        fs::write(&file, "x").unwrap();
        assert!(secure_ping_cwd(file).is_err());
        let linked = root.join("linked");
        symlink(root.join("missing"), &linked).unwrap();
        assert!(secure_ping_cwd(linked).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn argv_is_exact_and_completion_parsers_only_expose_token_count() {
        let cwd = Path::new("/private/ping-cwd");
        assert_eq!(
            codex_args(cwd),
            vec![
                "-C",
                "/private/ping-cwd",
                "-m",
                CODEX_MODEL,
                "-c",
                "model_reasoning_effort=\"none\"",
                "-c",
                "approval_policy=\"never\"",
                "-s",
                "read-only",
                "--disable",
                "plugins",
                "--disable",
                "memories",
                "--disable",
                "shell_tool",
                "--disable",
                "view_image",
                "--disable",
                "sleep_tool",
                "--disable",
                "tool_suggest",
                "--disable",
                "apps",
                "exec",
                "--skip-git-repo-check",
                "--ephemeral",
                "--ignore-user-config",
                "--ignore-rules",
                "--json",
                "Reply 1",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        );
        assert_eq!(
            claude_args(),
            vec![
                "-p",
                "1",
                "--system-prompt",
                "Reply with the single character 1.",
                "--model",
                CLAUDE_MODEL,
                "--tools",
                "",
                "--strict-mcp-config",
                "--setting-sources",
                "",
                "--no-session-persistence",
                "--output-format",
                "json",
                "--max-turns",
                "1"
            ]
        );
        assert_eq!(codex_tokens(b"{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}\nsecret sk-no-leak"), Some(5));
        assert_eq!(claude_tokens(b"{\"type\":\"result\",\"is_error\":false,\"subtype\":\"success\",\"usage\":{\"input_tokens\":2,\"output_tokens\":3},\"result\":\"/Users/a@b sk-no-leak\"}"), Some(Some(5)));
        assert_eq!(
            claude_tokens(b"{\"type\":\"result\",\"is_error\":true}"),
            None
        );
    }

    #[test]
    fn synthetic_children_cover_success_failure_no_completion_timeout_and_closed_stdin() {
        let root = temp_root("children");
        let cwd = root.join("cwd");
        fs::create_dir_all(&cwd).unwrap();
        let success = fake_cli(&root, "success", "read ignored || true; printf '%s' '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":8}}'");
        let no_completion = fake_cli(
            &root,
            "no-completion",
            "printf '%s' '{\"type\":\"event\",\"secret\":\"sk-never-leaked\"}'",
        );
        let nonzero = fake_cli(&root, "nonzero", "exit 7");
        let slow = fake_cli(&root, "slow", "exec sleep 2");
        let command = clean_command(&success, &cwd, None);
        let ChildResult::Success(stdout) = run_child(command, Duration::from_secs(1)) else {
            panic!("success fake CLI failed")
        };
        assert_eq!(codex_tokens(&stdout), Some(15));
        let ChildResult::Success(stdout) = run_child(
            clean_command(&no_completion, &cwd, None),
            Duration::from_secs(1),
        ) else {
            panic!("completion fake CLI did not run")
        };
        assert_eq!(codex_tokens(&stdout), None);
        assert!(matches!(
            run_child(clean_command(&nonzero, &cwd, None), Duration::from_secs(1)),
            ChildResult::Nonzero
        ));
        assert!(matches!(
            run_child(clean_command(&slow, &cwd, None), Duration::from_millis(40)),
            ChildResult::Timeout
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_child_drains_large_stdout_before_the_child_exits() {
        let root = temp_root("large-stdout");
        let cwd = root.join("cwd");
        fs::create_dir_all(&cwd).unwrap();
        let large_stdout = fake_cli(
            &root,
            "large-stdout",
            "dd if=/dev/zero bs=131073 count=1 2>/dev/null; printf '%s' '{\"type\":\"turn.completed\"}'",
        );
        let ChildResult::Success(stdout) = run_child(
            clean_command(&large_stdout, &cwd, None),
            Duration::from_secs(2),
        ) else {
            panic!("large stdout fake CLI did not complete before its timeout");
        };
        assert!(stdout.len() > 128 * 1024);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn timeout_terminates_the_whole_child_process_group() {
        let root = temp_root("process-group");
        let cwd = root.join("cwd");
        let pid_file = root.join("grandchild.pid");
        fs::create_dir_all(&cwd).unwrap();
        let script = fake_cli(
            &root,
            "grandchild",
            "sleep 30 & grandchild=$!; printf '%s' \"$grandchild\" > \"$1\"; wait",
        );
        let mut command = clean_command(&script, &cwd, None);
        command.arg(&pid_file);
        let barrier = Arc::new(Barrier::new(2));
        let child_barrier = Arc::clone(&barrier);
        let timeout = thread::spawn(move || {
            run_child_after_arm(command, Duration::from_secs(1), move || {
                child_barrier.wait();
            })
        });
        let ready_by = Instant::now() + Duration::from_secs(2);
        while !pid_file.exists() {
            if Instant::now() >= ready_by {
                barrier.wait();
                let _ = timeout.join();
                let _ = fs::remove_dir_all(root);
                panic!("grandchild pid file was not ready within two seconds");
            }
            thread::sleep(Duration::from_millis(10));
        }
        barrier.wait();
        assert!(matches!(timeout.join().unwrap(), ChildResult::Timeout));
        let grandchild = fs::read_to_string(&pid_file).unwrap();
        let pid = grandchild.trim();
        for _ in 0..20 {
            if !Command::new("/bin/kill")
                .args(["-0", pid])
                .status()
                .unwrap()
                .success()
            {
                let _ = fs::remove_dir_all(root);
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = fs::remove_dir_all(root);
        panic!("grandchild remained after process-group timeout");
    }

    #[test]
    fn clean_environment_has_only_the_allowlist_keys_and_values() {
        let root = temp_root("environment");
        let cwd = root.join("cwd");
        fs::create_dir_all(&cwd).unwrap();
        let cli = fake_cli(&root, "env", "printf '%s' '{\"type\":\"result\",\"is_error\":false,\"subtype\":\"success\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}'");
        let command = clean_command(&cli, &cwd, Some(Path::new("/isolated-codex-home")));
        let environment = command
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut expected = std::collections::BTreeMap::from([
            ("CODEX_HOME".to_string(), "/isolated-codex-home".to_string()),
            (
                "HOME".to_string(),
                dirs::home_dir().unwrap().display().to_string(),
            ),
            ("LANG".to_string(), "C.UTF-8".to_string()),
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ]);
        if let Some(user) = std::env::var_os("USER") {
            expected.insert("USER".to_string(), user.to_string_lossy().into_owned());
        }
        assert_eq!(environment, expected);
        assert!(matches!(
            run_child(command, Duration::from_secs(1)),
            ChildResult::Success(_)
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mutual_exclusion_covers_claude_two_routes_early_return_and_panic() {
        let root = temp_root("locks");
        let one_home = root.join("one");
        let two_home = root.join("two");
        fs::create_dir_all(&one_home).unwrap();
        fs::create_dir_all(&two_home).unwrap();
        let one = CodexProfile::from_registry("one".into(), one_home);
        let two = CodexProfile::from_registry("two".into(), two_home);
        let barrier = Arc::new(Barrier::new(2));
        let first_one = one.clone();
        let first_barrier = barrier.clone();
        let first = thread::spawn(move || {
            let _guard = CodexFlight::acquire(&first_one).expect("first route acquires");
            first_barrier.wait();
            thread::sleep(Duration::from_millis(10));
        });
        let second_two = two.clone();
        let second_barrier = barrier.clone();
        let second = thread::spawn(move || {
            let _guard = CodexFlight::acquire(&second_two).expect("other route is concurrent");
            second_barrier.wait();
        });
        first.join().unwrap();
        second.join().unwrap();
        assert!(CodexFlight::acquire(&one).is_some());
        {
            let _claude = ClaudeFlight::acquire().expect("Claude key acquires");
            assert!(ClaudeFlight::acquire().is_none());
        }
        assert!(ClaudeFlight::acquire().is_some());
        fn acquire_then_return(profile: &CodexProfile) {
            let _guard = CodexFlight::acquire(profile).expect("early-return guard");
        }
        acquire_then_return(&one);
        assert!(CodexFlight::acquire(&one).is_some());
        let _ = std::panic::catch_unwind(|| {
            let _guard = CodexFlight::acquire(&one).expect("panic guard");
            panic!("test only");
        });
        assert!(CodexFlight::acquire(&one).is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ping_outcome_fixture_is_the_exact_rust_ipc_contract() {
        let fixtures: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../tests/fixtures/ping_outcomes.json"))
                .unwrap();
        for expected in fixtures {
            let outcome = match expected["kind"].as_str().unwrap() {
                "opened" => PingOutcome::Opened {
                    resets_at: expected["resetsAt"].as_i64().unwrap(),
                    tokens: expected["tokens"].as_u64(),
                    confirmed_after_secs: expected["confirmedAfterSecs"].as_u64().unwrap(),
                },
                "sentUnconfirmed" => PingOutcome::SentUnconfirmed {
                    tokens: expected["tokens"].as_u64(),
                    expected_resets_at: expected["expectedResetsAt"].as_i64().unwrap(),
                },
                "alreadyOpen" => PingOutcome::AlreadyOpen {
                    resets_at: expected["resetsAt"].as_i64(),
                },
                "blocked" => PingOutcome::Blocked,
                "busy" => PingOutcome::Busy,
                "cliNotFound" => PingOutcome::CliNotFound {
                    cli: match expected["cli"].as_str().unwrap() {
                        "codex" => "codex",
                        "claude" => "claude",
                        other => panic!("unexpected cli fixture value: {other}"),
                    },
                },
                "cliFailed" => PingOutcome::CliFailed {
                    code: match expected["code"].as_str().unwrap() {
                        "nonzeroExit" => "nonzeroExit",
                        "noCompletion" => "noCompletion",
                        "timeout" => "timeout",
                        "spawnFailed" => "spawnFailed",
                        other => panic!("unexpected failure fixture value: {other}"),
                    },
                },
                "profileUnavailable" => PingOutcome::ProfileUnavailable,
                "quotaUnreadable" => PingOutcome::QuotaUnreadable,
                "confirmationRequired" => PingOutcome::ConfirmationRequired,
                other => panic!("unexpected ping outcome fixture kind: {other}"),
            };
            assert_eq!(serde_json::to_value(outcome).unwrap(), expected);
        }
    }
}

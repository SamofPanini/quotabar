//! Append-only Claude quota history. This deliberately keeps only a small
//! in-memory tail; quota fetches never scan the history file.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::state_location;

const HISTORY_FILE: &str = "quota-history.jsonl";
const RETENTION_DAYS: i64 = 120;
const PRUNE_INTERVAL: Duration = Duration::hours(24);
const HEARTBEAT_INTERVAL: Duration = Duration::minutes(30);
const TAIL_LIMIT: u64 = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Window {
    pct: f64,
    resets_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Record {
    v: u8,
    t: String,
    provider: String,
    windows: BTreeMap<String, Window>,
}

#[derive(Default)]
struct HistoryState {
    entries: HashMap<PathBuf, PathHistory>,
}

#[derive(Default)]
struct PathHistory {
    last: Option<Record>,
    last_pruned_at: Option<DateTime<Utc>>,
}

fn history_state() -> &'static Mutex<HistoryState> {
    static STATE: OnceLock<Mutex<HistoryState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HistoryState::default()))
}

pub(crate) fn initialize() {
    let Ok(directory) = state_location::primary_state_dir() else {
        eprintln!("[QuotaHistory] state directory is unavailable");
        return;
    };
    let path = directory.join(HISTORY_FILE);
    initialize_at(&path);
    // One scheduler per process: a repeated `initialize` must not stack loops
    // (and their 24h sleeper threads).
    static SCHEDULER_STARTED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    if SCHEDULER_STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    // Pruning reads and rewrites the whole file, so keep it off the quota
    // request path.  The startup pass and subsequent daily passes serialize
    // with appends through `history_state` below.
    tauri::async_runtime::spawn(async move {
        loop {
            let path = path.clone();
            let _ = tauri::async_runtime::spawn_blocking(move || {
                prune_if_due_at(&path, Utc::now());
            })
            .await;
            sleep_until_next_prune().await;
        }
    });
}

async fn sleep_until_next_prune() {
    let (sender, mut receiver) = tauri::async_runtime::channel(1);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(24 * 60 * 60));
        let _ = sender.blocking_send(());
    });
    let _ = receiver.recv().await;
}

/// Record a successfully parsed, real 200 response. Callers deliberately pass
/// the raw API value so the on-disk key names remain independent of UI aliases.
pub(crate) fn record_success(data: &Value, status: reqwest::StatusCode, now: DateTime<Utc>) {
    let Ok(directory) = state_location::primary_state_dir() else {
        eprintln!("[QuotaHistory] state directory is unavailable");
        return;
    };
    record_success_at_status(&directory.join(HISTORY_FILE), data, status, now);
}

fn record_success_at_status(
    path: &Path,
    data: &Value,
    status: reqwest::StatusCode,
    now: DateTime<Utc>,
) {
    if status != reqwest::StatusCode::OK {
        return;
    }
    record_success_at(path, data, now);
}

fn initialize_at(path: &Path) {
    let mut state = history_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.entries.contains_key(path) {
        return;
    }
    state.entries.insert(
        path.to_path_buf(),
        PathHistory {
            last: read_last_record(path),
            last_pruned_at: None,
        },
    );
}

fn record_success_at(path: &Path, data: &Value, now: DateTime<Utc>) {
    // This is intentionally only a tail read on first use.  Full-file prune
    // work belongs to the startup/background task above.
    initialize_at(path);
    let record = Record {
        v: 1,
        t: now.to_rfc3339_opts(SecondsFormat::Secs, true),
        provider: "claude".into(),
        windows: windows_from_response(data),
    };
    if record.windows.is_empty() {
        return;
    }
    let mut state = history_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = state.entries.get_mut(path).expect("history initialized");
    let should_write = match entry.last.as_ref() {
        None => true,
        Some(previous) if !windows_match_for_change(&previous.windows, &record.windows) => true,
        Some(previous) => DateTime::parse_from_rfc3339(&previous.t)
            .map(|time| now - time.with_timezone(&Utc) >= HEARTBEAT_INTERVAL)
            .unwrap_or(true),
    };
    if !should_write {
        return;
    }
    match append_record(path, &record) {
        Ok(()) => entry.last = Some(record),
        Err(error) => eprintln!("[QuotaHistory] append failed: {error}"),
    }
}

fn windows_match_for_change(
    previous: &BTreeMap<String, Window>,
    current: &BTreeMap<String, Window>,
) -> bool {
    previous.len() == current.len()
        && previous.iter().all(|(key, previous_window)| {
            current.get(key).is_some_and(|current_window| {
                previous_window.pct == current_window.pct
                    && resets_at_matches(
                        previous_window.resets_at.as_deref(),
                        current_window.resets_at.as_deref(),
                    )
            })
        })
}

fn resets_at_matches(previous: Option<&str>, current: Option<&str>) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => {
            match (
                DateTime::parse_from_rfc3339(previous),
                DateTime::parse_from_rfc3339(current),
            ) {
                (Ok(previous), Ok(current)) => {
                    previous.timestamp() / 60 == current.timestamp() / 60
                }
                _ => previous == current,
            }
        }
        (None, None) => true,
        _ => false,
    }
}

fn windows_from_response(data: &Value) -> BTreeMap<String, Window> {
    let mut windows = BTreeMap::new();
    for key in [
        "five_hour",
        "seven_day",
        "seven_day_opus",
        "seven_day_sonnet",
        "seven_day_omelette",
    ] {
        if let Some(window) = direct_window(data.get(key)) {
            windows.insert(key.into(), window);
        }
    }
    let fable = [
        "seven_day_fable5",
        "seven_day_fable_5",
        "seven_day_fable",
        "seven_day_claude_fable5",
        "seven_day_claude_fable_5",
    ]
    .into_iter()
    .find_map(|key| direct_window(data.get(key)))
    .or_else(|| weekly_scoped_fable(data));
    if let Some(window) = fable {
        windows.insert("fable5".into(), window);
    }
    windows
}

fn direct_window(value: Option<&Value>) -> Option<Window> {
    let value = value?.as_object()?;
    Some(Window {
        pct: value.get("utilization")?.as_f64()?,
        resets_at: value
            .get("resets_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn weekly_scoped_fable(data: &Value) -> Option<Window> {
    data.get("limits")?.as_array()?.iter().find_map(|limit| {
        if limit.get("group")?.as_str()? != "weekly"
            || limit.get("kind")?.as_str()? != "weekly_scoped"
            || !limit
                .get("scope")?
                .get("model")?
                .get("display_name")?
                .as_str()?
                .eq_ignore_ascii_case("Fable")
        {
            return None;
        }
        Some(Window {
            pct: limit.get("percent")?.as_f64()?,
            resets_at: limit
                .get("resets_at")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    })
}

fn append_record(path: &Path, record: &Record) -> Result<(), String> {
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    set_private_permissions(path)?;
    serde_json::to_writer(&mut file, record).map_err(|error| error.to_string())?;
    file.write_all(b"\n").map_err(|error| error.to_string())?;
    file.flush().map_err(|error| error.to_string())
}

fn read_last_record(path: &Path) -> Option<Record> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(TAIL_LIMIT);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let line = std::str::from_utf8(&tail)
        .ok()?
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())?;
    serde_json::from_str(line).ok()
}

fn prune_if_due_at(path: &Path, now: DateTime<Utc>) {
    prune_if_due_at_with_before_rename(path, now, || {});
}

fn prune_if_due_at_with_before_rename<F>(path: &Path, now: DateTime<Utc>, before_rename: F)
where
    F: FnOnce(),
{
    let mut state = history_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = state.entries.entry(path.to_path_buf()).or_default();
    if entry
        .last_pruned_at
        .is_some_and(|last| now - last < PRUNE_INTERVAL)
    {
        return;
    }
    // Keep this lock through read -> temporary write -> rename so a concurrent
    // append cannot land in the old inode and be lost at rename.
    if let Err(error) = prune_at_with_before_rename(path, now, before_rename) {
        eprintln!("[QuotaHistory] prune failed: {error}");
    } else {
        entry.last_pruned_at = Some(now);
    }
}

fn prune_at(path: &Path, now: DateTime<Utc>) -> Result<(), String> {
    prune_at_with_before_rename(path, now, || {})
}

fn prune_at_with_before_rename<F>(
    path: &Path,
    now: DateTime<Utc>,
    before_rename: F,
) -> Result<(), String>
where
    F: FnOnce(),
{
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let cutoff = now - Duration::days(RETENTION_DAYS);
    let mut invalid = 0usize;
    let retained: Vec<&str> = contents
        .lines()
        .filter(|line| match serde_json::from_str::<Record>(line) {
            Ok(record) => DateTime::parse_from_rfc3339(&record.t)
                .map(|time| time.with_timezone(&Utc) >= cutoff)
                .unwrap_or_else(|_| {
                    invalid += 1;
                    false
                }),
            Err(_) => {
                invalid += 1;
                false
            }
        })
        .collect();
    if invalid > 0 {
        eprintln!("[QuotaHistory] prune dropped {invalid} unparseable history lines");
    }
    if retained.len() == contents.lines().count() {
        return Ok(());
    }
    let temporary = temporary_path(path);
    {
        let mut file = File::create(&temporary).map_err(|error| error.to_string())?;
        set_private_permissions(&temporary)?;
        for line in retained {
            file.write_all(line.as_bytes())
                .map_err(|error| error.to_string())?;
            file.write_all(b"\n").map_err(|error| error.to_string())?;
        }
        file.flush().map_err(|error| error.to_string())?;
    }
    before_rename();
    fs::rename(&temporary, path).map_err(|error| error.to_string())
}

fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension("jsonl.tmp")
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::mpsc;
    use std::thread;

    fn temporary_directory(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("quotabar-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory.canonicalize().unwrap()
    }

    #[test]
    fn writes_line_with_v1_schema_on_fresh_success() {
        let directory = temporary_directory("quota-history-schema");
        let path = directory.join(HISTORY_FILE);
        let now = DateTime::parse_from_rfc3339("2026-10-19T11:41:05Z")
            .unwrap()
            .with_timezone(&Utc);
        record_success_at(
            &path,
            &json!({"five_hour":{"utilization":12.0,"resets_at":"2026-10-19T13:00:00+00:00"},"seven_day":{"utilization":84.0,"resets_at":null}}),
            now,
        );
        let record: Value = serde_json::from_str(fs::read_to_string(path).unwrap().trim()).unwrap();
        assert_eq!(record["v"], 1);
        assert_eq!(record["t"], "2026-10-19T11:41:05Z");
        assert_eq!(record["provider"], "claude");
        assert_eq!(record["windows"]["five_hour"]["pct"], 12.0);
        assert!(record["windows"].get("seven_day_opus").is_none());
    }

    #[test]
    fn non_200_success_is_not_recorded() {
        let directory = temporary_directory("quota-history-non-200");
        let path = directory.join(HISTORY_FILE);
        let data = json!({"five_hour":{"utilization":12.0}});
        let now = Utc::now();
        record_success_at_status(&path, &data, reqwest::StatusCode::PARTIAL_CONTENT, now);
        record_success_at_status(&path, &data, reqwest::StatusCode::NO_CONTENT, now);
        assert!(!path.exists());
        record_success_at_status(&path, &data, reqwest::StatusCode::OK, now);
        assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 1);
    }

    #[test]
    fn identical_values_within_30_minutes_are_not_rewritten() {
        let directory = temporary_directory("quota-history-identical");
        let path = directory.join(HISTORY_FILE);
        let now = Utc::now();
        let data = json!({"five_hour":{"utilization":12.0,"resets_at":null}});
        record_success_at(&path, &data, now);
        record_success_at(&path, &data, now + Duration::minutes(29));
        assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 1);
    }

    #[test]
    fn heartbeat_written_after_30_minutes() {
        let directory = temporary_directory("quota-history-heartbeat");
        let path = directory.join(HISTORY_FILE);
        let now = Utc::now();
        let data = json!({"five_hour":{"utilization":12.0,"resets_at":null}});
        record_success_at(&path, &data, now);
        record_success_at(&path, &data, now + Duration::minutes(30));
        assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 2);
    }

    #[test]
    fn record_success_does_not_prune_inline() {
        let directory = temporary_directory("quota-history-no-inline-prune");
        let path = directory.join(HISTORY_FILE);
        fs::write(
            &path,
            "{\"v\":1,\"t\":\"2025-01-01T00:00:00Z\",\"provider\":\"claude\",\"windows\":{}}\n",
        )
        .unwrap();
        initialize_at(&path);
        record_success_at(&path, &json!({"five_hour":{"utilization":1.0}}), Utc::now());
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("2025-01-01T00:00:00Z"));
        assert_eq!(contents.lines().count(), 2);
    }

    #[test]
    fn append_during_prune_is_not_lost() {
        let directory = temporary_directory("quota-history-append-during-prune");
        let path = directory.join(HISTORY_FILE);
        let now = Utc::now();
        fs::write(&path, "{bad}\n").unwrap();
        record_success_at(&path, &json!({"five_hour":{"utilization":1.0}}), now);
        let (at_rename, wait_for_release) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let prune_path = path.clone();
        let prune = thread::spawn(move || {
            prune_if_due_at_with_before_rename(&prune_path, now + Duration::hours(25), || {
                at_rename.send(()).unwrap();
                released.recv().unwrap();
            });
        });
        wait_for_release.recv().unwrap();
        let (append_started_tx, append_started) = mpsc::channel();
        let (append_finished, append_finished_rx) = mpsc::channel();
        let append_path = path.clone();
        let append = thread::spawn(move || {
            append_started_tx.send(()).unwrap();
            record_success_at(
                &append_path,
                &json!({"five_hour":{"utilization":2.0}}),
                now + Duration::seconds(1),
            );
            append_finished.send(()).unwrap();
        });
        append_started.recv().unwrap();
        assert!(append_finished_rx.try_recv().is_err());
        release.send(()).unwrap();
        prune.join().unwrap();
        append.join().unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert_eq!(contents.lines().count(), 2);
        assert!(contents.contains("\"pct\":2.0"));
    }

    #[test]
    fn subminute_resets_at_jitter_is_not_a_change() {
        let directory = temporary_directory("quota-history-subminute-jitter");
        let path = directory.join(HISTORY_FILE);
        let now = Utc::now();
        record_success_at(
            &path,
            &json!({"five_hour":{"utilization":12.0,"resets_at":"2026-10-20T01:02:03.001Z"}}),
            now,
        );
        record_success_at(
            &path,
            &json!({"five_hour":{"utilization":12.0,"resets_at":"2026-10-20T01:02:59.999Z"}}),
            now + Duration::minutes(1),
        );
        assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 1);
    }

    #[test]
    fn resets_at_minute_change_is_a_change() {
        let directory = temporary_directory("quota-history-minute-change");
        let path = directory.join(HISTORY_FILE);
        let now = Utc::now();
        record_success_at(
            &path,
            &json!({"five_hour":{"utilization":12.0,"resets_at":"2026-10-20T01:02:59Z"}}),
            now,
        );
        record_success_at(
            &path,
            &json!({"five_hour":{"utilization":12.0,"resets_at":"2026-10-20T01:03:00Z"}}),
            now + Duration::minutes(1),
        );
        assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 2);
    }

    #[test]
    fn prune_drops_lines_older_than_120_days_and_unparseable_lines() {
        let directory = temporary_directory("quota-history-prune");
        let path = directory.join(HISTORY_FILE);
        fs::write(&path, "{bad}\n{\"v\":1,\"t\":\"2025-01-01T00:00:00Z\",\"provider\":\"claude\",\"windows\":{}}\n{\"v\":1,\"t\":\"2026-10-19T00:00:00Z\",\"provider\":\"claude\",\"windows\":{}}\n").unwrap();
        prune_at(
            &path,
            DateTime::parse_from_rfc3339("2026-10-20T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn history_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let directory = temporary_directory("quota-history-permissions");
        let path = directory.join(HISTORY_FILE);
        record_success_at(&path, &json!({"five_hour":{"utilization":1.0}}), Utc::now());
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_world_readable_history_is_tightened_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let directory = temporary_directory("quota-history-existing-permissions");
        let path = directory.join(HISTORY_FILE);
        fs::write(&path, "existing record\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        record_success_at(&path, &json!({"five_hour":{"utilization":1.0}}), Utc::now());
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

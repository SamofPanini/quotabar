//! Disk persistence for cost summaries so cold starts can paint instantly.

use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Snapshots older than this are ignored entirely.
pub const STALE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub const COST_CACHE_SCHEMA_VERSION: u32 = 2;

/// Identity of the compiled ccstats SDK, whose prices and parsers shape the payload.
pub const CCSTATS_VERSION: &str = ccstats::VERSION;

#[derive(serde::Serialize, serde::Deserialize)]
struct Snapshot<T> {
    schema_version: u32,
    app_version: String,
    ccstats_version: String,
    saved_at_unix_ms: u64,
    payload: T,
}

/// How a disk snapshot may be used by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotUse {
    /// Within the cache TTL: as good as a memory-cache hit.
    Fresh,
    /// Expired but recent enough to paint a first frame once.
    ServeStaleOnce,
    /// Too old (or callers already served it once): ignore.
    Ignore,
}

pub fn classify_snapshot(age: Duration, ttl: Duration, already_served: bool) -> SnapshotUse {
    if age <= ttl {
        SnapshotUse::Fresh
    } else if age <= STALE_MAX_AGE && !already_served {
        SnapshotUse::ServeStaleOnce
    } else {
        SnapshotUse::Ignore
    }
}

fn default_cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|dir| dir.join("quotabar"))
}

fn snapshot_path(base_dir: &Path, cache_key: &str) -> PathBuf {
    let file_name: String = cache_key
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    base_dir.join(format!("cost-{file_name}.json"))
}

pub fn read_snapshot<T: DeserializeOwned>(cache_key: &str) -> Option<(Duration, T)> {
    read_snapshot_in(&default_cache_dir()?, cache_key)
}

pub fn write_snapshot<T: Serialize>(cache_key: &str, payload: &T) {
    let Some(base_dir) = default_cache_dir() else {
        eprintln!("[CostCache] cache dir unavailable; skipping disk write");
        return;
    };
    write_snapshot_in(&base_dir, cache_key, payload);
}

fn snapshot_identity_matches<T>(snapshot: &Snapshot<T>) -> bool {
    snapshot.schema_version == COST_CACHE_SCHEMA_VERSION
        && snapshot.app_version == env!("CARGO_PKG_VERSION")
        && snapshot.ccstats_version == CCSTATS_VERSION
}

fn read_snapshot_in<T: DeserializeOwned>(
    base_dir: &Path,
    cache_key: &str,
) -> Option<(Duration, T)> {
    let path = snapshot_path(base_dir, cache_key);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            eprintln!("[CostCache] failed to read {}: {err}", path.display());
            return None;
        }
    };
    let snapshot: Snapshot<T> = match serde_json::from_slice(&bytes) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            eprintln!("[CostCache] discarding corrupt {}: {err}", path.display());
            let _removed = fs::remove_file(&path);
            return None;
        }
    };
    if !snapshot_identity_matches(&snapshot) {
        eprintln!(
            "[CostCache] discarding snapshot with incompatible identity: {}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("<unknown>")
        );
        let _removed = fs::remove_file(&path);
        return None;
    }

    let saved_at = UNIX_EPOCH + Duration::from_millis(snapshot.saved_at_unix_ms);
    let age = match SystemTime::now().duration_since(saved_at) {
        Ok(age) => age,
        Err(_) => {
            eprintln!(
                "[CostCache] discarding snapshot with future timestamp: {}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("<unknown>")
            );
            let _removed = fs::remove_file(&path);
            return None;
        }
    };
    Some((age, snapshot.payload))
}

fn write_snapshot_in<T: Serialize>(base_dir: &Path, cache_key: &str, payload: &T) {
    let saved_at_unix_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis() as u64,
        Err(err) => {
            eprintln!("[CostCache] system clock before epoch; skipping disk write: {err}");
            return;
        }
    };
    let snapshot = Snapshot {
        schema_version: COST_CACHE_SCHEMA_VERSION,
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        ccstats_version: CCSTATS_VERSION.to_owned(),
        saved_at_unix_ms,
        payload,
    };
    let bytes = match serde_json::to_vec(&snapshot) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("[CostCache] failed to serialize snapshot {cache_key}: {err}");
            return;
        }
    };

    if let Err(err) = fs::create_dir_all(base_dir) {
        eprintln!("[CostCache] failed to create {}: {err}", base_dir.display());
        return;
    }
    let path = snapshot_path(base_dir, cache_key);
    let tmp_path = path.with_extension("json.tmp");
    if let Err(err) = fs::write(&tmp_path, &bytes) {
        eprintln!("[CostCache] failed to write {}: {err}", tmp_path.display());
        return;
    }
    if let Err(err) = fs::rename(&tmp_path, &path) {
        eprintln!("[CostCache] failed to move {}: {err}", path.display());
        let _removed = fs::remove_file(&tmp_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_base() -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "quotabar-cost-cache-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn now_unix_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_millis() as u64
    }

    fn write_raw_snapshot(base: &Path, cache_key: &str, value: serde_json::Value) -> PathBuf {
        std::fs::create_dir_all(base).expect("temp dir should create");
        let path = snapshot_path(base, cache_key);
        std::fs::write(
            &path,
            serde_json::to_vec(&value).expect("snapshot JSON should serialize"),
        )
        .expect("snapshot JSON should write");
        path
    }

    #[test]
    fn snapshot_roundtrip_preserves_payload_and_identity() {
        let base = temp_base();
        write_snapshot_in(&base, "overview|claude|USD|local", &vec![1_i64, 2, 3]);
        let (age, payload): (Duration, Vec<i64>) =
            read_snapshot_in(&base, "overview|claude|USD|local")
                .expect("snapshot should read back");
        assert_eq!(payload, vec![1, 2, 3]);
        assert!(age < Duration::from_secs(60), "age should be near zero");
        let path = snapshot_path(&base, "overview|claude|USD|local");
        let snapshot: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("snapshot should be readable"))
                .expect("snapshot should be valid JSON");
        assert_eq!(snapshot["schema_version"], COST_CACHE_SCHEMA_VERSION);
        assert_eq!(snapshot["app_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(snapshot["ccstats_version"], CCSTATS_VERSION);
        let _cleanup = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_snapshot_without_identity_is_discarded() {
        let base = temp_base();
        let path = write_raw_snapshot(
            &base,
            "legacy",
            serde_json::json!({
                "saved_at_unix_ms": now_unix_ms(),
                "payload": [1, 2, 3],
            }),
        );
        let result: Option<(Duration, Vec<i64>)> = read_snapshot_in(&base, "legacy");
        assert!(result.is_none());
        assert!(!path.exists(), "legacy file should be deleted");
        let _cleanup = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn mismatched_schema_is_discarded() {
        let base = temp_base();
        let path = write_raw_snapshot(
            &base,
            "schema-mismatch",
            serde_json::json!({
                "schema_version": COST_CACHE_SCHEMA_VERSION + 1,
                "app_version": env!("CARGO_PKG_VERSION"),
                "ccstats_version": CCSTATS_VERSION,
                "saved_at_unix_ms": now_unix_ms(),
                "payload": [1, 2, 3],
            }),
        );
        let result: Option<(Duration, Vec<i64>)> = read_snapshot_in(&base, "schema-mismatch");
        assert!(result.is_none());
        assert!(!path.exists(), "mismatched file should be deleted");
        let _cleanup = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn mismatched_app_version_is_discarded() {
        let base = temp_base();
        let path = write_raw_snapshot(
            &base,
            "app-mismatch",
            serde_json::json!({
                "schema_version": COST_CACHE_SCHEMA_VERSION,
                "app_version": "0.0.0-other",
                "ccstats_version": CCSTATS_VERSION,
                "saved_at_unix_ms": now_unix_ms(),
                "payload": [1, 2, 3],
            }),
        );
        let result: Option<(Duration, Vec<i64>)> = read_snapshot_in(&base, "app-mismatch");
        assert!(result.is_none());
        assert!(!path.exists(), "mismatched file should be deleted");
        let _cleanup = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn mismatched_ccstats_version_is_discarded() {
        let base = temp_base();
        let path = write_raw_snapshot(
            &base,
            "ccstats-mismatch",
            serde_json::json!({
                "schema_version": COST_CACHE_SCHEMA_VERSION,
                "app_version": env!("CARGO_PKG_VERSION"),
                "ccstats_version": "0.0.0-other",
                "saved_at_unix_ms": now_unix_ms(),
                "payload": [1, 2, 3],
            }),
        );
        let result: Option<(Duration, Vec<i64>)> = read_snapshot_in(&base, "ccstats-mismatch");
        assert!(result.is_none());
        assert!(!path.exists(), "mismatched file should be deleted");
        let _cleanup = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn future_saved_at_is_invalid() {
        let base = temp_base();
        let path = write_raw_snapshot(
            &base,
            "future",
            serde_json::json!({
                "schema_version": COST_CACHE_SCHEMA_VERSION,
                "app_version": env!("CARGO_PKG_VERSION"),
                "ccstats_version": CCSTATS_VERSION,
                "saved_at_unix_ms": now_unix_ms() + 86_400_000,
                "payload": [1, 2, 3],
            }),
        );
        let result: Option<(Duration, Vec<i64>)> = read_snapshot_in(&base, "future");
        assert!(result.is_none());
        assert!(!path.exists(), "future-dated file should be deleted");
        let _cleanup = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_snapshot_returns_none() {
        let base = temp_base();
        let result: Option<(Duration, Vec<i64>)> = read_snapshot_in(&base, "missing");
        assert!(result.is_none());
    }

    #[test]
    fn corrupt_snapshot_is_discarded_and_removed() {
        let base = temp_base();
        std::fs::create_dir_all(&base).expect("temp dir should create");
        let path = snapshot_path(&base, "bad");
        std::fs::write(&path, b"not json").expect("corrupt file should write");
        let result: Option<(Duration, Vec<i64>)> = read_snapshot_in(&base, "bad");
        assert!(result.is_none());
        assert!(!path.exists(), "corrupt file should be deleted");
        let _cleanup = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cache_keys_map_to_distinct_sanitized_files() {
        let base = PathBuf::from("/base");
        let overview = snapshot_path(&base, "overview|claude|USD|local");
        let daily = snapshot_path(&base, "daily|claude|30|USD|local");
        assert_ne!(overview, daily);
        assert_eq!(
            overview.file_name().and_then(|name| name.to_str()),
            Some("cost-overview-claude-USD-local.json")
        );
    }

    #[test]
    fn classify_snapshot_covers_fresh_stale_and_ignore() {
        let ttl = Duration::from_secs(1200);
        assert_eq!(
            classify_snapshot(Duration::from_secs(60), ttl, false),
            SnapshotUse::Fresh
        );
        assert_eq!(
            classify_snapshot(Duration::from_secs(3600), ttl, false),
            SnapshotUse::ServeStaleOnce
        );
        assert_eq!(
            classify_snapshot(Duration::from_secs(3600), ttl, true),
            SnapshotUse::Ignore
        );
        assert_eq!(
            classify_snapshot(STALE_MAX_AGE + Duration::from_secs(1), ttl, false),
            SnapshotUse::Ignore
        );
    }
}

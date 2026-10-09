//! The release-pinned LiteLLM pricing snapshot installed before ccstats prices usage.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Once,
    },
    time::SystemTime,
};

const SNAPSHOT: &[u8] = include_bytes!("../../resources/pricing/litellm_model_prices.json");
const SOURCE: &str = include_str!("../../resources/pricing/pricing-source.json");

static INSTALL_ONCE: Once = Once::new();
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExistingSnapshot {
    Missing,
    NotJsonObject,
    JsonObject { modified_at: SystemTime },
    Unreadable,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PricingSnapshotDecision<'a> {
    pub target: &'a Path,
    pub existing: ExistingSnapshot,
    pub committed_at: SystemTime,
}

/// Purely decides whether this process should replace the ccstats pricing cache.
pub(crate) fn should_write_snapshot(decision: PricingSnapshotDecision<'_>) -> bool {
    let _target = decision.target;
    match decision.existing {
        ExistingSnapshot::Missing
        | ExistingSnapshot::NotJsonObject
        | ExistingSnapshot::Unreadable => true,
        ExistingSnapshot::JsonObject { modified_at } => modified_at < decision.committed_at,
    }
}

fn snapshot_committed_at() -> Result<SystemTime, String> {
    let source: Value = serde_json::from_str(SOURCE)
        .map_err(|error| format!("could not parse bundled pricing metadata: {error}"))?;
    let committed_at = source
        .get("committed_at")
        .and_then(Value::as_str)
        .ok_or_else(|| "bundled pricing metadata is missing committed_at".to_string())?;
    DateTime::parse_from_rfc3339(committed_at)
        .map(|time| time.with_timezone(&Utc).into())
        .map_err(|error| format!("bundled pricing metadata has invalid committed_at: {error}"))
}

pub(crate) fn select_cache_path(
    xdg_cache_home: Option<&Path>,
    platform_cache_dir: Option<&Path>,
    home_dir: Option<&Path>,
) -> Option<PathBuf> {
    let cache_root = xdg_cache_home
        .filter(|path| path.is_absolute())
        .or(platform_cache_dir);
    cache_root
        .map(|root| root.join("ccstats").join("pricing.json"))
        .or_else(|| home_dir.map(|home| home.join(".cache").join("ccstats").join("pricing.json")))
}

fn cache_path() -> Option<PathBuf> {
    let xdg_cache_home = env::var_os("XDG_CACHE_HOME").map(PathBuf::from);
    let platform_cache_dir = dirs::cache_dir();
    let home_dir = dirs::home_dir();
    select_cache_path(
        xdg_cache_home.as_deref(),
        platform_cache_dir.as_deref(),
        home_dir.as_deref(),
    )
}

fn inspect_existing(path: &Path) -> ExistingSnapshot {
    match fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ExistingSnapshot::Missing,
        Err(_) => ExistingSnapshot::Unreadable,
        Ok(metadata) => match fs::read(path) {
            Ok(bytes)
                if serde_json::from_slice::<Value>(&bytes).is_ok_and(|value| value.is_object()) =>
            {
                match metadata.modified() {
                    Ok(modified_at) => ExistingSnapshot::JsonObject { modified_at },
                    Err(_) => ExistingSnapshot::Unreadable,
                }
            }
            Ok(_) | Err(_) => ExistingSnapshot::NotJsonObject,
        },
    }
}

fn atomic_write(path: &Path, bytes: &[u8], modified_at: Option<SystemTime>) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or_else(|| "pricing cache path has no parent directory".to_string())?;
    fs::create_dir_all(directory)
        .map_err(|error| format!("could not create pricing cache directory: {error}"))?;
    let temporary = directory.join(format!(
        ".pricing.json.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("could not create temporary pricing cache: {error}"))?;
    let result = (|| {
        file.write_all(bytes)
            .map_err(|error| format!("could not write temporary pricing cache: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("could not sync temporary pricing cache: {error}"))?;
        // Stamp the snapshot with its data time, not the install time, so a later
        // release whose snapshot is newer than this one still replaces it. The data
        // is flushed first: on macOS a later flush can overwrite an earlier stamp.
        if let Some(modified_at) = modified_at {
            file.set_modified(modified_at)
                .map_err(|error| format!("could not stamp temporary pricing cache: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("could not sync temporary pricing cache: {error}"))?;
        }
        fs::rename(&temporary, path)
            .map_err(|error| format!("could not replace pricing cache: {error}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn install_snapshot_at(path: &Path) -> Result<bool, String> {
    let committed_at = snapshot_committed_at()?;
    if !should_write_snapshot(PricingSnapshotDecision {
        target: path,
        existing: inspect_existing(path),
        committed_at,
    }) {
        return Ok(false);
    }
    atomic_write(path, SNAPSHOT, Some(committed_at))?;
    Ok(true)
}

fn run_once(once: &Once, action: impl FnOnce()) {
    once.call_once(action);
}

/// Best-effort only: ccstats can still fall back to its built-in model prices.
pub(crate) fn ensure_installed() {
    ensure_installed_with(&INSTALL_ONCE, || {
        let Some(path) = cache_path() else {
            eprintln!("[Pricing] no cache directory is available for the bundled snapshot");
            return;
        };
        if let Err(error) = install_snapshot_at(&path) {
            eprintln!(
                "[Pricing] could not install bundled pricing snapshot at {}: {error}",
                path.display()
            );
        }
    });
}

fn ensure_installed_with(once: &Once, install: impl FnOnce()) {
    run_once(once, install);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("quotabar-pricing-{name}-{}", std::process::id()))
    }

    #[test]
    fn decision_covers_missing_invalid_stale_and_current_files() {
        let target = Path::new("pricing.json");
        let committed = SystemTime::UNIX_EPOCH + Duration::from_secs(20);
        assert!(should_write_snapshot(PricingSnapshotDecision {
            target,
            existing: ExistingSnapshot::Missing,
            committed_at: committed
        }));
        assert!(should_write_snapshot(PricingSnapshotDecision {
            target,
            existing: ExistingSnapshot::NotJsonObject,
            committed_at: committed
        }));
        assert!(should_write_snapshot(PricingSnapshotDecision {
            target,
            existing: ExistingSnapshot::JsonObject {
                modified_at: SystemTime::UNIX_EPOCH + Duration::from_secs(19)
            },
            committed_at: committed
        }));
        assert!(!should_write_snapshot(PricingSnapshotDecision {
            target,
            existing: ExistingSnapshot::JsonObject {
                modified_at: committed
            },
            committed_at: committed
        }));
        assert!(!should_write_snapshot(PricingSnapshotDecision {
            target,
            existing: ExistingSnapshot::JsonObject {
                modified_at: committed + Duration::from_secs(1)
            },
            committed_at: committed
        }));
    }

    #[test]
    fn cache_path_matches_ccstats_xdg_and_home_selection_rules() {
        let xdg = Path::new("/tmp/ccstats-xdg");
        let platform = Path::new("/tmp/platform-cache");
        let home = Path::new("/tmp/home");
        assert_eq!(
            select_cache_path(Some(xdg), Some(platform), Some(home)),
            Some(xdg.join("ccstats/pricing.json"))
        );
        assert_eq!(
            select_cache_path(
                Some(Path::new("relative-cache")),
                Some(platform),
                Some(home)
            ),
            Some(platform.join("ccstats/pricing.json"))
        );
        assert_eq!(
            select_cache_path(Some(Path::new("")), Some(platform), Some(home)),
            Some(platform.join("ccstats/pricing.json"))
        );
        assert_eq!(
            select_cache_path(None, Some(platform), Some(home)),
            Some(platform.join("ccstats/pricing.json"))
        );
        assert_eq!(
            select_cache_path(None, None, Some(home)),
            Some(home.join(".cache/ccstats/pricing.json"))
        );
    }

    #[test]
    fn atomic_write_preserves_unrelated_files() {
        let directory = temp_path("atomic");
        let target = directory.join("pricing.json");
        let unrelated = directory.join("other-data");
        fs::create_dir_all(&directory).expect("create temp directory");
        fs::write(&unrelated, b"keep me").expect("write unrelated file");
        atomic_write(&target, b"{}", None).expect("write pricing snapshot");
        assert_eq!(fs::read(&target).expect("read pricing snapshot"), b"{}");
        assert_eq!(
            fs::read(&unrelated).expect("read unrelated file"),
            b"keep me"
        );
        fs::remove_dir_all(directory).expect("clean temporary directory");
    }

    #[cfg(unix)]
    #[test]
    fn write_to_read_only_directory_returns_error() {
        use std::os::unix::fs::PermissionsExt;
        let directory = temp_path("readonly");
        fs::create_dir_all(&directory).expect("create temp directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o555))
            .expect("make directory read-only");
        let result = atomic_write(&directory.join("pricing.json"), b"{}", None);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))
            .expect("restore permissions");
        assert!(result.is_err());
        fs::remove_dir_all(directory).expect("clean temporary directory");
    }

    #[test]
    fn installed_snapshot_carries_its_data_time_so_a_newer_snapshot_replaces_it() {
        let directory = temp_path("stamp");
        let target = directory.join("ccstats/pricing.json");
        assert!(install_snapshot_at(&target).expect("install into empty cache"));
        let committed_at = snapshot_committed_at().expect("bundled committed_at");
        let modified_at = fs::metadata(&target)
            .and_then(|metadata| metadata.modified())
            .expect("read installed mtime");
        assert_eq!(modified_at, committed_at);
        // A snapshot published one minute later must still win over this install.
        assert!(should_write_snapshot(PricingSnapshotDecision {
            target: &target,
            existing: inspect_existing(&target),
            committed_at: committed_at + Duration::from_secs(60),
        }));
        // Reinstalling the same snapshot is a no-op.
        assert!(!install_snapshot_at(&target).expect("second install"));
        fs::remove_dir_all(directory).expect("clean temporary directory");
    }

    #[test]
    fn production_install_entry_runs_the_install_action_only_once() {
        let once = Once::new();
        let calls = std::sync::atomic::AtomicU64::new(0);
        ensure_installed_with(&once, || {
            calls.fetch_add(1, Ordering::Relaxed);
        });
        ensure_installed_with(&once, || {
            calls.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn bundled_snapshot_metadata_matches_bytes_and_required_models() {
        let source: Value = serde_json::from_str(SOURCE).expect("parse pricing metadata");
        assert_eq!(
            source["snapshot_bytes"].as_u64(),
            Some(SNAPSHOT.len() as u64)
        );
        let pricing: Value =
            serde_json::from_slice(SNAPSHOT).expect("parse bundled pricing snapshot");
        let pricing = pricing.as_object().expect("snapshot top level object");
        assert_eq!(source["entries"].as_u64(), Some(pricing.len() as u64));
        for model in ["claude-haiku-5-5", "gpt-5.6-luna", "gpt-5.6-sol"] {
            assert!(pricing.contains_key(model), "missing {model}");
        }
    }
}

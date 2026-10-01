//! Task-owned macOS smoke for PR #19 R2/R3.
//!
//! It builds the product Tauri context (and therefore uses its real bundle
//! identifier path resolver) but does not start the desktop event loop or any
//! provider request. The caller must provide an isolated HOME strictly below
//! the named task root.

#[path = "../commands.rs"]
mod commands;
#[path = "../domain/mod.rs"]
mod domain;
#[path = "../services/mod.rs"]
mod services;

#[cfg(target_os = "macos")]
use std::{
    env, fs,
    path::{Component, Path, PathBuf},
};

#[cfg(target_os = "macos")]
fn has_parent_dir(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

#[cfg(target_os = "macos")]
fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("{label} must exist before the smoke starts: {error}"))?;
    if !metadata.is_dir() {
        return Err(format!("{label} must be a directory"));
    }
    path.canonicalize()
        .map_err(|error| format!("{label} must canonicalize before the smoke starts: {error}"))
}

#[cfg(target_os = "macos")]
fn strictly_within(root: &Path, candidate: &Path) -> bool {
    candidate != root && candidate.starts_with(root)
}

#[cfg(target_os = "macos")]
fn validate_task_paths(smoke_root: &Path, home: &Path) -> Result<(PathBuf, PathBuf), String> {
    if !smoke_root.is_absolute() || has_parent_dir(smoke_root) {
        return Err("the task-owned smoke root must be an absolute path without `..`".into());
    }
    if !home.is_absolute() || has_parent_dir(home) {
        return Err("HOME must be an absolute path without `..`".into());
    }

    let smoke_root = canonical_directory(smoke_root, "the task-owned smoke root")?;
    let home = canonical_directory(home, "HOME")?;
    if !strictly_within(&smoke_root, &home) {
        return Err("HOME must canonicalize strictly inside the task-owned smoke root".into());
    }
    Ok((smoke_root, home))
}

#[cfg(target_os = "macos")]
fn assert_target_contained(smoke_root: &Path, target: &Path, label: &str) -> Result<(), String> {
    if !strictly_within(smoke_root, target) || has_parent_dir(target) {
        return Err(format!(
            "{label} is not lexically within the task-owned smoke root"
        ));
    }

    let existing_ancestor = target
        .ancestors()
        .find(|ancestor| ancestor.exists())
        .ok_or_else(|| format!("{label} has no existing ancestor"))?;
    let canonical_ancestor = existing_ancestor.canonicalize().map_err(|error| {
        format!("{label} existing ancestor must canonicalize before filesystem use: {error}")
    })?;
    if canonical_ancestor != smoke_root && !canonical_ancestor.starts_with(smoke_root) {
        return Err(format!(
            "{label} existing ancestor escapes the task-owned smoke root"
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn assert_existing_target_contained(
    smoke_root: &Path,
    target: &Path,
    label: &str,
) -> Result<(), String> {
    assert_target_contained(smoke_root, target, label)?;
    let canonical_target = target
        .canonicalize()
        .map_err(|error| format!("{label} must canonicalize after filesystem use: {error}"))?;
    if !strictly_within(smoke_root, &canonical_target) {
        return Err(format!(
            "{label} canonical path escapes the task-owned smoke root"
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn create_task_directory(smoke_root: &Path, target: &Path, label: &str) -> Result<(), String> {
    assert_target_contained(smoke_root, target, label)?;
    fs::create_dir_all(target).map_err(|error| format!("cannot create {label}: {error}"))?;
    assert_existing_target_contained(smoke_root, target, label)
}

#[cfg(target_os = "macos")]
fn remove_task_directory(smoke_root: &Path, target: &Path, label: &str) -> Result<(), String> {
    assert_existing_target_contained(smoke_root, target, label)?;
    fs::remove_dir_all(target).map_err(|error| format!("cannot remove {label}: {error}"))
}

#[cfg(target_os = "macos")]
fn run() -> Result<(), String> {
    use chrono::{DateTime, Utc};
    use services::{claude_snapshot::ClaudeSnapshotStore, state_location::StateProvenance};
    use std::os::unix::fs::MetadataExt;
    use tauri::Manager;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("fixed smoke timestamp is valid")
    }

    let raw_smoke_root = PathBuf::from(
        env::var_os("QBI_P1_R2_SMOKE_ROOT")
            .ok_or("the smoke command must provide its task-owned root")?,
    );
    let raw_home = PathBuf::from(env::var_os("HOME").ok_or("the smoke command must provide HOME")?);
    let (smoke_root, home) = validate_task_paths(&raw_smoke_root, &raw_home)?;

    // Tauri and dirs resolve the paths below from HOME. Keep their resolution
    // aligned with the path we canonicalized above instead of using raw input.
    env::set_var("HOME", &home);

    let app = tauri::Builder::default()
        .build(tauri::generate_context!())
        .map_err(|error| format!("the product Tauri context must build: {error}"))?;
    let legacy_config = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("the product Tauri app-config path must resolve: {error}"))?;
    let primary_config = services::state_location::primary_state_dir()
        .map_err(|_| "the durable primary path must resolve".to_owned())?;
    assert_target_contained(
        &smoke_root,
        &legacy_config,
        "the derived legacy config path",
    )?;
    assert_target_contained(
        &smoke_root,
        &primary_config,
        "the derived primary config path",
    )?;

    let legacy_state = legacy_config.join("claude-current-state");
    let primary_state = primary_config.join("claude-current-state");
    assert_target_contained(&smoke_root, &legacy_state, "the derived legacy state path")?;
    assert_target_contained(
        &smoke_root,
        &primary_state,
        "the derived primary state path",
    )?;
    let first = commands::get_claude_current_snapshots(app.handle().clone())
        .map_err(|error| format!("the first fresh projection must resolve: {error:?}"))?;
    let second = commands::get_claude_current_snapshots(app.handle().clone())
        .map_err(|error| format!("the second fresh projection must resolve: {error:?}"))?;
    if !(first.slots.is_empty() && second.slots.is_empty())
        || first.provenance != StateProvenance::None
        || second.provenance != StateProvenance::None
    {
        return Err("fresh projections must both return the empty none baseline".into());
    }
    if primary_config.exists() || legacy_config.exists() {
        return Err("a fresh read must not create primary or legacy state".into());
    }

    create_task_directory(
        &smoke_root,
        legacy_state
            .parent()
            .ok_or("legacy state must have a parent")?,
        "the legacy fixture directory",
    )?;
    let legacy = ClaudeSnapshotStore::at_root(legacy_state.clone())
        .map_err(|error| format!("the legacy fixture store must initialize: {error:?}"))?;
    assert_existing_target_contained(&smoke_root, &legacy_state, "the legacy fixture state")?;
    legacy
        .register_slot(
            services::claude_snapshot::AccountSlotId::parse("123e4567-e89b-42d3-a456-426614174050")
                .map_err(|error| format!("the fixed legacy account id must parse: {error:?}"))?,
            "smoke".into(),
            None,
            now(),
        )
        .map_err(|error| format!("the legacy fixture slot must register: {error:?}"))?;
    let legacy_state_file = legacy_state.join("current-state.json");
    assert_existing_target_contained(
        &smoke_root,
        &legacy_state_file,
        "the legacy fixture state file",
    )?;
    let legacy_bytes = fs::read(&legacy_state_file)
        .map_err(|error| format!("the legacy fixture state must read: {error}"))?;
    let legacy_metadata = fs::metadata(&legacy_state)
        .map_err(|error| format!("the legacy fixture metadata must read: {error}"))?;
    let legacy_entries = fs::read_dir(&legacy_state)
        .map_err(|error| format!("the legacy fixture entries must read: {error}"))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("the legacy fixture entries must enumerate: {error}"))?;
    let legacy_projection = commands::get_claude_current_snapshots(app.handle().clone())
        .map_err(|error| format!("the legacy projection must resolve: {error:?}"))?;
    if legacy_projection.provenance != StateProvenance::Legacy
        || fs::read(&legacy_state_file)
            .map_err(|error| format!("the legacy fixture state must re-read: {error}"))?
            != legacy_bytes
    {
        return Err("legacy projection must be read-only".into());
    }
    let legacy_after = fs::metadata(&legacy_state)
        .map_err(|error| format!("the legacy fixture metadata must re-read: {error}"))?;
    if (
        legacy_metadata.mode(),
        legacy_metadata.ctime(),
        legacy_metadata.ctime_nsec(),
    ) != (
        legacy_after.mode(),
        legacy_after.ctime(),
        legacy_after.ctime_nsec(),
    ) {
        return Err("legacy projection must preserve directory metadata".into());
    }
    let legacy_entries_after = fs::read_dir(&legacy_state)
        .map_err(|error| format!("the legacy fixture entries must re-enumerate: {error}"))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("the legacy fixture entries must re-enumerate: {error}"))?;
    if legacy_entries_after != legacy_entries {
        return Err("legacy projection must preserve directory entries".into());
    }

    create_task_directory(
        &smoke_root,
        &primary_config,
        "the primary fixture directory",
    )?;
    let primary = ClaudeSnapshotStore::at_root(primary_state.clone())
        .map_err(|error| format!("the primary fixture store must initialize: {error:?}"))?;
    assert_existing_target_contained(&smoke_root, &primary_state, "the primary fixture state")?;
    primary
        .register_slot(
            services::claude_snapshot::AccountSlotId::parse("123e4567-e89b-42d3-a456-426614174051")
                .map_err(|error| format!("the fixed primary account id must parse: {error:?}"))?,
            "primary".into(),
            None,
            now(),
        )
        .map_err(|error| format!("the primary fixture slot must register: {error:?}"))?;
    let registry_file = primary_config.join(services::codex_profiles::CONFIG_FILE);
    assert_target_contained(&smoke_root, &registry_file, "the primary registry fixture")?;
    fs::write(&registry_file, r#"{"version":1,"profiles":[]}"#)
        .map_err(|error| format!("the primary registry fixture must write: {error}"))?;
    assert_existing_target_contained(&smoke_root, &registry_file, "the primary registry fixture")?;
    let registry =
        tauri::async_runtime::block_on(commands::get_codex_profiles(app.handle().clone()))
            .map_err(|error| format!("the product registry command must resolve: {error:?}"))?;
    if registry.registry_provenance != StateProvenance::Primary || registry.registry_error.is_some()
    {
        return Err("the primary registry fixture must report primary without an error".into());
    }

    remove_task_directory(&smoke_root, &legacy_config, "the controlled legacy fixture")?;
    if !primary_state.exists() {
        return Err("controlled legacy removal must retain primary state".into());
    }
    assert_existing_target_contained(&smoke_root, &primary_state, "the retained primary state")?;
    let primary_projection = commands::get_claude_current_snapshots(app.handle().clone())
        .map_err(|error| format!("the primary projection must resolve: {error:?}"))?;
    if primary_projection.provenance != StateProvenance::Primary {
        return Err("the retained primary state must remain selected".into());
    }
    println!(
        "QBI_P1_R3_SMOKE primary={} legacy={} provenance=primary registry=primary",
        primary_config.display(),
        legacy_config.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = run() {
        eprintln!("QBI_P1_R3_SMOKE_REJECTED: {error}");
        std::process::exit(2);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    panic!("QBI-P1 R2 local smoke is macOS-only");
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::symlink,
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

    fn fixture_base() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = env::temp_dir().join(format!(
            "qbi-p1-r3-containment-{}-{}-{}",
            std::process::id(),
            nonce,
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn task_paths_reject_parent_dir_and_home_symlink_escape() {
        let base = fixture_base();
        let root = base.join("root");
        let victim = base.join("victim");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&victim).unwrap();
        assert!(validate_task_paths(&root, &root.join("home/../../victim")).is_err());

        let escaped_home = root.join("home");
        symlink(&victim, &escaped_home).unwrap();
        assert!(validate_task_paths(&root, &escaped_home).is_err());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn target_containment_rejects_primary_and_legacy_symlink_ancestors() {
        let base = fixture_base();
        let root = base.join("root");
        let home = root.join("home");
        let victim = base.join("victim");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&victim).unwrap();
        let (root, home) = validate_task_paths(&root, &home).unwrap();

        symlink(&victim, home.join(".config")).unwrap();
        assert!(assert_target_contained(
            &root,
            &home.join(".config/quotabar-custom"),
            "primary target"
        )
        .is_err());
        fs::remove_file(home.join(".config")).unwrap();

        let legacy_parent = home.join("Library/Application Support");
        fs::create_dir_all(&legacy_parent).unwrap();
        symlink(&victim, legacy_parent.join("com.majiayu.quotabar")).unwrap();
        assert!(assert_target_contained(
            &root,
            &legacy_parent.join("com.majiayu.quotabar"),
            "legacy target"
        )
        .is_err());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn canonical_task_home_is_accepted() {
        let base = fixture_base();
        let root = base.join("root");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let (canonical_root, canonical_home) = validate_task_paths(&root, &home).unwrap();
        assert!(strictly_within(&canonical_root, &canonical_home));
        fs::remove_dir_all(&base).unwrap();
    }
}

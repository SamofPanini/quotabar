//! Task-owned macOS smoke for PR #19 R2.
//!
//! It builds the product Tauri context (and therefore uses its real bundle
//! identifier path resolver) but does not start the desktop event loop or any
//! provider request. The caller must provide an isolated HOME below the named
//! task root.

#[path = "../commands.rs"]
mod commands;
#[path = "../domain/mod.rs"]
mod domain;
#[path = "../services/mod.rs"]
mod services;

#[cfg(target_os = "macos")]
fn main() {
    use chrono::{DateTime, Utc};
    use services::{claude_snapshot::ClaudeSnapshotStore, state_location::StateProvenance};
    use std::os::unix::fs::MetadataExt;
    use std::{env, fs, path::PathBuf};
    use tauri::Manager;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    let smoke_root = PathBuf::from(
        env::var_os("QBI_P1_R2_SMOKE_ROOT")
            .expect("the smoke command must provide its task-owned root"),
    )
    .canonicalize()
    .expect("the task-owned smoke root must exist");
    let home = PathBuf::from(env::var_os("HOME").expect("the smoke command must provide HOME"));
    assert!(
        home.starts_with(&smoke_root),
        "the smoke must not run against a non-task-owned HOME"
    );

    let app = tauri::Builder::default()
        .build(tauri::generate_context!())
        .expect("the product Tauri context must build");
    let legacy_config = app
        .path()
        .app_config_dir()
        .expect("the product Tauri app-config path must resolve");
    let primary_config = services::state_location::primary_state_dir()
        .expect("the durable primary path must resolve");
    assert!(legacy_config.starts_with(&smoke_root));
    assert!(primary_config.starts_with(&smoke_root));
    assert!(!legacy_config.starts_with("/Applications"));
    assert!(!primary_config.starts_with("/Applications"));

    let legacy_state = legacy_config.join("claude-current-state");
    let primary_state = primary_config.join("claude-current-state");
    let first = commands::get_claude_current_snapshots(app.handle().clone()).unwrap();
    let second = commands::get_claude_current_snapshots(app.handle().clone()).unwrap();
    assert!(first.slots.is_empty() && second.slots.is_empty());
    assert_eq!(first.provenance, StateProvenance::None);
    assert_eq!(second.provenance, StateProvenance::None);
    assert!(
        !primary_config.exists(),
        "a fresh read must not create primary state"
    );
    assert!(
        !legacy_config.exists(),
        "a fresh read must not create legacy state"
    );

    fs::create_dir_all(legacy_state.parent().unwrap()).unwrap();
    let legacy = ClaudeSnapshotStore::at_root(legacy_state.clone()).unwrap();
    legacy
        .register_slot(
            services::claude_snapshot::AccountSlotId::parse("123e4567-e89b-42d3-a456-426614174050")
                .unwrap(),
            "smoke".into(),
            None,
            now(),
        )
        .unwrap();
    let legacy_state_file = legacy_state.join("current-state.json");
    let legacy_bytes = fs::read(&legacy_state_file).unwrap();
    let legacy_metadata = fs::metadata(&legacy_state).unwrap();
    let legacy_entries = fs::read_dir(&legacy_state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    let legacy_projection = commands::get_claude_current_snapshots(app.handle().clone()).unwrap();
    assert_eq!(legacy_projection.provenance, StateProvenance::Legacy);
    assert_eq!(fs::read(&legacy_state_file).unwrap(), legacy_bytes);
    let legacy_after = fs::metadata(&legacy_state).unwrap();
    assert_eq!(
        (
            legacy_metadata.mode(),
            legacy_metadata.ctime(),
            legacy_metadata.ctime_nsec()
        ),
        (
            legacy_after.mode(),
            legacy_after.ctime(),
            legacy_after.ctime_nsec()
        )
    );
    assert_eq!(
        fs::read_dir(&legacy_state)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        legacy_entries
    );

    fs::create_dir_all(&primary_config).unwrap();
    let primary = ClaudeSnapshotStore::at_root(primary_state.clone()).unwrap();
    primary
        .register_slot(
            services::claude_snapshot::AccountSlotId::parse("123e4567-e89b-42d3-a456-426614174051")
                .unwrap(),
            "primary".into(),
            None,
            now(),
        )
        .unwrap();
    fs::write(
        primary_config.join(services::codex_profiles::CONFIG_FILE),
        r#"{"version":1,"profiles":[]}"#,
    )
    .unwrap();
    let registry =
        tauri::async_runtime::block_on(commands::get_codex_profiles(app.handle().clone()))
            .expect("the product registry command must resolve");
    assert_eq!(registry.registry_provenance, StateProvenance::Primary);
    assert!(registry.registry_error.is_none());

    fs::remove_dir_all(&legacy_config).unwrap();
    assert!(
        primary_state.exists(),
        "controlled legacy removal must retain primary state"
    );
    let primary_projection = commands::get_claude_current_snapshots(app.handle().clone()).unwrap();
    assert_eq!(primary_projection.provenance, StateProvenance::Primary);
    println!(
        "QBI_P1_R2_SMOKE primary={} legacy={} provenance=primary registry=primary",
        primary_config.display(),
        legacy_config.display()
    );
}

#[cfg(not(target_os = "macos"))]
fn main() {
    panic!("QBI-P1 R2 local smoke is macOS-only");
}

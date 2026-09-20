//! Acquisition-neutral, current-only Claude quota state.
//!
//! This module deliberately has no provider, credential, browser, or IPC-write
//! dependency. Future trusted adapters can use the crate-private mutation API;
//! the webview can only obtain `ClaudeCurrentSnapshotsDto` projections.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::{Uuid, Variant};

#[cfg(unix)]
use std::{
    ffi::CString,
    os::unix::io::{AsRawFd, FromRawFd},
};

const SCHEMA_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_SLOTS: usize = 4;
const MAX_ALIAS_BYTES: usize = 48;
const FRESH_FOR: Duration = Duration::minutes(15);
const ROLLBACK_TOLERANCE: Duration = Duration::seconds(60);
const STATE_FILE: &str = "current-state.json";
const LOCK_FILE: &str = ".current-state.lock";
const TEMP_PREFIX: &str = ".current-state.tmp-";

#[cfg(test)]
#[derive(Clone)]
struct WriteTestHook {
    root: PathBuf,
    reached: std::sync::Arc<std::sync::Barrier>,
    resume: std::sync::Arc<std::sync::Barrier>,
    temp_path: std::sync::Arc<std::sync::Mutex<Option<PathBuf>>>,
}

#[cfg(test)]
static WRITE_TEST_HOOK: std::sync::OnceLock<std::sync::Mutex<Option<WriteTestHook>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn pause_after_temp_fsync(root: &Path, name: &str) {
    let hook = WRITE_TEST_HOOK
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap()
        .clone();
    if let Some(hook) = hook.filter(|hook| hook.root == root) {
        *hook.temp_path.lock().unwrap() = Some(root.join(name));
        hook.reached.wait();
        hook.resume.wait();
    }
}

#[cfg(not(test))]
fn pause_after_temp_fsync(_: &Path, _: &str) {}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct AccountSlotId(String);

impl fmt::Debug for AccountSlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccountSlotId(<opaque>)")
    }
}

impl AccountSlotId {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, SnapshotError> {
        let value = value.into();
        Uuid::parse_str(&value)
            .ok()
            .filter(|uuid| {
                uuid.get_version_num() == 4
                    && uuid.get_variant() == Variant::RFC4122
                    && uuid.hyphenated().to_string() == value
            })
            .map(|_| Self(value))
            .ok_or(SnapshotError::InvalidInput)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WindowKind {
    FiveHour,
    Weekly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BindingState {
    Unbound,
    Bound,
    Unverified,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanMetadata {
    Paid,
    Free,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservationStatus {
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceClass {
    CompletionSse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SafeErrorCode {
    Unavailable,
    MalformedPayload,
    UnsupportedObservation,
    ClockRollback,
}

#[derive(Clone)]
pub(crate) struct ObservationEnvelopeV1 {
    pub(crate) slot_id: AccountSlotId,
    pub(crate) binding_epoch: u64,
    pub(crate) sequence: u64,
    pub(crate) observed_at: DateTime<Utc>,
    pub(crate) status: ObservationStatus,
    pub(crate) source: Option<SourceClass>,
    pub(crate) windows: Vec<ObservationWindow>,
    pub(crate) error_code: Option<SafeErrorCode>,
}

impl fmt::Debug for ObservationEnvelopeV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ObservationEnvelopeV1(<redacted>)")
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ObservationWindow {
    pub(crate) kind: WindowKind,
    pub(crate) used_percent: f64,
    pub(crate) reset_at: Option<DateTime<Utc>>,
}

impl ObservationEnvelopeV1 {
    pub(crate) fn validate(&self, received_at: DateTime<Utc>) -> Result<(), SnapshotError> {
        if self.binding_epoch == 0
            || self.sequence == 0
            || self.observed_at > received_at + Duration::minutes(1)
        {
            return Err(SnapshotError::InvalidInput);
        }
        match self.status {
            ObservationStatus::Available => {
                if self.source != Some(SourceClass::CompletionSse)
                    || self.error_code.is_some()
                    || self.windows.is_empty()
                    || self.windows.len() > 2
                {
                    return Err(SnapshotError::InvalidInput);
                }
                if self.windows.iter().enumerate().any(|(index, window)| {
                    !window.used_percent.is_finite()
                        || !(0.0..=100.0).contains(&window.used_percent)
                        || window
                            .reset_at
                            .is_some_and(|reset| reset < received_at - Duration::minutes(1))
                        || (index == 1 && self.windows[0].kind != WindowKind::FiveHour)
                        || (index == 1 && window.kind != WindowKind::Weekly)
                }) {
                    return Err(SnapshotError::InvalidInput);
                }
            }
            ObservationStatus::Unavailable => {
                if self.source.is_some() || !self.windows.is_empty() || self.error_code.is_none() {
                    return Err(SnapshotError::InvalidInput);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeCurrentSnapshotsDto {
    pub(crate) slots: Vec<ClaudeSlotProjectionDto>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeSlotProjectionDto {
    pub(crate) alias: String,
    pub(crate) binding_state: BindingState,
    pub(crate) plan: Option<PlanMetadata>,
    pub(crate) five_hour: WindowProjectionDto,
    pub(crate) weekly: WindowProjectionDto,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WindowProjectionStatus {
    Fresh,
    Stale,
    Expired,
    Unavailable,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WindowProjectionDto {
    pub(crate) status: WindowProjectionStatus,
    pub(crate) used_percent: Option<f64>,
    pub(crate) reset_at: Option<DateTime<Utc>>,
    pub(crate) observed_at: Option<DateTime<Utc>>,
    pub(crate) last_error_code: Option<SafeErrorCode>,
}

// Deliberately no Debug implementation for the persisted aggregate. Its private
// binding ID, epoch, and sequence must not accidentally reach logs or diagnostics.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Aggregate {
    schema_version: u32,
    last_evaluated_wall_time: DateTime<Utc>,
    slots: Vec<SlotState>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SlotState {
    slot_id: AccountSlotId,
    alias: String,
    plan: Option<PlanMetadata>,
    binding_id: Option<BindingId>,
    binding_state: BindingState,
    binding_epoch: u64,
    next_sequence: u64,
    windows: Vec<WindowRecord>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
struct BindingId(String);

impl BindingId {
    fn generate() -> Self {
        Self(Uuid::new_v4().hyphenated().to_string())
    }

    fn is_canonical(&self) -> bool {
        Uuid::parse_str(&self.0).ok().is_some_and(|value| {
            value.get_version_num() == 4
                && value.get_variant() == Variant::RFC4122
                && value.hyphenated().to_string() == self.0
        })
    }
}

impl fmt::Debug for BindingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BindingId(<opaque>)")
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowRecord {
    kind: WindowKind,
    used_percent: Option<f64>,
    observed_at: Option<DateTime<Utc>>,
    received_at: Option<DateTime<Utc>>,
    reset_at: Option<DateTime<Utc>>,
    source: Option<SourceClass>,
    terminal_expired: bool,
    last_error_code: Option<SafeErrorCode>,
}

impl Aggregate {
    fn empty(now: DateTime<Utc>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            last_evaluated_wall_time: now,
            slots: vec![],
        }
    }

    fn validate(&self) -> Result<(), SnapshotError> {
        if self.schema_version != SCHEMA_VERSION || self.slots.len() > MAX_SLOTS {
            return Err(SnapshotError::InvalidState);
        }
        let mut ids = std::collections::HashSet::new();
        for slot in &self.slots {
            if AccountSlotId::parse(slot.slot_id.0.clone()).is_err()
                || !ids.insert(slot.slot_id.0.clone())
                || !safe_alias(&slot.alias)
                || slot.binding_epoch == 0
                || slot.next_sequence == 0
                || slot.windows.len() > 2
            {
                return Err(SnapshotError::InvalidState);
            }
            match slot.binding_state {
                BindingState::Bound => {
                    if !slot
                        .binding_id
                        .as_ref()
                        .is_some_and(BindingId::is_canonical)
                    {
                        return Err(SnapshotError::InvalidState);
                    }
                }
                BindingState::Unbound | BindingState::Unverified | BindingState::Error => {
                    if slot.binding_id.is_some() || !slot.windows.is_empty() {
                        return Err(SnapshotError::InvalidState);
                    }
                }
            }
            let mut kinds = std::collections::HashSet::new();
            for window in &slot.windows {
                if !kinds.insert(window.kind as u8)
                    || window
                        .used_percent
                        .is_some_and(|v| !v.is_finite() || !(0.0..=100.0).contains(&v))
                    || window.observed_at.is_some() != window.received_at.is_some()
                    || (window.used_percent.is_some() && window.source.is_none())
                    || (window.terminal_expired && window.used_percent.is_some())
                    || (!window.terminal_expired && window.used_percent.is_none())
                    || window.received_at.is_some_and(|received| {
                        received > self.last_evaluated_wall_time + Duration::minutes(1)
                    })
                {
                    return Err(SnapshotError::InvalidState);
                }
                if let (Some(observed), Some(received), Some(reset)) =
                    (window.observed_at, window.received_at, window.reset_at)
                {
                    if observed > received + Duration::minutes(1)
                        || reset < observed - Duration::minutes(1)
                    {
                        return Err(SnapshotError::InvalidState);
                    }
                }
            }
        }
        Ok(())
    }
}

pub(crate) struct ClaudeSnapshotStore {
    root: PathBuf,
    #[cfg(unix)]
    root_dir: File,
}

impl ClaudeSnapshotStore {
    pub(crate) fn in_app_config(config_dir: &Path) -> Result<Self, SnapshotError> {
        validate_no_symlink_ancestors(config_dir)?;
        let config_dir = config_dir.canonicalize().map_err(|_| SnapshotError::Io)?;
        let root = config_dir.join("claude-current-state");
        Self::at_root(root)
    }

    pub(crate) fn at_root(root: PathBuf) -> Result<Self, SnapshotError> {
        platform_supported()?;
        let parent = root.parent().ok_or(SnapshotError::InvalidInput)?;
        let name = root.file_name().ok_or(SnapshotError::InvalidInput)?;
        let root = parent
            .canonicalize()
            .map_err(|_| SnapshotError::Io)?
            .join(name);
        ensure_root(&root)?;
        #[cfg(unix)]
        let root_dir = open_directory(&root)?;
        Ok(Self {
            root,
            #[cfg(unix)]
            root_dir,
        })
    }

    pub(crate) fn register_slot(
        &self,
        slot_id: AccountSlotId,
        alias: String,
        plan: Option<PlanMetadata>,
        now: DateTime<Utc>,
    ) -> Result<(), SnapshotError> {
        if !safe_alias(&alias) {
            return Err(SnapshotError::InvalidInput);
        }
        self.mutate(now, |aggregate| {
            if aggregate.slots.iter().any(|slot| slot.slot_id == slot_id)
                || aggregate.slots.len() == MAX_SLOTS
            {
                return Err(SnapshotError::InvalidInput);
            }
            aggregate.slots.push(SlotState {
                slot_id,
                alias,
                plan,
                binding_id: Some(BindingId::generate()),
                binding_state: BindingState::Bound,
                binding_epoch: 1,
                next_sequence: 1,
                windows: vec![],
            });
            Ok(())
        })
    }

    pub(crate) fn apply_observation(
        &self,
        observation: ObservationEnvelopeV1,
        received_at: DateTime<Utc>,
    ) -> Result<(), SnapshotError> {
        observation.validate(received_at)?;
        self.mutate(received_at, |aggregate| {
            let slot = aggregate
                .slots
                .iter_mut()
                .find(|slot| slot.slot_id == observation.slot_id)
                .ok_or(SnapshotError::InvalidInput)?;
            if slot.binding_state != BindingState::Bound
                || observation.binding_epoch != slot.binding_epoch
                || observation.sequence != slot.next_sequence
            {
                return Err(SnapshotError::Rejected);
            }
            match observation.status {
                ObservationStatus::Available => {
                    for incoming in observation.windows {
                        let record = WindowRecord {
                            kind: incoming.kind,
                            used_percent: Some(incoming.used_percent),
                            observed_at: Some(observation.observed_at),
                            received_at: Some(received_at),
                            reset_at: incoming.reset_at,
                            source: observation.source,
                            terminal_expired: false,
                            last_error_code: None,
                        };
                        replace_window(&mut slot.windows, record);
                    }
                }
                ObservationStatus::Unavailable => {
                    for window in &mut slot.windows {
                        window.last_error_code = observation.error_code;
                    }
                }
            }
            slot.next_sequence = slot
                .next_sequence
                .checked_add(1)
                .ok_or(SnapshotError::Rejected)?;
            Ok(())
        })
    }

    pub(crate) fn mark_unverified(
        &self,
        slot_id: &AccountSlotId,
        now: DateTime<Utc>,
    ) -> Result<(), SnapshotError> {
        self.mutate(now, |aggregate| {
            let slot = aggregate
                .slots
                .iter_mut()
                .find(|slot| &slot.slot_id == slot_id)
                .ok_or(SnapshotError::InvalidInput)?;
            if slot.binding_state != BindingState::Unverified {
                slot.binding_epoch = slot
                    .binding_epoch
                    .checked_add(1)
                    .ok_or(SnapshotError::Rejected)?;
            }
            slot.next_sequence = 1;
            slot.binding_state = BindingState::Unverified;
            slot.binding_id = None;
            slot.windows.clear();
            Ok(())
        })
    }

    pub(crate) fn rebind(
        &self,
        slot_id: &AccountSlotId,
        now: DateTime<Utc>,
    ) -> Result<(), SnapshotError> {
        self.mutate(now, |aggregate| {
            let slot = aggregate
                .slots
                .iter_mut()
                .find(|slot| &slot.slot_id == slot_id)
                .ok_or(SnapshotError::InvalidInput)?;
            if slot.binding_state != BindingState::Unverified {
                slot.binding_epoch = slot
                    .binding_epoch
                    .checked_add(1)
                    .ok_or(SnapshotError::Rejected)?;
            }
            slot.binding_id = Some(BindingId::generate());
            slot.binding_state = BindingState::Bound;
            slot.next_sequence = 1;
            slot.windows.clear();
            Ok(())
        })
    }

    pub(crate) fn unpair(
        &self,
        slot_id: &AccountSlotId,
        now: DateTime<Utc>,
    ) -> Result<(), SnapshotError> {
        self.mutate(now, |aggregate| {
            let slot = aggregate
                .slots
                .iter_mut()
                .find(|slot| &slot.slot_id == slot_id)
                .ok_or(SnapshotError::InvalidInput)?;
            slot.binding_id = None;
            slot.binding_state = BindingState::Unbound;
            slot.next_sequence = 1;
            slot.windows.clear();
            Ok(())
        })
    }

    pub(crate) fn project(
        &self,
        now: DateTime<Utc>,
    ) -> Result<ClaudeCurrentSnapshotsDto, SnapshotError> {
        let _lock = self.lock()?;
        cleanup_orphan_temps(&self.root)?;
        let mut aggregate = self.load(now)?;
        if impossible_clock(&aggregate, now) {
            if fail_closed_for_clock(&mut aggregate, now) {
                self.write_aggregate(&aggregate)?;
            }
        }
        let changed = project_expiry(&mut aggregate, now);
        if changed {
            self.write_aggregate(&aggregate)?;
        }
        Ok(ClaudeCurrentSnapshotsDto {
            slots: aggregate
                .slots
                .iter()
                .map(|slot| ClaudeSlotProjectionDto {
                    alias: slot.alias.clone(),
                    binding_state: slot.binding_state,
                    plan: slot.plan,
                    five_hour: projection_for(slot, WindowKind::FiveHour, now),
                    weekly: projection_for(slot, WindowKind::Weekly, now),
                })
                .collect(),
        })
    }

    fn mutate<F>(&self, now: DateTime<Utc>, change: F) -> Result<(), SnapshotError>
    where
        F: FnOnce(&mut Aggregate) -> Result<(), SnapshotError>,
    {
        let _lock = self.lock()?;
        cleanup_orphan_temps(&self.root)?;
        let mut aggregate = self.load(now)?;
        if impossible_clock(&aggregate, now) {
            let changed = fail_closed_for_clock(&mut aggregate, now);
            if changed {
                self.write_aggregate(&aggregate)?;
            }
            return Err(SnapshotError::Rejected);
        }
        project_expiry(&mut aggregate, now);
        change(&mut aggregate)?;
        aggregate.last_evaluated_wall_time = aggregate.last_evaluated_wall_time.max(now);
        aggregate.validate()?;
        self.write_aggregate(&aggregate)
    }

    fn load(&self, now: DateTime<Utc>) -> Result<Aggregate, SnapshotError> {
        self.verify_root()?;
        #[cfg(unix)]
        return self.load_from_pinned_root(now);
        #[cfg(not(unix))]
        return Err(SnapshotError::Unsupported);
    }

    #[cfg(unix)]
    fn load_from_pinned_root(&self, now: DateTime<Utc>) -> Result<Aggregate, SnapshotError> {
        let path_metadata = match child_metadata(&self.root_dir, STATE_FILE) {
            Ok(metadata) => metadata,
            Err(SnapshotError::NotFound) => return Ok(Aggregate::empty(now)),
            Err(error) => return Err(error),
        };
        if path_metadata.is_symlink() {
            return Err(SnapshotError::InvalidState);
        }
        let mut file = open_child_existing(&self.root_dir, STATE_FILE)?;
        let metadata = file.metadata().map_err(|_| SnapshotError::Io)?;
        validate_open_regular_owned(&metadata, 0o600)?;
        if !path_metadata.matches(&metadata) {
            return Err(SnapshotError::InvalidState);
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(SnapshotError::InvalidState);
        }
        let mut content = Vec::with_capacity(metadata.len() as usize + 1);
        std::io::Read::by_ref(&mut file)
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut content)
            .map_err(|_| SnapshotError::Io)?;
        if content.len() as u64 > MAX_FILE_BYTES {
            return Err(SnapshotError::InvalidState);
        }
        let aggregate: Aggregate =
            serde_json::from_slice(&content).map_err(|_| SnapshotError::InvalidState)?;
        aggregate.validate()?;
        Ok(aggregate)
    }

    fn write_aggregate(&self, aggregate: &Aggregate) -> Result<(), SnapshotError> {
        self.verify_root()?;
        #[cfg(unix)]
        return self.write_to_pinned_root(aggregate);
        #[cfg(not(unix))]
        return Err(SnapshotError::Unsupported);
    }

    #[cfg(unix)]
    fn write_to_pinned_root(&self, aggregate: &Aggregate) -> Result<(), SnapshotError> {
        let bytes = serde_json::to_vec(aggregate).map_err(|_| SnapshotError::InvalidState)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(SnapshotError::InvalidState);
        }
        let temp = format!("{TEMP_PREFIX}{}-{}", std::process::id(), Uuid::new_v4());
        let mut file = open_child_new(&self.root_dir, &temp)?;
        file.write_all(&bytes).map_err(|_| SnapshotError::Io)?;
        file.sync_all().map_err(|_| SnapshotError::Io)?;
        pause_after_temp_fsync(&self.root, &temp);
        let path_metadata = child_metadata(&self.root_dir, &temp)?;
        let file_metadata = file.metadata().map_err(|_| SnapshotError::Io)?;
        if !path_metadata.matches(&file_metadata) {
            return Err(SnapshotError::InvalidState);
        }
        rename_child(&self.root_dir, &temp, STATE_FILE)?;
        self.root_dir.sync_all().map_err(|_| SnapshotError::Io)?;
        Ok(())
    }

    fn lock(&self) -> Result<LockGuard, SnapshotError> {
        self.verify_root()?;
        #[cfg(unix)]
        {
            return LockGuard::acquire(&self.root_dir);
        }
        #[cfg(not(unix))]
        Err(SnapshotError::Unsupported)
    }

    fn verify_root(&self) -> Result<(), SnapshotError> {
        #[cfg(unix)]
        {
            let path_metadata = fs::symlink_metadata(&self.root).map_err(|_| SnapshotError::Io)?;
            let descriptor_metadata = self.root_dir.metadata().map_err(|_| SnapshotError::Io)?;
            validate_open_directory_owned(&descriptor_metadata)?;
            if path_metadata.file_type().is_symlink()
                || !same_file_identity(&path_metadata, &descriptor_metadata)
            {
                return Err(SnapshotError::InvalidState);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotError {
    InvalidInput,
    InvalidState,
    Rejected,
    Io,
    Unsupported,
    NotFound,
}

fn safe_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= MAX_ALIAS_BYTES
        && alias
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '/' && c != '\\')
}

fn replace_window(windows: &mut Vec<WindowRecord>, record: WindowRecord) {
    if let Some(existing) = windows
        .iter_mut()
        .find(|existing| existing.kind == record.kind)
    {
        *existing = record;
    } else {
        windows.push(record);
        windows.sort_by_key(|window| match window.kind {
            WindowKind::FiveHour => 0,
            WindowKind::Weekly => 1,
        });
    }
}

fn project_expiry(aggregate: &mut Aggregate, now: DateTime<Utc>) -> bool {
    let rollback = now + ROLLBACK_TOLERANCE < aggregate.last_evaluated_wall_time;
    let mut changed = rollback;
    for slot in &mut aggregate.slots {
        for window in &mut slot.windows {
            let expired = rollback
                || window.terminal_expired
                || window.reset_at.is_some_and(|reset| now >= reset)
                || window.reset_at.is_none()
                    && window
                        .received_at
                        .is_some_and(|received| now > received + FRESH_FOR);
            if expired && (!window.terminal_expired || window.used_percent.is_some()) {
                window.terminal_expired = true;
                window.used_percent = None;
                if rollback {
                    window.last_error_code = Some(SafeErrorCode::ClockRollback);
                }
                changed = true;
            }
        }
    }
    if now > aggregate.last_evaluated_wall_time {
        aggregate.last_evaluated_wall_time = now;
        changed = true;
    }
    changed
}

fn impossible_clock(aggregate: &Aggregate, now: DateTime<Utc>) -> bool {
    now + ROLLBACK_TOLERANCE < aggregate.last_evaluated_wall_time
        || aggregate
            .slots
            .iter()
            .flat_map(|slot| &slot.windows)
            .any(|window| {
                window
                    .received_at
                    .is_some_and(|received| now + ROLLBACK_TOLERANCE < received)
            })
}

fn fail_closed_for_clock(aggregate: &mut Aggregate, now: DateTime<Utc>) -> bool {
    let aggregate_rollback = now + ROLLBACK_TOLERANCE < aggregate.last_evaluated_wall_time;
    let mut changed = false;
    for slot in &mut aggregate.slots {
        for window in &mut slot.windows {
            let window_rollback = window
                .received_at
                .is_some_and(|received| now + ROLLBACK_TOLERANCE < received);
            if (aggregate_rollback || window_rollback)
                && (window.used_percent.take().is_some() || !window.terminal_expired)
            {
                window.terminal_expired = true;
                window.last_error_code = Some(SafeErrorCode::ClockRollback);
                changed = true;
            }
        }
    }
    changed
}

fn projection_for(slot: &SlotState, kind: WindowKind, now: DateTime<Utc>) -> WindowProjectionDto {
    if slot.binding_state != BindingState::Bound {
        return WindowProjectionDto {
            status: WindowProjectionStatus::Unavailable,
            used_percent: None,
            reset_at: None,
            observed_at: None,
            last_error_code: Some(SafeErrorCode::Unavailable),
        };
    }
    let Some(window) = slot.windows.iter().find(|window| window.kind == kind) else {
        return WindowProjectionDto {
            status: WindowProjectionStatus::Unavailable,
            used_percent: None,
            reset_at: None,
            observed_at: None,
            last_error_code: None,
        };
    };
    if window.terminal_expired || window.used_percent.is_none() {
        return WindowProjectionDto {
            status: if window.reset_at.is_some() {
                WindowProjectionStatus::Expired
            } else {
                WindowProjectionStatus::Unavailable
            },
            used_percent: None,
            reset_at: window.reset_at,
            observed_at: window.observed_at,
            last_error_code: window.last_error_code,
        };
    }
    let status = if window
        .received_at
        .is_some_and(|received| now <= received + FRESH_FOR)
    {
        WindowProjectionStatus::Fresh
    } else {
        WindowProjectionStatus::Stale
    };
    WindowProjectionDto {
        status,
        used_percent: window.used_percent,
        reset_at: window.reset_at,
        observed_at: window.observed_at,
        last_error_code: window.last_error_code,
    }
}

fn ensure_root(root: &Path) -> Result<(), SnapshotError> {
    validate_no_symlink_ancestors(root)?;
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(SnapshotError::InvalidState),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(root).map_err(|_| SnapshotError::Io)?
        }
        Err(_) => return Err(SnapshotError::Io),
    }
    #[cfg(unix)]
    {
        fs::set_permissions(root, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .map_err(|_| SnapshotError::Io)?;
    }
    validate_directory_owned(root)
}

#[cfg(unix)]
fn platform_supported() -> Result<(), SnapshotError> {
    Ok(())
}

#[cfg(not(unix))]
fn platform_supported() -> Result<(), SnapshotError> {
    // Current crash-safety depends on no-follow opens, descriptor checks, and
    // advisory locking. Refuse persistence where those guarantees are absent.
    Err(SnapshotError::Unsupported)
}

fn validate_no_symlink_ancestors(path: &Path) -> Result<(), SnapshotError> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(SnapshotError::InvalidState)
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(SnapshotError::Io),
        }
    }
    Ok(())
}

fn validate_directory_owned(path: &Path) -> Result<(), SnapshotError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| SnapshotError::Io)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(SnapshotError::InvalidState);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(SnapshotError::InvalidState);
        }
    }
    Ok(())
}

fn validate_open_directory_owned(metadata: &fs::Metadata) -> Result<(), SnapshotError> {
    if !metadata.file_type().is_dir() {
        return Err(SnapshotError::InvalidState);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(SnapshotError::InvalidState);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_directory(path: &Path) -> Result<File, SnapshotError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path).map_err(|_| SnapshotError::Io)?;
    validate_open_directory_owned(&file.metadata().map_err(|_| SnapshotError::Io)?)?;
    Ok(file)
}

#[cfg(unix)]
struct ChildMetadata {
    dev: u64,
    ino: u64,
    mode: u32,
}

#[cfg(unix)]
impl ChildMetadata {
    fn is_symlink(&self) -> bool {
        self.mode & libc::S_IFMT as u32 == libc::S_IFLNK as u32
    }

    fn matches(&self, metadata: &fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        self.dev == metadata.dev() && self.ino == metadata.ino()
    }
}

#[cfg(unix)]
fn child_metadata(root: &File, name: &str) -> Result<ChildMetadata, SnapshotError> {
    let name = CString::new(name).map_err(|_| SnapshotError::InvalidInput)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            root.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            Err(SnapshotError::NotFound)
        } else {
            Err(SnapshotError::Io)
        };
    }
    let stat = unsafe { stat.assume_init() };
    Ok(ChildMetadata {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
        mode: stat.st_mode as u32,
    })
}

#[cfg(unix)]
fn open_child_existing(root: &File, name: &str) -> Result<File, SnapshotError> {
    openat(root, name, libc::O_RDONLY | libc::O_NOFOLLOW, 0)
}

#[cfg(unix)]
fn open_child_new(root: &File, name: &str) -> Result<File, SnapshotError> {
    openat(
        root,
        name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
        0o600,
    )
}

#[cfg(unix)]
fn openat(root: &File, name: &str, flags: i32, mode: u32) -> Result<File, SnapshotError> {
    let name = CString::new(name).map_err(|_| SnapshotError::InvalidInput)?;
    let fd = unsafe { libc::openat(root.as_raw_fd(), name.as_ptr(), flags, mode) };
    if fd < 0 {
        return Err(SnapshotError::Io);
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn rename_child(root: &File, from: &str, to: &str) -> Result<(), SnapshotError> {
    let from = CString::new(from).map_err(|_| SnapshotError::InvalidInput)?;
    let to = CString::new(to).map_err(|_| SnapshotError::InvalidInput)?;
    if unsafe {
        libc::renameat(
            root.as_raw_fd(),
            from.as_ptr(),
            root.as_raw_fd(),
            to.as_ptr(),
        )
    } != 0
    {
        return Err(SnapshotError::Io);
    }
    Ok(())
}

fn validate_regular_owned(path: &Path, expected_mode: u32) -> Result<(), SnapshotError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| SnapshotError::Io)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(SnapshotError::InvalidState);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != expected_mode
        {
            return Err(SnapshotError::InvalidState);
        }
    }
    Ok(())
}

fn validate_open_regular_owned(
    metadata: &fs::Metadata,
    expected_mode: u32,
) -> Result<(), SnapshotError> {
    if !metadata.file_type().is_file() {
        return Err(SnapshotError::InvalidState);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != expected_mode
        {
            return Err(SnapshotError::InvalidState);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(_: &fs::Metadata, _: &fs::Metadata) -> bool {
    false
}

fn open_private_new(path: &Path) -> Result<File, SnapshotError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(|_| SnapshotError::Io)
}

fn open_private_existing(path: &Path) -> Result<File, SnapshotError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(|_| SnapshotError::Io)
}

fn sync_directory(path: &Path) -> Result<(), SnapshotError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|_| SnapshotError::Io)?;
    }
    Ok(())
}

fn cleanup_orphan_temps(root: &Path) -> Result<(), SnapshotError> {
    for entry in fs::read_dir(root).map_err(|_| SnapshotError::Io)? {
        let entry = entry.map_err(|_| SnapshotError::Io)?;
        if is_inactive_temp_name(&entry.file_name().to_string_lossy()) {
            validate_regular_owned(&entry.path(), 0o600)?;
            fs::remove_file(entry.path()).map_err(|_| SnapshotError::Io)?;
        }
    }
    Ok(())
}

fn is_inactive_temp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(TEMP_PREFIX) else {
        return false;
    };
    let Some((pid, nonce)) = rest.split_once('-') else {
        return false;
    };
    pid.parse::<u32>().is_ok() && Uuid::parse_str(nonce).is_ok()
}

struct LockGuard {
    file: File,
}
impl LockGuard {
    #[cfg(unix)]
    fn acquire(root: &File) -> Result<Self, SnapshotError> {
        let file = match openat(
            root,
            LOCK_FILE,
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW,
            0o600,
        ) {
            Ok(file) => file,
            Err(_) => return Err(SnapshotError::Io),
        };
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .map_err(|_| SnapshotError::Io)?;
        let file_metadata = file.metadata().map_err(|_| SnapshotError::Io)?;
        validate_open_regular_owned(&file_metadata, 0o600)?;
        #[cfg(unix)]
        {
            if unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) }
                != 0
            {
                return Err(SnapshotError::Io);
            }
        }
        let path_metadata = child_metadata(root, LOCK_FILE)?;
        if path_metadata.is_symlink() || !path_metadata.matches(&file_metadata) {
            return Err(SnapshotError::InvalidState);
        }
        Ok(Self { file })
    }
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = unsafe {
                libc::flock(
                    std::os::unix::io::AsRawFd::as_raw_fd(&self.file),
                    libc::LOCK_UN,
                )
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }
    fn root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "quotabar-c3a-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
    fn store(name: &str) -> ClaudeSnapshotStore {
        ClaudeSnapshotStore::at_root(root(name)).unwrap()
    }
    fn slot(value: &str) -> AccountSlotId {
        AccountSlotId::parse(value).unwrap()
    }
    fn register(store: &ClaudeSnapshotStore, id: AccountSlotId, now: DateTime<Utc>) {
        store
            .register_slot(id, "safe".into(), Some(PlanMetadata::Paid), now)
            .unwrap();
    }
    fn available(
        id: AccountSlotId,
        epoch: u64,
        sequence: u64,
        windows: Vec<ObservationWindow>,
    ) -> ObservationEnvelopeV1 {
        ObservationEnvelopeV1 {
            slot_id: id,
            binding_epoch: epoch,
            sequence,
            observed_at: now(),
            status: ObservationStatus::Available,
            source: Some(SourceClass::CompletionSse),
            windows,
            error_code: None,
        }
    }
    fn window(kind: WindowKind, percent: f64, reset: Option<DateTime<Utc>>) -> ObservationWindow {
        ObservationWindow {
            kind,
            used_percent: percent,
            reset_at: reset,
        }
    }
    fn projection(store: &ClaudeSnapshotStore, now: DateTime<Utc>) -> ClaudeSlotProjectionDto {
        store.project(now).unwrap().slots.remove(0)
    }

    fn overwrite_aggregate(store: &ClaudeSnapshotStore, aggregate: &Aggregate) {
        let path = store.root.join(STATE_FILE);
        let bytes = serde_json::to_vec(aggregate).unwrap();
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
        #[cfg(unix)]
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
    }

    #[test]
    fn weekly_expiry_hides_only_weekly() {
        let s = store("weekly");
        let id = slot("123e4567-e89b-42d3-a456-426614174000");
        register(&s, id.clone(), now());
        s.apply_observation(
            available(
                id,
                1,
                1,
                vec![
                    window(WindowKind::FiveHour, 20.0, Some(now() + Duration::hours(2))),
                    window(WindowKind::Weekly, 40.0, Some(now() + Duration::minutes(1))),
                ],
            ),
            now(),
        )
        .unwrap();
        let p = projection(&s, now() + Duration::minutes(2));
        assert!(p.five_hour.used_percent.is_some());
        assert!(p.weekly.used_percent.is_none());
    }
    #[test]
    fn five_hour_expiry_hides_only_five_hour() {
        let s = store("five");
        let id = slot("123e4567-e89b-42d3-a456-426614174001");
        register(&s, id.clone(), now());
        s.apply_observation(
            available(
                id,
                1,
                1,
                vec![
                    window(
                        WindowKind::FiveHour,
                        20.0,
                        Some(now() + Duration::minutes(1)),
                    ),
                    window(WindowKind::Weekly, 40.0, Some(now() + Duration::hours(2))),
                ],
            ),
            now(),
        )
        .unwrap();
        let p = projection(&s, now() + Duration::minutes(2));
        assert!(p.five_hour.used_percent.is_none());
        assert!(p.weekly.used_percent.is_some());
    }
    #[test]
    fn reset_never_unpairs_and_post_reset_repopulates() {
        let s = store("reset");
        let id = slot("123e4567-e89b-42d3-a456-426614174002");
        register(&s, id.clone(), now());
        s.apply_observation(
            available(
                id.clone(),
                1,
                1,
                vec![window(
                    WindowKind::FiveHour,
                    20.0,
                    Some(now() + Duration::minutes(1)),
                )],
            ),
            now(),
        )
        .unwrap();
        assert_eq!(
            projection(&s, now() + Duration::minutes(2)).binding_state,
            BindingState::Bound
        );
        s.apply_observation(
            available(
                id,
                1,
                2,
                vec![window(
                    WindowKind::FiveHour,
                    1.0,
                    Some(now() + Duration::hours(1)),
                )],
            ),
            now() + Duration::minutes(2),
        )
        .unwrap();
        assert_eq!(
            projection(&s, now() + Duration::minutes(2))
                .five_hour
                .used_percent,
            Some(1.0)
        );
    }
    #[test]
    fn missing_reset_becomes_unavailable() {
        let s = store("missing-reset");
        let id = slot("123e4567-e89b-42d3-a456-426614174003");
        register(&s, id.clone(), now());
        s.apply_observation(
            available(id, 1, 1, vec![window(WindowKind::FiveHour, 20.0, None)]),
            now(),
        )
        .unwrap();
        assert_eq!(
            projection(&s, now() + Duration::minutes(16))
                .five_hour
                .status,
            WindowProjectionStatus::Unavailable
        );
    }
    #[test]
    fn partial_update_preserves_other_window() {
        let s = store("partial");
        let id = slot("123e4567-e89b-42d3-a456-426614174004");
        register(&s, id.clone(), now());
        s.apply_observation(
            available(
                id.clone(),
                1,
                1,
                vec![
                    window(WindowKind::FiveHour, 20.0, Some(now() + Duration::hours(1))),
                    window(WindowKind::Weekly, 40.0, Some(now() + Duration::hours(1))),
                ],
            ),
            now(),
        )
        .unwrap();
        s.apply_observation(
            available(
                id,
                1,
                2,
                vec![window(
                    WindowKind::FiveHour,
                    30.0,
                    Some(now() + Duration::hours(1)),
                )],
            ),
            now(),
        )
        .unwrap();
        let p = projection(&s, now());
        assert_eq!(p.five_hour.used_percent, Some(30.0));
        assert_eq!(p.weekly.used_percent, Some(40.0));
    }
    #[test]
    fn old_epoch_replay_and_skipped_sequence_mutate_nothing() {
        let s = store("sequence");
        let id = slot("123e4567-e89b-42d3-a456-426614174005");
        register(&s, id.clone(), now());
        let one = available(
            id.clone(),
            1,
            1,
            vec![window(
                WindowKind::FiveHour,
                20.0,
                Some(now() + Duration::hours(1)),
            )],
        );
        s.apply_observation(one.clone(), now()).unwrap();
        for bad in [
            available(
                id.clone(),
                0,
                2,
                vec![window(WindowKind::FiveHour, 1.0, None)],
            ),
            available(
                id.clone(),
                1,
                1,
                vec![window(WindowKind::FiveHour, 1.0, None)],
            ),
            available(id, 1, 3, vec![window(WindowKind::FiveHour, 1.0, None)]),
        ] {
            assert!(s.apply_observation(bad, now()).is_err());
        }
        assert_eq!(projection(&s, now()).five_hour.used_percent, Some(20.0));
    }
    #[test]
    fn uncertainty_hides_old_values_immediately() {
        let s = store("uncertain");
        let id = slot("123e4567-e89b-42d3-a456-426614174006");
        register(&s, id.clone(), now());
        s.apply_observation(
            available(
                id.clone(),
                1,
                1,
                vec![window(
                    WindowKind::FiveHour,
                    20.0,
                    Some(now() + Duration::hours(1)),
                )],
            ),
            now(),
        )
        .unwrap();
        s.mark_unverified(&id, now()).unwrap();
        let p = projection(&s, now());
        assert_eq!(p.binding_state, BindingState::Unverified);
        assert_eq!(p.five_hour.used_percent, None);
    }

    #[test]
    fn uncertainty_rebind_rejects_prior_epoch_and_accepts_new_epoch() {
        let s = store("rebind");
        let id = slot("123e4567-e89b-42d3-a456-42661417400e");
        register(&s, id.clone(), now());
        s.apply_observation(
            available(
                id.clone(),
                1,
                1,
                vec![window(
                    WindowKind::FiveHour,
                    20.0,
                    Some(now() + Duration::hours(1)),
                )],
            ),
            now(),
        )
        .unwrap();
        s.mark_unverified(&id, now()).unwrap();
        s.rebind(&id, now()).unwrap();
        assert!(s
            .apply_observation(
                available(
                    id.clone(),
                    1,
                    2,
                    vec![window(
                        WindowKind::FiveHour,
                        90.0,
                        Some(now() + Duration::hours(1))
                    )]
                ),
                now()
            )
            .is_err());
        s.apply_observation(
            available(
                id,
                2,
                1,
                vec![window(
                    WindowKind::FiveHour,
                    30.0,
                    Some(now() + Duration::hours(1)),
                )],
            ),
            now(),
        )
        .unwrap();
        assert_eq!(projection(&s, now()).five_hour.used_percent, Some(30.0));
    }

    #[test]
    fn rollback_next_sequence_is_rejected_and_terminal_state_survives_restart() {
        let path = root("rollback-next");
        let s = ClaudeSnapshotStore::at_root(path.clone()).unwrap();
        let id = slot("123e4567-e89b-42d3-a456-42661417400f");
        register(&s, id.clone(), now());
        let future = now() + Duration::hours(1);
        s.apply_observation(
            available(
                id.clone(),
                1,
                1,
                vec![window(
                    WindowKind::FiveHour,
                    20.0,
                    Some(future + Duration::hours(1)),
                )],
            ),
            future,
        )
        .unwrap();
        assert_eq!(
            s.apply_observation(
                available(
                    id,
                    1,
                    2,
                    vec![window(
                        WindowKind::FiveHour,
                        30.0,
                        Some(future + Duration::hours(2))
                    )]
                ),
                now()
            ),
            Err(SnapshotError::Rejected)
        );
        let restarted = ClaudeSnapshotStore::at_root(path.clone()).unwrap();
        assert_eq!(projection(&restarted, future).five_hour.used_percent, None);
        let restarted_again = ClaudeSnapshotStore::at_root(path).unwrap();
        assert_eq!(
            projection(&restarted_again, future).five_hour.used_percent,
            None
        );
    }
    #[test]
    fn rollback_cannot_resurrect_after_reload() {
        let s = store("rollback");
        let id = slot("123e4567-e89b-42d3-a456-426614174007");
        register(&s, id.clone(), now());
        s.apply_observation(
            available(
                id,
                1,
                1,
                vec![window(
                    WindowKind::FiveHour,
                    20.0,
                    Some(now() + Duration::minutes(1)),
                )],
            ),
            now(),
        )
        .unwrap();
        assert_eq!(
            projection(&s, now() + Duration::minutes(2))
                .five_hour
                .used_percent,
            None
        );
        assert_eq!(projection(&s, now()).five_hour.used_percent, None);
    }
    #[test]
    fn atomic_file_is_complete_old_or_new() {
        let s = store("atomic");
        let id = slot("123e4567-e89b-42d3-a456-426614174008");
        register(&s, id.clone(), now());
        s.apply_observation(
            available(
                id,
                1,
                1,
                vec![window(
                    WindowKind::FiveHour,
                    20.0,
                    Some(now() + Duration::hours(1)),
                )],
            ),
            now(),
        )
        .unwrap();
        let data = fs::read(s.root.join(STATE_FILE)).unwrap();
        let _: Aggregate = serde_json::from_slice(&data).unwrap();
    }
    #[test]
    fn slots_are_independent_and_safe_dto_has_no_private_fields() {
        let s = store("slots");
        let a = slot("123e4567-e89b-42d3-a456-426614174009");
        let b = slot("123e4567-e89b-42d3-a456-42661417400a");
        register(&s, a.clone(), now());
        register(&s, b.clone(), now());
        s.apply_observation(
            available(
                a,
                1,
                1,
                vec![window(
                    WindowKind::FiveHour,
                    20.0,
                    Some(now() + Duration::hours(1)),
                )],
            ),
            now(),
        )
        .unwrap();
        s.apply_observation(
            ObservationEnvelopeV1 {
                slot_id: b,
                binding_epoch: 1,
                sequence: 1,
                observed_at: now(),
                status: ObservationStatus::Unavailable,
                source: None,
                windows: vec![],
                error_code: Some(SafeErrorCode::Unavailable),
            },
            now(),
        )
        .unwrap();
        let json = serde_json::to_string(&s.project(now()).unwrap()).unwrap();
        assert!(
            !json.contains("bindingId")
                && !json.contains("bindingEpoch")
                && !json.contains("sequence")
                && !json.contains("BindingId")
        );
    }
    #[test]
    fn malformed_event_does_not_contaminate_another_slot() {
        let s = store("malformed-isolation");
        let a = slot("123e4567-e89b-42d3-a456-42661417400b");
        let b = slot("123e4567-e89b-42d3-a456-42661417400c");
        register(&s, a.clone(), now());
        register(&s, b.clone(), now());
        s.apply_observation(
            available(
                b,
                1,
                1,
                vec![window(
                    WindowKind::Weekly,
                    44.0,
                    Some(now() + Duration::hours(1)),
                )],
            ),
            now(),
        )
        .unwrap();
        assert!(s
            .apply_observation(
                available(a, 1, 1, vec![window(WindowKind::FiveHour, 101.0, None)]),
                now()
            )
            .is_err());
        let dto = s.project(now()).unwrap();
        assert_eq!(dto.slots[1].weekly.used_percent, Some(44.0));
    }

    #[test]
    fn offline_restart_has_same_safe_projection() {
        let root = root("offline-restart");
        let s = ClaudeSnapshotStore::at_root(root.clone()).unwrap();
        let id = slot("123e4567-e89b-42d3-a456-42661417400d");
        register(&s, id.clone(), now());
        s.apply_observation(
            available(
                id,
                1,
                1,
                vec![window(
                    WindowKind::Weekly,
                    12.0,
                    Some(now() + Duration::hours(1)),
                )],
            ),
            now(),
        )
        .unwrap();
        let before = serde_json::to_string(&s.project(now()).unwrap()).unwrap();
        let restarted = ClaudeSnapshotStore::at_root(root).unwrap();
        let after = serde_json::to_string(&restarted.project(now()).unwrap()).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn repeated_sub_tolerance_calls_never_lower_high_water() {
        let s = store("high-water");
        let id = slot("123e4567-e89b-42d3-a456-426614174010");
        let later = now() + Duration::minutes(5);
        register(&s, id, later);
        let high_water = s.load(later).unwrap().last_evaluated_wall_time;
        for offset in [
            Duration::seconds(1),
            Duration::seconds(30),
            Duration::seconds(59),
        ] {
            s.project(later - offset).unwrap();
            assert_eq!(s.load(later).unwrap().last_evaluated_wall_time, high_water);
        }
    }

    #[test]
    fn unpair_is_atomic_and_retains_only_slot_metadata() {
        let s = store("unpair");
        let id = slot("123e4567-e89b-42d3-a456-426614174011");
        register(&s, id.clone(), now());
        s.apply_observation(
            available(
                id.clone(),
                1,
                1,
                vec![window(
                    WindowKind::Weekly,
                    20.0,
                    Some(now() + Duration::hours(1)),
                )],
            ),
            now(),
        )
        .unwrap();
        s.unpair(&id, now()).unwrap();
        let aggregate = s.load(now()).unwrap();
        assert_eq!(aggregate.slots.len(), 1);
        assert_eq!(aggregate.slots[0].binding_state, BindingState::Unbound);
        assert!(aggregate.slots[0].binding_id.is_none());
        assert!(aggregate.slots[0].windows.is_empty());
    }

    #[test]
    fn invalid_structure_and_binding_id_fail_closed_without_overwrite() {
        let s = store("invalid-state");
        let id = slot("123e4567-e89b-42d3-a456-426614174012");
        register(&s, id, now());
        let mut aggregate = s.load(now()).unwrap();
        aggregate.slots[0].binding_id = Some(BindingId("not-a-uuid".into()));
        overwrite_aggregate(&s, &aggregate);
        let before = fs::read(s.root.join(STATE_FILE)).unwrap();
        assert!(matches!(s.project(now()), Err(SnapshotError::InvalidState)));
        assert_eq!(fs::read(s.root.join(STATE_FILE)).unwrap(), before);
    }

    #[test]
    fn duplicate_slot_and_terminal_value_conflict_are_rejected() {
        let s = store("invalid-records");
        let id = slot("123e4567-e89b-42d3-a456-426614174013");
        register(&s, id, now());
        let mut aggregate = s.load(now()).unwrap();
        aggregate.slots.push(aggregate.slots[0].clone());
        assert_eq!(aggregate.validate(), Err(SnapshotError::InvalidState));
        aggregate.slots.pop();
        aggregate.slots[0].windows.push(WindowRecord {
            kind: WindowKind::Weekly,
            used_percent: Some(1.0),
            observed_at: Some(now()),
            received_at: Some(now()),
            reset_at: Some(now() + Duration::hours(1)),
            source: Some(SourceClass::CompletionSse),
            terminal_expired: true,
            last_error_code: None,
        });
        assert_eq!(aggregate.validate(), Err(SnapshotError::InvalidState));
    }

    #[test]
    fn malformed_json_and_precisely_named_orphan_fail_closed_or_cleanup_under_lock() {
        let s = store("malformed-and-orphan");
        let id = slot("123e4567-e89b-42d3-a456-426614174014");
        register(&s, id, now());
        let state = s.root.join(STATE_FILE);
        fs::write(&state, b"{").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&state, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        assert!(matches!(s.project(now()), Err(SnapshotError::InvalidState)));

        let temp = s
            .root
            .join(format!("{TEMP_PREFIX}99999-{}", Uuid::new_v4()));
        let _file = open_private_new(&temp).unwrap();
        drop(_file);
        let _lock = s.lock().unwrap();
        cleanup_orphan_temps(&s.root).unwrap();
        assert!(!temp.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_app_config_and_final_state_component_are_rejected() {
        use std::os::unix::fs::symlink;

        let parent = root("symlinks");
        fs::create_dir_all(&parent).unwrap();
        let config = parent.join("config");
        fs::create_dir(&config).unwrap();
        let linked = parent.join("linked-config");
        symlink(&config, &linked).unwrap();
        assert!(matches!(
            ClaudeSnapshotStore::in_app_config(&linked),
            Err(SnapshotError::InvalidState)
        ));

        let s = ClaudeSnapshotStore::at_root(parent.join("state")).unwrap();
        let state = s.root.join(STATE_FILE);
        symlink("missing-target", &state).unwrap();
        assert!(matches!(s.project(now()), Err(SnapshotError::InvalidState)));
    }

    #[test]
    fn debug_rendering_redacts_forbidden_internal_values() {
        let envelope = available(
            slot("123e4567-e89b-42d3-a456-426614174015"),
            77,
            88,
            vec![window(WindowKind::Weekly, 9.0, None)],
        );
        let debug = format!("{envelope:?}");
        assert!(!debug.contains("77") && !debug.contains("88") && !debug.contains("123e"));
        let binding_debug = format!("{:?}", BindingId::generate());
        assert!(!binding_debug.contains('-'));
    }

    #[test]
    fn read_side_received_at_rollback_persists_terminal_until_newer_observation() {
        let path = root("read-rollback");
        let s = ClaudeSnapshotStore::at_root(path.clone()).unwrap();
        let id = slot("123e4567-e89b-42d3-a456-426614174016");
        register(&s, id.clone(), now());
        s.apply_observation(
            available(
                id.clone(),
                1,
                1,
                vec![window(
                    WindowKind::FiveHour,
                    42.0,
                    Some(now() + Duration::hours(2)),
                )],
            ),
            now(),
        )
        .unwrap();
        let mut aggregate = s.load(now()).unwrap();
        aggregate.slots[0].windows[0].received_at = Some(now() + Duration::seconds(50));
        overwrite_aggregate(&s, &aggregate);

        let read_time = now() - Duration::seconds(20);
        let projected = projection(&s, read_time);
        assert_eq!(projected.five_hour.used_percent, None);
        let terminal = s.load(now()).unwrap();
        assert!(terminal.slots[0].windows[0].terminal_expired);
        assert_eq!(terminal.last_evaluated_wall_time, now());
        let restarted = ClaudeSnapshotStore::at_root(path).unwrap();
        assert_eq!(projection(&restarted, now()).five_hour.used_percent, None);
        assert!(restarted
            .apply_observation(
                available(
                    id.clone(),
                    1,
                    2,
                    vec![window(WindowKind::FiveHour, 1.0, None)]
                ),
                read_time
            )
            .is_err());
        restarted
            .apply_observation(
                available(
                    id,
                    1,
                    2,
                    vec![window(
                        WindowKind::FiveHour,
                        7.0,
                        Some(now() + Duration::hours(3)),
                    )],
                ),
                now() + Duration::minutes(2),
            )
            .unwrap();
        assert_eq!(
            projection(&restarted, now() + Duration::minutes(2))
                .five_hour
                .used_percent,
            Some(7.0)
        );
    }

    #[test]
    fn uuid_variant_is_required_for_slot_and_binding_ids() {
        for invalid in [
            "123e4567-e89b-42d3-0456-426614174000",
            "123e4567-e89b-42d3-c456-426614174000",
            "123e4567-e89b-42d3-e456-426614174000",
        ] {
            assert!(AccountSlotId::parse(invalid).is_err());
            assert!(!BindingId(invalid.into()).is_canonical());
        }
        let generated = Uuid::new_v4().hyphenated().to_string();
        assert!(AccountSlotId::parse(generated.clone()).is_ok());
        assert!(BindingId(generated).is_canonical());
    }

    #[cfg(unix)]
    #[test]
    fn pinned_root_replacement_fails_closed_without_writing_replacement() {
        let path = root("pinned-root");
        let s = ClaudeSnapshotStore::at_root(path.clone()).unwrap();
        let id = slot("123e4567-e89b-42d3-a456-426614174017");
        register(&s, id, now());
        let displaced = path.with_extension("displaced");
        fs::rename(&path, &displaced).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(matches!(s.project(now()), Err(SnapshotError::InvalidState)));
        assert!(!path.join(STATE_FILE).exists());
    }

    #[test]
    fn temp_path_substitution_after_fsync_fails_closed() {
        let s = std::sync::Arc::new(store("temp-substitution"));
        let id = slot("123e4567-e89b-42d3-a456-426614174018");
        register(&s, id, now());
        let aggregate = s.load(now()).unwrap();
        let reached = std::sync::Arc::new(std::sync::Barrier::new(2));
        let resume = std::sync::Arc::new(std::sync::Barrier::new(2));
        let temp_path = std::sync::Arc::new(std::sync::Mutex::new(None));
        *WRITE_TEST_HOOK
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap() = Some(WriteTestHook {
            root: s.root.clone(),
            reached: reached.clone(),
            resume: resume.clone(),
            temp_path: temp_path.clone(),
        });
        let writer = {
            let s = s.clone();
            std::thread::spawn(move || s.write_aggregate(&aggregate))
        };
        reached.wait();
        let temp = temp_path.lock().unwrap().clone().unwrap();
        let substitute = temp.with_extension("substitute");
        fs::rename(&temp, &substitute).unwrap();
        fs::write(&temp, b"substituted").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&temp, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        resume.wait();
        assert!(writer.join().unwrap().is_err());
        *WRITE_TEST_HOOK.get().unwrap().lock().unwrap() = None;
    }
}

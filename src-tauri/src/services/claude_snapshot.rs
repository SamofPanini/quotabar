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

const SCHEMA_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_SLOTS: usize = 4;
const MAX_ALIAS_BYTES: usize = 48;
const FRESH_FOR: Duration = Duration::minutes(15);
const ROLLBACK_TOLERANCE: Duration = Duration::seconds(60);
const STATE_FILE: &str = "current-state.json";
const LOCK_FILE: &str = ".current-state.lock";
const TEMP_PREFIX: &str = ".current-state.tmp-";

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
        let bytes = value.as_bytes();
        let valid = bytes.len() == 36
            && [8, 13, 18, 23]
                .into_iter()
                .all(|index| bytes[index] == b'-')
            && bytes.iter().enumerate().all(|(index, byte)| {
                [8, 13, 18, 23].contains(&index)
                    || byte.is_ascii_digit()
                    || (b'a'..=b'f').contains(byte)
            })
            && bytes[14] == b'4';
        valid
            .then_some(Self(value))
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

#[derive(Clone, Debug)]
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
    binding_id: String,
    binding_state: BindingState,
    binding_epoch: u64,
    next_sequence: u64,
    windows: Vec<WindowRecord>,
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
        for slot in &self.slots {
            if !safe_alias(&slot.alias)
                || slot.binding_epoch == 0
                || slot.next_sequence == 0
                || slot.windows.len() > 2
                || slot.windows.iter().enumerate().any(|(index, window)| {
                    (index == 1 && slot.windows[0].kind != WindowKind::FiveHour)
                        || (index == 1 && window.kind != WindowKind::Weekly)
                        || window.used_percent.is_some_and(|value| {
                            !value.is_finite() || !(0.0..=100.0).contains(&value)
                        })
                })
            {
                return Err(SnapshotError::InvalidState);
            }
        }
        Ok(())
    }
}

pub(crate) struct ClaudeSnapshotStore {
    root: PathBuf,
}

impl ClaudeSnapshotStore {
    pub(crate) fn in_app_config(config_dir: &Path) -> Result<Self, SnapshotError> {
        let root = config_dir.join("claude-current-state");
        Self::at_root(root)
    }

    pub(crate) fn at_root(root: PathBuf) -> Result<Self, SnapshotError> {
        ensure_root(&root)?;
        cleanup_orphan_temps(&root)?;
        Ok(Self { root })
    }

    pub(crate) fn register_slot(
        &self,
        slot_id: AccountSlotId,
        alias: String,
        plan: Option<PlanMetadata>,
        binding_id: String,
        now: DateTime<Utc>,
    ) -> Result<(), SnapshotError> {
        if !safe_alias(&alias) || binding_id.is_empty() || binding_id.len() > 128 {
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
                binding_id,
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
            slot.binding_epoch = slot
                .binding_epoch
                .checked_add(1)
                .ok_or(SnapshotError::Rejected)?;
            slot.next_sequence = 1;
            slot.binding_state = BindingState::Unverified;
            for window in &mut slot.windows {
                window.used_percent = None;
                window.terminal_expired = true;
                window.last_error_code = Some(SafeErrorCode::Unavailable);
            }
            Ok(())
        })
    }

    pub(crate) fn project(
        &self,
        now: DateTime<Utc>,
    ) -> Result<ClaudeCurrentSnapshotsDto, SnapshotError> {
        let _lock = LockGuard::acquire(&self.root)?;
        let mut aggregate = self.load(now)?;
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
        let _lock = LockGuard::acquire(&self.root)?;
        let mut aggregate = self.load(now)?;
        project_expiry(&mut aggregate, now);
        change(&mut aggregate)?;
        aggregate.last_evaluated_wall_time = now;
        aggregate.validate()?;
        self.write_aggregate(&aggregate)
    }

    fn load(&self, now: DateTime<Utc>) -> Result<Aggregate, SnapshotError> {
        let path = self.root.join(STATE_FILE);
        if fs::symlink_metadata(&path).is_err() {
            return Ok(Aggregate::empty(now));
        }
        validate_regular_owned(&path, 0o600)?;
        let metadata = fs::metadata(&path).map_err(|_| SnapshotError::Io)?;
        if metadata.len() > MAX_FILE_BYTES {
            return Err(SnapshotError::InvalidState);
        }
        let mut content = String::with_capacity(metadata.len() as usize);
        open_private_existing(&path)?
            .read_to_string(&mut content)
            .map_err(|_| SnapshotError::Io)?;
        let aggregate: Aggregate =
            serde_json::from_str(&content).map_err(|_| SnapshotError::InvalidState)?;
        aggregate.validate()?;
        Ok(aggregate)
    }

    fn write_aggregate(&self, aggregate: &Aggregate) -> Result<(), SnapshotError> {
        let bytes = serde_json::to_vec(aggregate).map_err(|_| SnapshotError::InvalidState)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(SnapshotError::InvalidState);
        }
        let temp = self
            .root
            .join(format!("{TEMP_PREFIX}{}", std::process::id()));
        let mut file = open_private_new(&temp)?;
        file.write_all(&bytes).map_err(|_| SnapshotError::Io)?;
        file.sync_all().map_err(|_| SnapshotError::Io)?;
        drop(file);
        fs::rename(&temp, self.root.join(STATE_FILE)).map_err(|_| SnapshotError::Io)?;
        sync_directory(&self.root)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotError {
    InvalidInput,
    InvalidState,
    Rejected,
    Io,
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
        if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
            validate_regular_owned(&entry.path(), 0o600)?;
            fs::remove_file(entry.path()).map_err(|_| SnapshotError::Io)?;
        }
    }
    Ok(())
}

struct LockGuard {
    file: File,
}
impl LockGuard {
    fn acquire(root: &Path) -> Result<Self, SnapshotError> {
        let path = root.join(LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path).map_err(|_| SnapshotError::Io)?;
        #[cfg(unix)]
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .map_err(|_| SnapshotError::Io)?;
        validate_regular_owned(&path, 0o600)?;
        #[cfg(unix)]
        {
            if unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) }
                != 0
            {
                return Err(SnapshotError::Io);
            }
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
            .register_slot(
                id,
                "safe".into(),
                Some(PlanMetadata::Paid),
                "internal-binding".into(),
                now,
            )
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
                && !json.contains("internal-binding")
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
}

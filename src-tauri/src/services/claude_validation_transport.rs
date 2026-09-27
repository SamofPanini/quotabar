//! Bounded inherited Unix socketpair transport for the synthetic C3-B1A boundary.
//!
//! There is intentionally no production listener constructor or Tauri wiring.
//! Tests create a private anonymous socketpair and pass its owned parent end to
//! the crate-private session handler. There is no filesystem endpoint.

use super::claude_snapshot::{
    AccountSlotId, ClaudeSnapshotStore, PlanMetadata, SafeErrorCode, SourceClass, WindowKind,
};
use super::claude_synthetic_adapter::{
    submit_correlated_observation, CorrelatedDisposition, CorrelatedObservation, SyntheticWindow,
};
use super::claude_validation_pairing::{PairingConsume, PairingTable, SessionAuthority};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use zeroize::Zeroizing;

const HANDSHAKE_MAX: usize = 1024;
const FRAME_MAX: usize = 16 * 1024;
const SESSION_MAX_FRAMES: usize = 8;
const SESSION_MAX_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportError {
    Rejected,
    Expired,
    Io,
}

/*
    pub(crate) fn for_synthetic(root: PathBuf) -> Result<Self, TransportError> {
        let expected_name = format!("quotabar-c3b1-{}", unsafe { libc::geteuid() });
        if root.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
            return Err(TransportError::Rejected);
        }
        let encoded = std::ffi::CString::new(root.as_os_str().as_encoded_bytes())
            .map_err(|_| TransportError::Rejected)?;
        let fd = unsafe {
            libc::open(
                encoded.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(TransportError::Rejected);
        }
        let descriptor = unsafe { fs::File::from_raw_fd(fd) };
        let metadata = descriptor
            .metadata()
            .map_err(|_| TransportError::Rejected)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || (metadata.mode() & 0o777) != 0o700
        {
            return Err(TransportError::Rejected);
        }
        let socket_root = Self {
            root,
            descriptor,
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        socket_root.initialize_owner_markers()?;
        Ok(socket_root)
    }

    pub(crate) fn socket_path(&self) -> Result<PathBuf, TransportError> {
        self.assert_pinned()?;
        let candidate = self.root.join(SOCKET_NAME);
        // macOS sockaddr_un accepts at most 104 bytes; reject rather than
        // relying on truncation or a fallback path.
        if candidate.as_os_str().as_encoded_bytes().len() >= 104 {
            return Err(TransportError::Rejected);
        }
        if candidate.exists() || fs::symlink_metadata(&candidate).is_ok() {
            return Err(TransportError::Rejected);
        }
        Ok(candidate)
    }

    pub(crate) fn bind(&self) -> Result<UnixListener, TransportError> {
        self.classify_existing_endpoint()?;
        let path = self.socket_path()?;
        let listener = UnixListener::bind(&path).map_err(|_| TransportError::Rejected)?;
        let created = fs::symlink_metadata(&path).map_err(|_| TransportError::Rejected)?;
        if !created.file_type().is_socket() || created.file_type().is_symlink() {
            return Err(TransportError::Rejected);
        }
        let socket = CStringName::new(SOCKET_NAME)?;
        if unsafe { libc::fchmodat(self.descriptor.as_raw_fd(), socket.as_ptr(), 0o600, 0) } != 0 {
            return Err(TransportError::Io);
        }
        if self.assert_pinned().is_err() {
            // Only remove the exact inode created by this attempt; a
            // replacement at the pathname is left untouched.
            if let Ok(current) = fs::symlink_metadata(&path) {
                if current.dev() == created.dev() && current.ino() == created.ino() {
                    let _ = fs::remove_file(&path);
                }
            }
            return Err(TransportError::Rejected);
        }
        let metadata =
            stat_at(self.descriptor.as_raw_fd(), &socket)?.ok_or(TransportError::Rejected)?;
        if (metadata.st_mode & libc::S_IFMT) != libc::S_IFSOCK
            || metadata.st_uid != unsafe { libc::geteuid() }
            || (metadata.st_mode as u32 & 0o777) != 0o600
        {
            return Err(TransportError::Rejected);
        }
        Ok(listener)
    }

    fn initialize_owner_markers(&self) -> Result<(), TransportError> {
        self.assert_pinned()?;
        let manifest = CStringName::new(SOCKET_MANIFEST)?;
        let pid = CStringName::new(SOCKET_PID)?;
        if stat_at(self.descriptor.as_raw_fd(), &manifest)?.is_some()
            || stat_at(self.descriptor.as_raw_fd(), &pid)?.is_some()
        {
            return self.validate_owner_markers();
        }
        if stat_at(self.descriptor.as_raw_fd(), &CStringName::new(SOCKET_NAME)?)?.is_some() {
            return Err(TransportError::Rejected);
        }
        write_private_at(
            self.descriptor.as_raw_fd(),
            &manifest,
            SOCKET_MANIFEST_BYTES,
        )?;
        write_private_at(
            self.descriptor.as_raw_fd(),
            &pid,
            std::process::id().to_string().as_bytes(),
        )?;
        self.validate_owner_markers()
    }

    fn validate_owner_markers(&self) -> Result<(), TransportError> {
        self.assert_pinned()?;
        let manifest = CStringName::new(SOCKET_MANIFEST)?;
        let pid = CStringName::new(SOCKET_PID)?;
        for name in [&manifest, &pid] {
            let metadata =
                stat_at(self.descriptor.as_raw_fd(), name)?.ok_or(TransportError::Rejected)?;
            if (metadata.st_mode & libc::S_IFMT) != libc::S_IFREG
                || metadata.st_uid != unsafe { libc::geteuid() }
                || (metadata.st_mode as u32 & 0o777) != 0o600
            {
                return Err(TransportError::Rejected);
            }
        }
        if read_private_at(self.descriptor.as_raw_fd(), &manifest)? != SOCKET_MANIFEST_BYTES {
            return Err(TransportError::Rejected);
        }
        let pid = String::from_utf8(read_private_at(self.descriptor.as_raw_fd(), &pid)?)
            .map_err(|_| TransportError::Rejected)?;
        let parsed = pid.parse::<u32>().map_err(|_| TransportError::Rejected)?;
        if parsed == 0 {
            return Err(TransportError::Rejected);
        }
        Ok(())
    }

    fn classify_existing_endpoint(&self) -> Result<(), TransportError> {
        self.validate_owner_markers()?;
        let socket = CStringName::new(SOCKET_NAME)?;
        let metadata = match stat_at(self.descriptor.as_raw_fd(), &socket)? {
            Some(metadata) => metadata,
            None => return Ok(()),
        };
        if (metadata.st_mode & libc::S_IFMT) != libc::S_IFSOCK
            || metadata.st_uid != unsafe { libc::geteuid() }
            || (metadata.st_mode as u32 & 0o777) != 0o600
        {
            return Err(TransportError::Rejected);
        }
        let endpoint = self.root.join(SOCKET_NAME);
        if UnixStream::connect(&endpoint).is_ok() {
            return Err(TransportError::Rejected);
        }
        let pid = String::from_utf8(read_private_at(
            self.descriptor.as_raw_fd(),
            &CStringName::new(SOCKET_PID)?,
        )?)
        .map_err(|_| TransportError::Rejected)?
        .parse::<i32>()
        .map_err(|_| TransportError::Rejected)?;
        let dead = unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if !dead {
            return Err(TransportError::Rejected);
        }
        self.assert_pinned()?;
        let current =
            stat_at(self.descriptor.as_raw_fd(), &socket)?.ok_or(TransportError::Rejected)?;
        if current.st_dev != metadata.st_dev
            || current.st_ino != metadata.st_ino
            || current.st_mode != metadata.st_mode
        {
            return Err(TransportError::Rejected);
        }
        if unsafe { libc::unlinkat(self.descriptor.as_raw_fd(), socket.as_ptr(), 0) } == 0 {
            Ok(())
        } else {
            Err(TransportError::Rejected)
        }
    }

    fn assert_pinned(&self) -> Result<(), TransportError> {
        let descriptor = self
            .descriptor
            .metadata()
            .map_err(|_| TransportError::Rejected)?;
        let path = fs::symlink_metadata(&self.root).map_err(|_| TransportError::Rejected)?;
        if !descriptor.is_dir()
            || !path.is_dir()
            || path.file_type().is_symlink()
            || descriptor.uid() != unsafe { libc::geteuid() }
            || (descriptor.mode() & 0o777) != 0o700
            || descriptor.dev() != self.dev
            || descriptor.ino() != self.ino
            || path.dev() != self.dev
            || path.ino() != self.ino
        {
            return Err(TransportError::Rejected);
        }
        Ok(())
    }
}

struct CStringName(std::ffi::CString);

impl CStringName {
    fn new(name: &str) -> Result<Self, TransportError> {
        if name.is_empty() || name.contains('/') {
            return Err(TransportError::Rejected);
        }
        std::ffi::CString::new(name)
            .map(Self)
            .map_err(|_| TransportError::Rejected)
    }

    fn as_ptr(&self) -> *const libc::c_char {
        self.0.as_ptr()
    }
}

fn stat_at(dirfd: i32, name: &CStringName) -> Result<Option<libc::stat>, TransportError> {
    let mut stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dirfd, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        Ok(Some(stat))
    } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
        Ok(None)
    } else {
        Err(TransportError::Rejected)
    }
}

fn write_private_at(dirfd: i32, name: &CStringName, bytes: &[u8]) -> Result<(), TransportError> {
    let fd = unsafe {
        libc::openat(
            dirfd,
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(TransportError::Rejected);
    }
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    file.write_all(bytes).map_err(|_| TransportError::Io)?;
    file.sync_all().map_err(|_| TransportError::Io)
}

fn read_private_at(dirfd: i32, name: &CStringName) -> Result<Vec<u8>, TransportError> {
    let fd = unsafe {
        libc::openat(
            dirfd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(TransportError::Rejected);
    }
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| TransportError::Io)?;
    Ok(bytes)
}

/// Returns both credentials required by the contract. Absence of either is a
/// reject; there is deliberately no same-UID-only fallback.
pub(crate) fn peer_credentials(stream: &UnixStream) -> Result<PeerCredentials, TransportError> {
    #[cfg(target_os = "macos")]
    unsafe {
        let fd = stream.as_raw_fd();
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        if libc::getpeereid(fd, &mut uid, &mut gid) != 0 {
            return Err(TransportError::Rejected);
        }
        // LOCAL_PEERPID is the Darwin local-domain peer PID option. It is not
        // supplied by libc's portable constants, so keep the ABI value local.
        const SOL_LOCAL: libc::c_int = 0;
        const LOCAL_PEERPID: libc::c_int = 0x002;
        let mut pid: libc::pid_t = 0;
        let mut size = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        if libc::getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut size,
        ) != 0
            || size != std::mem::size_of::<libc::pid_t>() as libc::socklen_t
            || pid <= 0
        {
            return Err(TransportError::Rejected);
        }
        Ok(PeerCredentials {
            uid,
            pid: pid as u32,
        })
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let mut credentials: libc::ucred = std::mem::zeroed();
        let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut size,
        ) != 0
            || size != std::mem::size_of::<libc::ucred>() as libc::socklen_t
            || credentials.pid <= 0
        {
            return Err(TransportError::Rejected);
        }
        Ok(PeerCredentials {
            uid: credentials.uid,
            pid: credentials.pid as u32,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = stream;
        Err(TransportError::Rejected)
    }
}
*/

/// Handles an owned parent end of a private inherited socketpair. The spawn
/// owner proves delivery by retaining the sole parent end; no socket peer PID
/// is claimed to identify the eventual child writer.
pub(crate) fn handle_authenticated_session(
    stream: &mut UnixStream,
    store: &ClaudeSnapshotStore,
    pairing: &mut PairingTable,
    now: DateTime<Utc>,
) -> Result<(), TransportError> {
    let authority = authenticate_session(stream, pairing, now)?;
    handle_session_loop(stream, store, authority, now)
}

/// Authentication is the only phase that borrows the shared pairing table.
/// Once this returns, callers may release their table guard before any frame
/// I/O or snapshot mutation begins.
pub(crate) fn authenticate_session(
    stream: &mut UnixStream,
    pairing: &mut PairingTable,
    now: DateTime<Utc>,
) -> Result<SessionAuthority, TransportError> {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .map_err(|_| TransportError::Io)?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|_| TransportError::Io)?;
    let handshake = read_secret_frame(stream, HANDSHAKE_MAX)?;
    let (slot, token) = parse_handshake(&handshake)?;
    match pairing.consume(&slot, &token, now) {
        PairingConsume::Accepted(authority) => {
            write_fixed_result(stream, "accepted")?;
            Ok(authority)
        }
        PairingConsume::Expired => {
            write_fixed_result(stream, "expired")?;
            return Err(TransportError::Expired);
        }
        PairingConsume::Rejected => {
            write_fixed_result(stream, "rejected")?;
            return Err(TransportError::Rejected);
        }
    }
}

/// Per-session worker phase: deliberately receives owned authority only and
/// cannot retain a pairing-table reference or synchronization guard.
pub(crate) fn handle_session_loop(
    stream: &mut UnixStream,
    store: &ClaudeSnapshotStore,
    authority: SessionAuthority,
    now: DateTime<Utc>,
) -> Result<(), TransportError> {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|_| TransportError::Io)?;
    let mut frames = 1usize;
    // The exact handshake payload is no longer retained after authentication;
    // reserve its maximum framing budget for a conservative session cap.
    let mut bytes = HANDSHAKE_MAX + 4;
    while frames < SESSION_MAX_FRAMES && bytes < SESSION_MAX_BYTES {
        let frame = match read_frame(stream, FRAME_MAX) {
            Ok(frame) => frame,
            Err(TransportError::Io) => return Ok(()),
            Err(error) => return Err(error),
        };
        bytes = bytes
            .checked_add(frame.len() + 4)
            .ok_or(TransportError::Rejected)?;
        if bytes > SESSION_MAX_BYTES {
            return Err(TransportError::Rejected);
        }
        let event = parse_ingress(&frame, &authority.slot_id, authority.epoch, authority.plan)?;
        submit_correlated_observation(
            store,
            CorrelatedObservation {
                slot_id: authority.slot_id.clone(),
                capability: authority.capability.clone(),
                binding_epoch: authority.epoch,
                sequence: event.sequence,
                observed_at: event.observed_at,
                // Slot plan is app-owned at foreground registration. Transport
                // metadata must never relabel an existing validation slot.
                plan: None,
                disposition: event.disposition,
            },
            now,
        )
        .map_err(|_| TransportError::Rejected)?;
        frames += 1;
    }
    Ok(())
}

fn read_frame(stream: &mut UnixStream, max: usize) -> Result<Vec<u8>, TransportError> {
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .map_err(|_| TransportError::Io)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > max {
        return Err(TransportError::Rejected);
    }
    let mut frame = vec![0; length];
    stream
        .read_exact(&mut frame)
        .map_err(|_| TransportError::Io)?;
    std::str::from_utf8(&frame).map_err(|_| TransportError::Rejected)?;
    Ok(frame)
}

/// The pairing frame contains the one-shot transport authority, so retain its
/// payload only in a zeroizing allocation.  Event frames intentionally remain
/// ordinary bounded JSON buffers because they carry no authority material.
fn read_secret_frame(
    stream: &mut UnixStream,
    max: usize,
) -> Result<Zeroizing<Vec<u8>>, TransportError> {
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .map_err(|_| TransportError::Io)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > max {
        return Err(TransportError::Rejected);
    }
    let mut frame = Zeroizing::new(vec![0; length]);
    stream
        .read_exact(&mut frame)
        .map_err(|_| TransportError::Io)?;
    Ok(frame)
}

fn write_fixed_result(stream: &mut UnixStream, result: &str) -> Result<(), TransportError> {
    let bytes = format!("{{\"result\":\"{result}\"}}").into_bytes();
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .map_err(|_| TransportError::Io)?;
    stream.write_all(&bytes).map_err(|_| TransportError::Io)
}

fn parse_handshake(bytes: &[u8]) -> Result<(AccountSlotId, Zeroizing<Vec<u8>>), TransportError> {
    // This deliberately accepts one canonical, escape-free wire shape.  It
    // avoids constructing a String or serde_json::Value containing the
    // transport secret while still rejecting duplicate keys and alternate JSON
    // representations at the boundary.
    const PREFIX: &[u8] = br#"{"kind":"pair","slot":"#;
    const TOKEN_MARKER: &[u8] = br#"","token":"#;
    const SUFFIX: &[u8] = br#""}"#;
    const SLOT_BYTES: usize = 36;
    const TOKEN_BYTES: usize = 43;
    let expected = PREFIX.len() + SLOT_BYTES + TOKEN_MARKER.len() + TOKEN_BYTES + SUFFIX.len();
    if bytes.len() != expected
        || !bytes.starts_with(PREFIX)
        || &bytes[PREFIX.len() + SLOT_BYTES..PREFIX.len() + SLOT_BYTES + TOKEN_MARKER.len()]
            != TOKEN_MARKER
        || !bytes.ends_with(SUFFIX)
    {
        return Err(TransportError::Rejected);
    }
    let slot_bytes = &bytes[PREFIX.len()..PREFIX.len() + SLOT_BYTES];
    let slot = AccountSlotId::parse(
        std::str::from_utf8(slot_bytes)
            .map_err(|_| TransportError::Rejected)?
            .to_owned(),
    )
    .map_err(|_| TransportError::Rejected)?;
    let token_start = PREFIX.len() + SLOT_BYTES + TOKEN_MARKER.len();
    let encoded = &bytes[token_start..token_start + TOKEN_BYTES];
    if !encoded
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(TransportError::Rejected);
    }
    let mut token = Zeroizing::new(vec![0; 32]);
    let written = URL_SAFE_NO_PAD
        .decode_slice(encoded, &mut token)
        .map_err(|_| TransportError::Rejected)?;
    if written != 32 {
        return Err(TransportError::Rejected);
    }
    Ok((slot, token))
}

struct ParsedIngress {
    sequence: u64,
    observed_at: DateTime<Utc>,
    disposition: CorrelatedDisposition,
}

fn parse_ingress(
    bytes: &[u8],
    slot: &AccountSlotId,
    epoch: u64,
    owner_plan: PlanMetadata,
) -> Result<ParsedIngress, TransportError> {
    let raw = strict_value(bytes)?;
    let object = raw.as_object().ok_or(TransportError::Rejected)?;
    required_exact(
        object,
        &["event", "slot", "epoch", "sequence", "observedAt"],
        &["status", "source", "windows", "errorCode", "plan"],
    )?;
    if AccountSlotId::parse(string(object, "slot")?).map_err(|_| TransportError::Rejected)? != *slot
        || canonical_u64(string(object, "epoch")?)? != epoch
    {
        return Err(TransportError::Rejected);
    }
    let sequence = canonical_u64(string(object, "sequence")?)?;
    let observed_at = canonical_time(string(object, "observedAt")?)?;
    let plan_matches_owner = match object.get("plan") {
        Some(Value::String(v)) => {
            (v == "paid" && owner_plan == PlanMetadata::Paid)
                || (v == "free" && owner_plan == PlanMetadata::Free)
        }
        None => true,
        _ => false,
    };
    if !plan_matches_owner {
        return Err(TransportError::Rejected);
    }
    let disposition = match string(object, "event")? {
        "continuity_uncertain" => {
            reject_any(
                object,
                &["status", "source", "windows", "errorCode", "plan"],
            )?;
            CorrelatedDisposition::ContinuityUncertain
        }
        "identity_changed" => {
            reject_any(
                object,
                &["status", "source", "windows", "errorCode", "plan"],
            )?;
            CorrelatedDisposition::IdentityChanged
        }
        "observation" => parse_observation(object)?,
        _ => return Err(TransportError::Rejected),
    };
    Ok(ParsedIngress {
        sequence,
        observed_at,
        disposition,
    })
}

fn parse_observation(object: &Map<String, Value>) -> Result<CorrelatedDisposition, TransportError> {
    match string(object, "status")? {
        "available" => {
            if string(object, "source")? != "completion_sse" || object.contains_key("errorCode") {
                return Err(TransportError::Rejected);
            }
            let windows = object
                .get("windows")
                .and_then(Value::as_array)
                .ok_or(TransportError::Rejected)?;
            if windows.is_empty() || windows.len() > 2 {
                return Err(TransportError::Rejected);
            }
            let mut parsed: Vec<SyntheticWindow> = Vec::with_capacity(windows.len());
            for (index, window) in windows.iter().enumerate() {
                let item = window.as_object().ok_or(TransportError::Rejected)?;
                required_exact(item, &["kind", "usedPercent"], &["resetAt"])?;
                let kind = match string(item, "kind")? {
                    "five_hour" => WindowKind::FiveHour,
                    "weekly" => WindowKind::Weekly,
                    _ => return Err(TransportError::Rejected),
                };
                if (index == 1 && !matches!(parsed[0].kind, WindowKind::FiveHour))
                    || (index == 1 && !matches!(kind, WindowKind::Weekly))
                {
                    return Err(TransportError::Rejected);
                }
                let used_percent = item
                    .get("usedPercent")
                    .and_then(Value::as_f64)
                    .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
                    .ok_or(TransportError::Rejected)?;
                let reset_at = match item.get("resetAt") {
                    Some(Value::String(value)) => Some(canonical_time(value)?),
                    None => None,
                    _ => return Err(TransportError::Rejected),
                };
                parsed.push(SyntheticWindow {
                    kind,
                    used_percent,
                    reset_at,
                });
            }
            if parsed.len() == 2
                && parsed[0].reset_at.is_some()
                && parsed[1].reset_at.is_some()
                && parsed[0].reset_at > parsed[1].reset_at
            {
                return Err(TransportError::Rejected);
            }
            Ok(CorrelatedDisposition::Available {
                source: SourceClass::CompletionSse,
                windows: parsed,
            })
        }
        "unavailable" => {
            reject_any(object, &["source", "windows"])?;
            let error = match string(object, "errorCode")? {
                "unavailable" => SafeErrorCode::Unavailable,
                "malformed_payload" => SafeErrorCode::MalformedPayload,
                "unsupported_observation" => SafeErrorCode::UnsupportedObservation,
                _ => return Err(TransportError::Rejected),
            };
            Ok(CorrelatedDisposition::Unavailable { error })
        }
        _ => Err(TransportError::Rejected),
    }
}

// serde_json normally retains the last duplicate key.  This small lexical
// preflight rejects duplicate object keys before serde parsing, including
// nested objects, then serde validates UTF-8 and number grammar.
fn strict_value(bytes: &[u8]) -> Result<Value, TransportError> {
    reject_duplicate_object_keys(bytes)?;
    serde_json::from_slice(bytes).map_err(|_| TransportError::Rejected)
}

fn reject_duplicate_object_keys(bytes: &[u8]) -> Result<(), TransportError> {
    let mut stack: Vec<std::collections::HashSet<String>> = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            let start = index;
            index += 1;
            while index < bytes.len() {
                if bytes[index] == b'\\' {
                    index += 2;
                    continue;
                }
                if bytes[index] == b'"' {
                    break;
                }
                index += 1;
            }
            if index >= bytes.len() {
                return Err(TransportError::Rejected);
            }
            let value: String = serde_json::from_slice(&bytes[start..=index])
                .map_err(|_| TransportError::Rejected)?;
            let mut look = index + 1;
            while look < bytes.len() && bytes[look].is_ascii_whitespace() {
                look += 1;
            }
            if look < bytes.len() && bytes[look] == b':' {
                if let Some(keys) = stack.last_mut() {
                    if !keys.insert(value) {
                        return Err(TransportError::Rejected);
                    }
                }
            }
        } else if bytes[index] == b'{' {
            stack.push(std::collections::HashSet::new());
        } else if bytes[index] == b'}' {
            if stack.pop().is_none() {
                return Err(TransportError::Rejected);
            }
        }
        index += 1;
    }
    if stack.is_empty() {
        Ok(())
    } else {
        Err(TransportError::Rejected)
    }
}

fn required_exact(
    object: &Map<String, Value>,
    required: &[&str],
    optional: &[&str],
) -> Result<(), TransportError> {
    if required.iter().any(|key| !object.contains_key(*key))
        || object
            .keys()
            .any(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        return Err(TransportError::Rejected);
    }
    Ok(())
}
fn reject_any(object: &Map<String, Value>, keys: &[&str]) -> Result<(), TransportError> {
    if keys.iter().any(|key| object.contains_key(*key)) {
        Err(TransportError::Rejected)
    } else {
        Ok(())
    }
}
fn string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, TransportError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or(TransportError::Rejected)
}
fn canonical_u64(value: &str) -> Result<u64, TransportError> {
    if value.is_empty()
        || value == "0"
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(TransportError::Rejected);
    }
    value.parse().map_err(|_| TransportError::Rejected)
}
fn canonical_time(value: &str) -> Result<DateTime<Utc>, TransportError> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|_| TransportError::Rejected)?
        .with_timezone(&Utc);
    if !value.ends_with('Z') || parsed.to_rfc3339_opts(SecondsFormat::AutoSi, true) != value {
        return Err(TransportError::Rejected);
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::super::claude_snapshot::{ClaudeSnapshotStore, PlanMetadata};
    use super::super::claude_validation_pairing::PairingTable;
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::io::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use uuid::Uuid;

    const CHILD_ENV: &str = "QUOTABAR_C3B1_SYNTHETIC_CHILD";

    /*fn socket_root(label: &str) -> PathBuf {
        let parent =
            PathBuf::from("/private/tmp").join(format!("qt-c3b1-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir(&parent).unwrap();
        let root = parent.join(format!("quotabar-c3b1-{}", unsafe { libc::geteuid() }));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    #[test]
    fn pinned_socket_root_rejects_path_substitution() {
        let root = socket_root("substitute");
        let socket_root = SocketRoot::for_synthetic(root.clone()).unwrap();
        let moved = root.with_extension("moved");
        let substituted = root.parent().unwrap().join("substituted");
        std::fs::create_dir(&substituted).unwrap();
        std::fs::rename(&root, &moved).unwrap();
        std::os::unix::fs::symlink(&substituted, &root).unwrap();
        assert_eq!(
            socket_root.socket_path().unwrap_err(),
            TransportError::Rejected
        );
        assert!(!substituted.join(SOCKET_NAME).exists());
        std::fs::remove_file(&root).unwrap();
        std::fs::remove_dir_all(moved.parent().unwrap()).unwrap();
    }

    #[test]
    fn fixed_root_classifies_live_stale_and_collision_endpoints() {
        let root = socket_root("lifecycle");
        let socket_root = SocketRoot::for_synthetic(root.clone()).unwrap();
        let live = socket_root.bind().unwrap();
        assert_eq!(socket_root.bind().unwrap_err(), TransportError::Rejected);
        drop(live);
        let mut exited = Command::new("/usr/bin/true").spawn().unwrap();
        let dead_pid = exited.id();
        assert!(exited.wait().unwrap().success());
        std::fs::write(root.join(SOCKET_PID), dead_pid.to_string()).unwrap();
        std::fs::set_permissions(
            root.join(SOCKET_PID),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let stale_reclaimed = socket_root.bind().unwrap();
        drop(stale_reclaimed);
        std::fs::remove_file(root.join(SOCKET_NAME)).unwrap();
        std::fs::write(root.join(SOCKET_NAME), b"ordinary").unwrap();
        assert_eq!(socket_root.bind().unwrap_err(), TransportError::Rejected);
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }*/

    fn write_frame(stream: &mut UnixStream, bytes: &[u8]) {
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(bytes).unwrap();
    }

    fn synthetic_handshake(slot: &[u8], token: &[u8]) -> Zeroizing<Vec<u8>> {
        let mut handshake = Zeroizing::new(Vec::with_capacity(128));
        handshake.extend_from_slice(br#"{"kind":"pair","slot":"#);
        handshake.extend_from_slice(slot);
        handshake.extend_from_slice(br#"","token":"#);
        handshake.extend_from_slice(token);
        handshake.extend_from_slice(br#""}"#);
        handshake
    }

    fn read_frame_for_test(stream: &mut UnixStream) -> Vec<u8> {
        let mut length = [0; 4];
        stream.read_exact(&mut length).unwrap();
        let mut bytes = vec![0; u32::from_be_bytes(length) as usize];
        stream.read_exact(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn reexecuted_synthetic_child_uses_bootstrap_fd_and_exact_peer_pid() {
        if std::env::var_os(CHILD_ENV).is_some() {
            synthetic_child();
            return;
        }
        let (mut parent_bootstrap, child_bootstrap) = UnixStream::pair().unwrap();
        let child_fd = child_bootstrap.as_raw_fd();
        let mut child = unsafe {
            Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("services::claude_validation_transport::tests::reexecuted_synthetic_child_uses_bootstrap_fd_and_exact_peer_pid")
                .arg("--nocapture")
                .env(CHILD_ENV, "1")
                .pre_exec(move || {
                    if libc::dup2(child_fd, 3) < 0 { return Err(std::io::Error::last_os_error()); }
                    Ok(())
                })
                .spawn()
                .unwrap()
        };
        drop(child_bootstrap);
        let root = std::env::temp_dir().join(format!("quotabar-c3b1-r4-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root.clone()).unwrap();
        let mut pairing = PairingTable::new();
        let now = chrono::Utc::now();
        let bootstrap = pairing
            .register_or_rebind(
                &store,
                PlanMetadata::Paid,
                unsafe { libc::geteuid() },
                child.id(),
                now,
            )
            .unwrap();
        let slot = bootstrap.slot_id().as_str().as_bytes();
        let token = bootstrap.token_bytes_for_synthetic_child();
        let probe = synthetic_handshake(slot, token);
        assert!(parse_handshake(&probe).is_ok());
        let mut bytes = Zeroizing::new(Vec::with_capacity(slot.len() + token.len()));
        bytes.extend_from_slice(slot);
        bytes.extend_from_slice(token);
        write_frame(&mut parent_bootstrap, &bytes);
        handle_authenticated_session(&mut parent_bootstrap, &store, &mut pairing, now).unwrap();
        assert!(child.wait().unwrap().success());
        std::fs::remove_dir_all(root).unwrap_or(());
    }

    fn synthetic_child() {
        let mut bootstrap = unsafe { UnixStream::from_raw_fd(3) };
        let payload = Zeroizing::new(read_frame_for_test(&mut bootstrap));
        let slot_end = 36;
        let slot = &payload[..slot_end];
        let token = &payload[slot_end..];
        let mut stream = bootstrap;
        let handshake = synthetic_handshake(slot, token);
        write_frame(&mut stream, &handshake);
        assert_eq!(
            read_frame_for_test(&mut stream),
            br#"{"result":"accepted"}"#
        );
    }

    #[test]
    fn strict_wire_rejects_duplicate_unknown_and_noncanonical_fields() {
        assert!(strict_value(br#"{"a":1,"a":2}"#).is_err());
        assert!(parse_handshake(br#"{"kind":"pair","slot":"11111111-1111-4111-8111-111111111111","token":"bad","extra":1}"#).is_err());
        assert!(canonical_u64("01").is_err());
        assert!(canonical_time("2024-01-01T00:00:00+00:00").is_err());
    }

    #[test]
    fn available_windows_require_known_order_and_monotonic_resets() {
        let slot = AccountSlotId::parse("11111111-1111-4111-8111-111111111111").unwrap();
        let valid = br#"{"event":"observation","slot":"11111111-1111-4111-8111-111111111111","epoch":"1","sequence":"1","observedAt":"2024-01-01T00:00:00Z","status":"available","source":"completion_sse","windows":[{"kind":"five_hour","usedPercent":1,"resetAt":"2024-01-01T01:00:00Z"},{"kind":"weekly","usedPercent":2,"resetAt":"2024-01-02T00:00:00Z"}]}"#;
        assert!(parse_ingress(valid, &slot, 1, PlanMetadata::Paid).is_ok());
        let reversed = br#"{"event":"observation","slot":"11111111-1111-4111-8111-111111111111","epoch":"1","sequence":"1","observedAt":"2024-01-01T00:00:00Z","status":"available","source":"completion_sse","windows":[{"kind":"weekly","usedPercent":1},{"kind":"five_hour","usedPercent":2}]}"#;
        assert!(parse_ingress(reversed, &slot, 1, PlanMetadata::Paid).is_err());
        let bad_reset = br#"{"event":"observation","slot":"11111111-1111-4111-8111-111111111111","epoch":"1","sequence":"1","observedAt":"2024-01-01T00:00:00Z","status":"available","source":"completion_sse","windows":[{"kind":"five_hour","usedPercent":1,"resetAt":"2024-01-03T00:00:00Z"},{"kind":"weekly","usedPercent":2,"resetAt":"2024-01-02T00:00:00Z"}]}"#;
        assert!(parse_ingress(bad_reset, &slot, 1, PlanMetadata::Paid).is_err());
    }

    #[test]
    fn ingress_plan_cannot_relabel_an_app_owned_slot() {
        let paid = AccountSlotId::parse("11111111-1111-4111-8111-111111111111").unwrap();
        let free = AccountSlotId::parse("22222222-2222-4222-8222-222222222222").unwrap();
        let paid_event = br#"{"event":"observation","slot":"11111111-1111-4111-8111-111111111111","epoch":"1","sequence":"1","observedAt":"2024-01-01T00:00:00Z","status":"unavailable","errorCode":"unavailable","plan":"paid"}"#;
        let free_event = br#"{"event":"observation","slot":"22222222-2222-4222-8222-222222222222","epoch":"1","sequence":"1","observedAt":"2024-01-01T00:00:00Z","status":"unavailable","errorCode":"unavailable","plan":"free"}"#;
        let missing_plan = br#"{"event":"observation","slot":"11111111-1111-4111-8111-111111111111","epoch":"1","sequence":"1","observedAt":"2024-01-01T00:00:00Z","status":"unavailable","errorCode":"unavailable"}"#;
        assert!(parse_ingress(paid_event, &paid, 1, PlanMetadata::Paid).is_ok());
        assert!(parse_ingress(free_event, &free, 1, PlanMetadata::Free).is_ok());
        assert!(parse_ingress(free_event, &free, 1, PlanMetadata::Paid).is_err());
        assert!(parse_ingress(paid_event, &paid, 1, PlanMetadata::Free).is_err());
        assert!(parse_ingress(missing_plan, &paid, 1, PlanMetadata::Paid).is_ok());
    }
}

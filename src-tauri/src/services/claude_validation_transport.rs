//! Bounded Unix-domain ingress for the synthetic C3-B1A process boundary.
//!
//! There is intentionally no production listener constructor or Tauri wiring.
//! Tests create a task-owned socket root and pass an already accepted stream to
//! the crate-private session handler.

use super::claude_snapshot::{
    AccountSlotId, ClaudeSnapshotStore, PlanMetadata, SafeErrorCode, SourceClass, WindowKind,
};
use super::claude_synthetic_adapter::{
    submit_correlated_observation, CorrelatedDisposition, CorrelatedObservation, SyntheticWindow,
};
use super::claude_validation_pairing::{PairingResult, PairingTable};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PeerCredentials {
    pub(crate) uid: u32,
    pub(crate) pid: u32,
}

/// A task-owned fixed root whose mode/owner are checked before a socket name
/// is admitted.  Listener construction remains crate-private and unwired.
pub(crate) struct SocketRoot {
    root: PathBuf,
    descriptor: fs::File,
    dev: u64,
    ino: u64,
}

impl SocketRoot {
    pub(crate) fn for_synthetic(root: PathBuf) -> Result<Self, TransportError> {
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
        Ok(Self {
            root,
            descriptor,
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }

    pub(crate) fn socket_path(&self) -> Result<PathBuf, TransportError> {
        self.assert_pinned()?;
        let candidate = self.root.join("v1.sock");
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
        let path = self.socket_path()?;
        let listener = UnixListener::bind(&path).map_err(|_| TransportError::Rejected)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|_| TransportError::Io)?;
        self.assert_pinned()?;
        let metadata = fs::symlink_metadata(&path).map_err(|_| TransportError::Rejected)?;
        if !metadata.file_type().is_socket()
            || metadata.file_type().is_symlink()
            || metadata.uid() != unsafe { libc::geteuid() }
            || (metadata.mode() & 0o777) != 0o600
        {
            return Err(TransportError::Rejected);
        }
        Ok(listener)
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

/// Handles one already accepted, UID/PID-authenticated synthetic stream. The
/// caller must obtain peer credentials before this function; lack of that
/// proof is a terminal reject and this function has no bypass argument.
pub(crate) fn handle_authenticated_session(
    stream: &mut UnixStream,
    store: &ClaudeSnapshotStore,
    pairing: &mut PairingTable,
    peer_uid: u32,
    peer_pid: u32,
    now: DateTime<Utc>,
) -> Result<(), TransportError> {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .map_err(|_| TransportError::Io)?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|_| TransportError::Io)?;
    let handshake = read_frame(stream, HANDSHAKE_MAX)?;
    let (slot, token) = parse_handshake(&handshake)?;
    match pairing.consume(&slot, &token, peer_uid, peer_pid, now) {
        PairingResult::Accepted => write_fixed_result(stream, "accepted")?,
        PairingResult::Expired => {
            write_fixed_result(stream, "expired")?;
            return Err(TransportError::Expired);
        }
        PairingResult::Rejected => {
            write_fixed_result(stream, "rejected")?;
            return Err(TransportError::Rejected);
        }
    }
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|_| TransportError::Io)?;
    let mut frames = 1usize;
    let mut bytes = handshake.len() + 4;
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
        let authority = pairing
            .session_authority(&slot)
            .ok_or(TransportError::Rejected)?;
        let event = parse_ingress(&frame, &slot, authority.1)?;
        submit_correlated_observation(
            store,
            CorrelatedObservation {
                slot_id: slot.clone(),
                capability: authority.0,
                binding_epoch: authority.1,
                sequence: event.sequence,
                observed_at: event.observed_at,
                plan: event.plan,
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

fn write_fixed_result(stream: &mut UnixStream, result: &str) -> Result<(), TransportError> {
    let bytes = format!("{{\"result\":\"{result}\"}}").into_bytes();
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .map_err(|_| TransportError::Io)?;
    stream.write_all(&bytes).map_err(|_| TransportError::Io)
}

fn parse_handshake(bytes: &[u8]) -> Result<(AccountSlotId, Vec<u8>), TransportError> {
    let object = strict_object(bytes, &["kind", "slot", "token"])?;
    if string(&object, "kind")? != "pair" {
        return Err(TransportError::Rejected);
    }
    let slot =
        AccountSlotId::parse(string(&object, "slot")?).map_err(|_| TransportError::Rejected)?;
    let token = URL_SAFE_NO_PAD
        .decode(string(&object, "token")?)
        .map_err(|_| TransportError::Rejected)?;
    if token.len() != 32 {
        return Err(TransportError::Rejected);
    }
    Ok((slot, token))
}

struct ParsedIngress {
    sequence: u64,
    observed_at: DateTime<Utc>,
    plan: Option<PlanMetadata>,
    disposition: CorrelatedDisposition,
}

fn parse_ingress(
    bytes: &[u8],
    slot: &AccountSlotId,
    epoch: u64,
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
    let plan = match object.get("plan") {
        Some(Value::String(v)) if v == "paid" => Some(PlanMetadata::Paid),
        Some(Value::String(v)) if v == "free" => Some(PlanMetadata::Free),
        None => None,
        _ => return Err(TransportError::Rejected),
    };
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
        plan,
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

fn strict_object(bytes: &[u8], exact: &[&str]) -> Result<Map<String, Value>, TransportError> {
    let value = strict_value(bytes)?;
    let object = value.as_object().cloned().ok_or(TransportError::Rejected)?;
    required_exact(&object, exact, &[])?;
    Ok(object)
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
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::io::FromRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use uuid::Uuid;

    const CHILD_ENV: &str = "QUOTABAR_C3B1_SYNTHETIC_CHILD";

    #[test]
    fn pinned_socket_root_rejects_path_substitution() {
        let root = PathBuf::from("/tmp").join(format!("qt-c3b1-substitute-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket_root = SocketRoot::for_synthetic(root.clone()).unwrap();
        let moved = root.with_extension("moved");
        std::fs::rename(&root, &moved).unwrap();
        std::os::unix::fs::symlink("/tmp", &root).unwrap();
        assert_eq!(
            socket_root.socket_path().unwrap_err(),
            TransportError::Rejected
        );
        std::fs::remove_file(&root).unwrap();
        std::fs::remove_dir(moved).unwrap();
    }

    fn write_frame(stream: &mut UnixStream, bytes: &[u8]) {
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(bytes).unwrap();
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
        let root = PathBuf::from("/tmp").join(format!("qt-c3b1-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket_root = SocketRoot::for_synthetic(root.clone()).unwrap();
        let endpoint = socket_root.socket_path().unwrap();
        let listener = socket_root.bind().unwrap();
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
        let store = ClaudeSnapshotStore::at_root(root.join("state")).unwrap();
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
        let payload = serde_json::json!({
            "endpoint": endpoint,
            "token": bootstrap.canonical_token_for_synthetic_child(),
            "slot": bootstrap.slot_id(),
        });
        let bytes = serde_json::to_vec(&payload).unwrap();
        parent_bootstrap
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        parent_bootstrap.write_all(&bytes).unwrap();
        drop(parent_bootstrap);
        let (mut accepted, _) = listener.accept().unwrap();
        let peer = peer_credentials(&accepted).unwrap();
        assert_eq!(peer.uid, unsafe { libc::geteuid() });
        assert_eq!(peer.pid, child.id());
        handle_authenticated_session(&mut accepted, &store, &mut pairing, peer.uid, peer.pid, now)
            .unwrap();
        assert!(child.wait().unwrap().success());
        std::fs::remove_file(socket_root.root.join("v1.sock")).unwrap();
        std::fs::remove_dir(root.join("state")).unwrap_or(());
        std::fs::remove_dir(root).unwrap_or(());
    }

    fn synthetic_child() {
        let mut bootstrap = unsafe { UnixStream::from_raw_fd(3) };
        let payload = read_frame_for_test(&mut bootstrap);
        let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        let endpoint = payload["endpoint"].as_str().unwrap();
        let token = payload["token"].as_str().unwrap();
        let slot = payload["slot"].as_str().unwrap();
        let mut stream = UnixStream::connect(endpoint).unwrap();
        write_frame(
            &mut stream,
            serde_json::to_string(&serde_json::json!({
                "kind": "pair", "slot": slot, "token": token,
            }))
            .unwrap()
            .as_bytes(),
        );
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
        assert!(parse_ingress(valid, &slot, 1).is_ok());
        let reversed = br#"{"event":"observation","slot":"11111111-1111-4111-8111-111111111111","epoch":"1","sequence":"1","observedAt":"2024-01-01T00:00:00Z","status":"available","source":"completion_sse","windows":[{"kind":"weekly","usedPercent":1},{"kind":"five_hour","usedPercent":2}]}"#;
        assert!(parse_ingress(reversed, &slot, 1).is_err());
        let bad_reset = br#"{"event":"observation","slot":"11111111-1111-4111-8111-111111111111","epoch":"1","sequence":"1","observedAt":"2024-01-01T00:00:00Z","status":"available","source":"completion_sse","windows":[{"kind":"five_hour","usedPercent":1,"resetAt":"2024-01-03T00:00:00Z"},{"kind":"weekly","usedPercent":2,"resetAt":"2024-01-02T00:00:00Z"}]}"#;
        assert!(parse_ingress(bad_reset, &slot, 1).is_err());
    }
}

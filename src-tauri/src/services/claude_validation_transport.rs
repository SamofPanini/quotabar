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
use super::claude_validation_pairing::{PairingConsume, PairingRegistry, SessionAuthority};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const HANDSHAKE_MAX: usize = 1024;
const FRAME_MAX: usize = 16 * 1024;
const FRAME_BURST: u8 = 4;
const FRAME_REFILL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportError {
    Rejected,
    Expired,
    Io,
}

/// Integer, per-session ingress limiter.  It bounds long sessions without a
/// lifetime timer or a background activity source.
struct FrameBucket {
    tokens: u8,
    last_refill: Instant,
}

/// Redacted, non-serializable ownership record for a synthetic child session.
/// It deliberately owns the child and the sole parent endpoint; neither a path
/// nor peer PID is accepted as an authentication substitute.
struct SpawnOwnedSession {
    child: Child,
    parent: UnixStream,
    revoke: UnixStream,
    slot: AccountSlotId,
    generation: u64,
}

impl std::fmt::Debug for SpawnOwnedSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SpawnOwnedSession(<redacted>)")
    }
}

impl SpawnOwnedSession {
    fn revoke_and_reap(&mut self, pairing: &PairingRegistry) {
        pairing.revoke(&self.slot, self.generation);
        // This control endpoint is intentionally private to the spawn owner.
        // A best-effort byte wakes a cooperating child; failure is harmless
        // because the owned child is subsequently terminated and reaped.
        let _ = self.revoke.write_all(b"revoke");
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn run_spawn_owned_session(
    owner: &mut SpawnOwnedSession,
    store: &ClaudeSnapshotStore,
    pairing: &PairingRegistry,
) -> Result<(), TransportError> {
    let result = handle_authenticated_session(&mut owner.parent, store, pairing);
    if result.is_err() {
        owner.revoke_and_reap(pairing);
    }
    result
}

impl FrameBucket {
    fn new(now: Instant) -> Self {
        Self {
            tokens: FRAME_BURST,
            last_refill: now,
        }
    }
    fn take(&mut self, now: Instant) -> bool {
        let periods = now.duration_since(self.last_refill).as_secs() / FRAME_REFILL.as_secs();
        if periods > 0 {
            self.tokens = self
                .tokens
                .saturating_add(periods.min(u8::MAX as u64) as u8)
                .min(FRAME_BURST);
            self.last_refill += FRAME_REFILL * periods as u32;
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

/// Handles an owned parent end of a private inherited socketpair. The spawn
/// owner proves delivery by retaining the sole parent end; no socket peer PID
/// is claimed to identify the eventual child writer.
pub(crate) fn handle_authenticated_session(
    stream: &mut UnixStream,
    store: &ClaudeSnapshotStore,
    pairing: &PairingRegistry,
) -> Result<(), TransportError> {
    let authority = authenticate_session(stream, pairing)?;
    handle_session_loop(stream, store, authority)
}

/// Authentication is the only phase that borrows the shared pairing table.
/// Once this returns, callers may release their table guard before any frame
/// I/O or snapshot mutation begins.
pub(crate) fn authenticate_session(
    stream: &mut UnixStream,
    pairing: &PairingRegistry,
) -> Result<SessionAuthority, TransportError> {
    let handshake = read_secret_frame(stream, HANDSHAKE_MAX, Duration::from_secs(3))?;
    let (slot, token) = parse_handshake(&handshake)?;
    match pairing.consume(&slot, &token) {
        PairingConsume::Accepted(authority) => {
            write_fixed_result(stream, "accepted", Duration::from_secs(5))?;
            Ok(authority)
        }
        PairingConsume::Expired => {
            write_fixed_result(stream, "expired", Duration::from_secs(5))?;
            return Err(TransportError::Expired);
        }
        PairingConsume::Rejected => {
            write_fixed_result(stream, "rejected", Duration::from_secs(5))?;
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
) -> Result<(), TransportError> {
    // No idle deadline: EOF is normal and a new frame may arrive arbitrarily late.
    stream
        .set_read_timeout(None)
        .map_err(|_| TransportError::Io)?;
    let mut bucket = FrameBucket::new(Instant::now());
    let mut next_sequence = 1u64;
    loop {
        let frame = match read_frame(stream, FRAME_MAX, &mut bucket) {
            Ok(frame) => frame,
            Err(TransportError::Io) => return Ok(()),
            Err(error) => return Err(error),
        };
        let event = parse_ingress(&frame, &authority.slot_id, authority.epoch, authority.plan)?;
        if event.sequence != next_sequence {
            return Err(TransportError::Rejected);
        }
        // Sampling at each commit boundary avoids a session-wide frozen UTC
        // observation timestamp; deadlines remain entirely monotonic above.
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
            Utc::now(),
        )
        .map_err(|_| TransportError::Rejected)?;
        next_sequence = next_sequence
            .checked_add(1)
            .ok_or(TransportError::Rejected)?;
    }
}

fn read_frame(
    stream: &mut UnixStream,
    max: usize,
    bucket: &mut FrameBucket,
) -> Result<Vec<u8>, TransportError> {
    let mut length = [0u8; 4];
    match stream.read(&mut length[..1]) {
        Ok(0) => return Err(TransportError::Io),
        Ok(_) => {}
        Err(_) => return Err(TransportError::Io),
    }
    if !bucket.take(Instant::now()) {
        return Err(TransportError::Rejected);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    read_exact_until(stream, &mut length[1..], deadline)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > max {
        return Err(TransportError::Rejected);
    }
    let mut frame = vec![0; length];
    read_exact_until(stream, &mut frame, deadline)?;
    stream
        .set_read_timeout(None)
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
    budget: Duration,
) -> Result<Zeroizing<Vec<u8>>, TransportError> {
    let mut length = [0u8; 4];
    stream
        .set_read_timeout(None)
        .map_err(|_| TransportError::Io)?;
    stream
        .read_exact(&mut length[..1])
        .map_err(|_| TransportError::Io)?;
    let deadline = Instant::now() + budget;
    read_exact_until(stream, &mut length[1..], deadline)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > max {
        return Err(TransportError::Rejected);
    }
    let mut frame = Zeroizing::new(vec![0; length]);
    read_exact_until(stream, &mut frame, deadline)?;
    Ok(frame)
}

fn read_exact_until(
    stream: &mut UnixStream,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> Result<(), TransportError> {
    while !bytes.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(TransportError::Io)?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| TransportError::Io)?;
        match stream.read(bytes) {
            Ok(0) => return Err(TransportError::Io),
            Ok(read) => bytes = &mut bytes[read..],
            Err(_) => return Err(TransportError::Io),
        }
    }
    Ok(())
}

fn write_fixed_result(
    stream: &mut UnixStream,
    result: &str,
    budget: Duration,
) -> Result<(), TransportError> {
    let bytes = format!("{{\"result\":\"{result}\"}}").into_bytes();
    let deadline = Instant::now() + budget;
    write_all_until(stream, &(bytes.len() as u32).to_be_bytes(), deadline)?;
    write_all_until(stream, &bytes, deadline)
}

fn write_all_until(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    deadline: Instant,
) -> Result<(), TransportError> {
    while !bytes.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(TransportError::Io)?;
        stream
            .set_write_timeout(Some(remaining))
            .map_err(|_| TransportError::Io)?;
        match stream.write(bytes) {
            Ok(0) => return Err(TransportError::Io),
            Ok(written) => bytes = &bytes[written..],
            Err(_) => return Err(TransportError::Io),
        }
    }
    Ok(())
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
    // The framed protocol transports exactly one object. Whitespace is data
    // here, not a JSON transport convenience, so it is rejected at either
    // boundary rather than accepted by serde's permissive top-level parser.
    if bytes.first().is_some_and(u8::is_ascii_whitespace)
        || bytes.last().is_some_and(u8::is_ascii_whitespace)
    {
        return Err(TransportError::Rejected);
    }
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
    use super::super::claude_validation_pairing::PairingRegistry;
    use super::*;
    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::os::unix::io::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use uuid::Uuid;

    const CHILD_STAGE_ENV: &str = "QUOTABAR_C3B1_SYNTHETIC_CHILD_STAGE";

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

    fn has_cloexec(fd: i32) -> bool {
        unsafe { libc::fcntl(fd, libc::F_GETFD) & libc::FD_CLOEXEC != 0 }
    }

    fn run_production_transport_frames(
        plan: PlanMetadata,
        frames: Vec<Vec<u8>>,
    ) -> Result<(), TransportError> {
        let root = std::env::temp_dir().join(format!("quotabar-c3b1-seam-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root.clone()).unwrap();
        let pairing = PairingRegistry::new();
        let bootstrap = pairing
            .register_or_rebind(&store, plan, chrono::Utc::now())
            .unwrap();
        let handshake = synthetic_handshake(
            bootstrap.slot_id().as_str().as_bytes(),
            bootstrap.token_bytes_for_synthetic_child(),
        );
        let (mut server, mut client) = UnixStream::pair().unwrap();
        let client_worker = std::thread::spawn(move || {
            write_frame(&mut client, &handshake);
            assert_eq!(
                read_frame_for_test(&mut client),
                br#"{"result":"accepted"}"#
            );
            for frame in frames {
                write_frame(&mut client, &frame);
            }
            // Keep the writer endpoint alive until the server has a chance to
            // enter its post-auth frame loop; EOF is a distinct normal case.
            std::thread::sleep(Duration::from_millis(20));
            client.shutdown(Shutdown::Write).unwrap();
        });
        let result = handle_authenticated_session(&mut server, &store, &pairing);
        client_worker.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
        result
    }

    fn ingress(slot: &str, epoch: u64, sequence: u64, event: &str) -> Vec<u8> {
        format!(
            "{{\"event\":\"{event}\",\"slot\":\"{slot}\",\"epoch\":\"{epoch}\",\"sequence\":\"{sequence}\",\"observedAt\":\"2024-01-01T00:00:00Z\"}}"
        )
        .into_bytes()
    }

    #[test]
    fn reexecuted_synthetic_child_uses_private_inherited_fd() {
        match std::env::var(CHILD_STAGE_ENV).ok().as_deref() {
            Some("bootstrap") => {
                synthetic_child();
                return;
            }
            Some("second-exec") => {
                assert_eq!(
                    unsafe { libc::fcntl(3, libc::F_GETFD) },
                    -1,
                    "CLOEXEC must close the bootstrap mapping before a second exec"
                );
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EBADF)
                );
                return;
            }
            _ => {}
        }
        for _ in 0..25 {
            run_reexecuted_synthetic_child_once();
        }
    }

    fn run_reexecuted_synthetic_child_once() {
        let (parent_bootstrap, child_bootstrap) = UnixStream::pair().unwrap();
        let (parent_revoke, child_revoke) = UnixStream::pair().unwrap();
        let child_fd = child_bootstrap.as_raw_fd();
        let revoke_fd = child_revoke.as_raw_fd();
        assert!(has_cloexec(parent_bootstrap.as_raw_fd()));
        assert!(has_cloexec(child_fd));
        assert!(has_cloexec(parent_revoke.as_raw_fd()));
        assert!(has_cloexec(revoke_fd));
        let child = unsafe {
            Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("services::claude_validation_transport::tests::reexecuted_synthetic_child_uses_private_inherited_fd")
                .arg("--nocapture")
                .env(CHILD_STAGE_ENV, "bootstrap")
                .pre_exec(move || {
                    if libc::dup2(child_fd, 3) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::dup2(revoke_fd, 4) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()
                .unwrap()
        };
        drop(child_bootstrap);
        drop(child_revoke);
        let root = std::env::temp_dir().join(format!("quotabar-c3b1-r4-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root.clone()).unwrap();
        let pairing = PairingRegistry::new();
        let now = chrono::Utc::now();
        let bootstrap = pairing
            .register_or_rebind(&store, PlanMetadata::Paid, now)
            .unwrap();
        let mut owner = SpawnOwnedSession {
            child,
            parent: parent_bootstrap,
            revoke: parent_revoke,
            slot: bootstrap.slot_id().clone(),
            generation: bootstrap.epoch(),
        };
        let slot = bootstrap.slot_id().as_str().as_bytes();
        let token = bootstrap.token_bytes_for_synthetic_child();
        let probe = synthetic_handshake(slot, token);
        assert!(parse_handshake(&probe).is_ok());
        let mut bytes = Zeroizing::new(Vec::with_capacity(slot.len() + token.len()));
        bytes.extend_from_slice(slot);
        bytes.extend_from_slice(token);
        write_frame(&mut owner.parent, &bytes);
        run_spawn_owned_session(&mut owner, &store, &pairing).unwrap();
        assert_eq!(owner.slot, *bootstrap.slot_id());
        assert_eq!(owner.generation, bootstrap.epoch());
        assert!(owner.child.wait().unwrap().success());
        std::fs::remove_dir_all(root).unwrap_or(());
    }

    fn synthetic_child() {
        let mut bootstrap = unsafe { UnixStream::from_raw_fd(3) };
        let revoke = unsafe { UnixStream::from_raw_fd(4) };
        assert!(!has_cloexec(bootstrap.as_raw_fd()));
        assert!(!has_cloexec(revoke.as_raw_fd()));
        assert_eq!(
            unsafe { libc::fcntl(bootstrap.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
        assert!(has_cloexec(bootstrap.as_raw_fd()));
        assert_eq!(
            unsafe { libc::fcntl(revoke.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
        assert!(has_cloexec(revoke.as_raw_fd()));
        let second = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("services::claude_validation_transport::tests::reexecuted_synthetic_child_uses_private_inherited_fd")
            .arg("--nocapture")
            .env(CHILD_STAGE_ENV, "second-exec")
            .status()
            .unwrap();
        assert!(second.success());
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
        assert!(strict_value(br#" {"a":1}"#).is_err());
        assert!(strict_value(b"{\"a\":1}\n").is_err());
        assert!(strict_value(br#"{"a":1}{"b":2}"#).is_err());
        assert!(parse_handshake(br#"{"kind":"pair","slot":"11111111-1111-4111-8111-111111111111","token":"bad","extra":1}"#).is_err());
        assert!(canonical_u64("01").is_err());
        assert!(canonical_time("2024-01-01T00:00:00+00:00").is_err());
    }

    #[test]
    fn per_session_bucket_has_fixed_burst_and_monotonic_refill_boundaries() {
        let start = Instant::now();
        let mut bucket = FrameBucket::new(start);
        for _ in 0..4 {
            assert!(bucket.take(start));
        }
        assert!(!bucket.take(start));
        assert!(!bucket.take(start + Duration::from_millis(14_999)));
        assert!(bucket.take(start + Duration::from_secs(15)));
        assert!(!bucket.take(start + Duration::from_secs(15)));
        assert!(bucket.take(start + Duration::from_secs(30)));
        let mut sustained = FrameBucket::new(start);
        for index in 0..100u64 {
            assert!(sustained.take(start + Duration::from_secs(index * 15)));
        }
    }

    #[test]
    fn blocked_handshake_does_not_hold_pairing_lock_or_block_second_session() {
        let root = std::env::temp_dir().join(format!("quotabar-c3b1-lock-{}", Uuid::new_v4()));
        let store = std::sync::Arc::new(ClaudeSnapshotStore::at_root(root.clone()).unwrap());
        let pairing = std::sync::Arc::new(PairingRegistry::new());
        let first = pairing
            .register_or_rebind(&store, PlanMetadata::Paid, chrono::Utc::now())
            .unwrap();
        let second = pairing
            .register_or_rebind(&store, PlanMetadata::Free, chrono::Utc::now())
            .unwrap();
        let (mut first_server, mut first_client) = UnixStream::pair().unwrap();
        first_client.write_all(&[0]).unwrap();
        let first_store = store.clone();
        let first_pairing = pairing.clone();
        let blocked = std::thread::spawn(move || {
            handle_authenticated_session(&mut first_server, &first_store, &first_pairing)
        });
        std::thread::sleep(Duration::from_millis(25));
        assert!(pairing.try_lock_available_for_test());

        let (mut second_server, mut second_client) = UnixStream::pair().unwrap();
        let second_handshake = synthetic_handshake(
            second.slot_id().as_str().as_bytes(),
            second.token_bytes_for_synthetic_child(),
        );
        write_frame(&mut second_client, &second_handshake);
        second_client.shutdown(Shutdown::Write).unwrap();
        assert_eq!(
            handle_authenticated_session(&mut second_server, &store, &pairing),
            Ok(())
        );
        first_client.shutdown(Shutdown::Both).unwrap();
        assert!(matches!(blocked.join().unwrap(), Err(TransportError::Io)));
        // The blocked first handshake has not reached the consume point; the
        // available try-lock above is the lock-boundary evidence.
        assert_eq!(
            first.slot_id().as_str(),
            "11111111-1111-4111-8111-111111111111"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_owner_session_revokes_exact_generation_and_reaps_child() {
        let root = std::env::temp_dir().join(format!("quotabar-c3b1-revoke-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root.clone()).unwrap();
        let pairing = PairingRegistry::new();
        let bootstrap = pairing
            .register_or_rebind(&store, PlanMetadata::Paid, chrono::Utc::now())
            .unwrap();
        let token = bootstrap.decoded_token_for_test();
        let (parent, mut child_end) = UnixStream::pair().unwrap();
        let (revoke, _child_revoke) = UnixStream::pair().unwrap();
        let child = Command::new("/usr/bin/true").spawn().unwrap();
        let mut owner = SpawnOwnedSession {
            child,
            parent,
            revoke,
            slot: bootstrap.slot_id().clone(),
            generation: bootstrap.epoch(),
        };
        // A malformed one-byte header fails before consume, then the owner
        // must revoke its still-pending authority and reap its exact child.
        child_end.write_all(&[0]).unwrap();
        child_end.shutdown(Shutdown::Write).unwrap();
        assert!(matches!(
            run_spawn_owned_session(&mut owner, &store, &pairing),
            Err(TransportError::Io)
        ));
        assert!(owner.child.try_wait().unwrap().is_some());
        assert!(matches!(
            pairing.consume(bootstrap.slot_id(), &token),
            PairingConsume::Rejected
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn production_transport_seam_accepts_lifecycle_and_unavailable_routes() {
        let slot = "11111111-1111-4111-8111-111111111111";
        let unavailable = format!(
            "{{\"event\":\"observation\",\"slot\":\"{slot}\",\"epoch\":\"1\",\"sequence\":\"1\",\"observedAt\":\"2024-01-01T00:00:00Z\",\"status\":\"unavailable\",\"errorCode\":\"unavailable\"}}"
        )
        .into_bytes();
        let lifecycle = ingress(slot, 1, 2, "continuity_uncertain");
        assert_eq!(
            run_production_transport_frames(PlanMetadata::Paid, vec![unavailable, lifecycle]),
            Ok(())
        );
    }

    #[test]
    fn production_transport_seam_rejects_wrong_slot_and_sequence_skip() {
        let paid = "11111111-1111-4111-8111-111111111111";
        let free = "22222222-2222-4222-8222-222222222222";
        assert_eq!(
            run_production_transport_frames(
                PlanMetadata::Paid,
                vec![ingress(free, 1, 1, "identity_changed")]
            ),
            Err(TransportError::Rejected)
        );
        assert_eq!(
            run_production_transport_frames(
                PlanMetadata::Paid,
                vec![ingress(paid, 1, 2, "identity_changed")]
            ),
            Err(TransportError::Rejected)
        );
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

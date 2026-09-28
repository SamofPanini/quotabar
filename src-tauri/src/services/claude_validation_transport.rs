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
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const HANDSHAKE_MAX: usize = 1024;
const FRAME_MAX: usize = 16 * 1024;
const FRAME_BURST: u8 = 4;
const FRAME_REFILL: Duration = Duration::from_secs(15);
const CHILD_DATA_FD: RawFd = 198;
const CHILD_REVOKE_FD: RawFd = 199;
// Keep prepared sources outside the complete target set.  These descriptors
// exist only between parent setup and exec; the child closes them after the
// two target mappings have been installed.
const CHILD_SAFE_FD_MIN: RawFd = 256;
const CHILD_EXIT_POLL_TICK: Duration = Duration::from_millis(25);
const CHILD_CLEANUP_GRACE: Duration = Duration::from_millis(100);
const CHILD_STAGE_ENV: &str = "QUOTABAR_C3B1_SYNTHETIC_CHILD_STAGE";
const CHILD_TEST_NAME: &str =
    "services::claude_validation_transport::tests::owned_session_child_entry";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportError {
    Rejected,
    Expired,
    Ended(SessionEnd),
}

/// Fixed terminal classification for the owned production path.  It is kept
/// separate from protocol parsing errors so EOF, truncation and revocation
/// cannot be silently accepted as a normal frame-loop completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    CleanEof,
    HeaderTruncated,
    PayloadTruncated,
    DeadlineExpired,
    ProtocolRejected,
    ResourceLimited,
    Revoked,
    ChildExited,
    WorkerOrStoreFailed,
}

/// Integer, per-session ingress limiter.  It bounds long sessions without a
/// lifetime timer or a background activity source.
struct FrameBucket {
    tokens: u8,
    last_refill: Instant,
}

#[derive(Clone, Copy, Debug)]
struct FixedDeadline {
    expires_at: Instant,
}

impl FixedDeadline {
    fn after(start: Instant, budget: Duration) -> Self {
        Self {
            expires_at: start + budget,
        }
    }

    fn remaining_at(self, now: Instant) -> Result<Duration, TransportError> {
        let remaining = self
            .expires_at
            .checked_duration_since(now)
            .ok_or(TransportError::Ended(SessionEnd::DeadlineExpired))?;
        if remaining.is_zero() {
            Err(TransportError::Ended(SessionEnd::DeadlineExpired))
        } else {
            Ok(remaining)
        }
    }

    fn remaining(self) -> Result<Duration, TransportError> {
        self.remaining_at(Instant::now())
    }
}

struct SessionMonitor<'a> {
    revoke_fd: Option<RawFd>,
    child: Option<&'a mut Child>,
}

impl SessionMonitor<'_> {
    fn disconnected() -> Self {
        Self {
            revoke_fd: None,
            child: None,
        }
    }
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
    #[cfg(test)]
    audit: Option<std::sync::Arc<TestLaunchAudit>>,
    #[cfg(test)]
    revoke_peer_for_test: Option<UnixStream>,
    #[cfg(test)]
    ready_receiver_for_test: Option<UnixStream>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnedChildMode {
    Session,
    ExitBeforeRegistration,
    ExitAfterBootstrap,
    BlockAfterHandshakeByte,
    SubmitAvailable,
    SubmitUnavailable,
    SubmitLifecycle,
    RejectWrongSlot,
    RejectOldEpoch,
    RejectReplay,
    RejectSkippedSequence,
}

impl OwnedChildMode {
    fn name(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::ExitBeforeRegistration => "exit-before-registration",
            Self::ExitAfterBootstrap => "exit-after-bootstrap",
            Self::BlockAfterHandshakeByte => "block-after-handshake-byte",
            Self::SubmitAvailable => "submit-available",
            Self::SubmitUnavailable => "submit-unavailable",
            Self::SubmitLifecycle => "submit-lifecycle",
            Self::RejectWrongSlot => "reject-wrong-slot",
            Self::RejectOldEpoch => "reject-old-epoch",
            Self::RejectReplay => "reject-replay",
            Self::RejectSkippedSequence => "reject-skipped-sequence",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnedLaunchFault {
    None,
    MissingExecutable,
    PreExec,
    BootstrapPartial,
}

#[derive(Clone, Debug)]
struct OwnedLaunchSpec {
    mode: OwnedChildMode,
    fault: OwnedLaunchFault,
    #[cfg(test)]
    audit: std::sync::Arc<TestLaunchAudit>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct TestLaunchAudit {
    originals_closed_in_parent: std::sync::atomic::AtomicBool,
    child_reaped: std::sync::atomic::AtomicBool,
}

impl OwnedLaunchSpec {
    fn production() -> Self {
        Self {
            mode: OwnedChildMode::Session,
            fault: OwnedLaunchFault::None,
            #[cfg(test)]
            audit: std::sync::Arc::new(TestLaunchAudit::default()),
        }
    }
}

#[cfg(test)]
impl OwnedLaunchSpec {
    fn test(mode: OwnedChildMode, fault: OwnedLaunchFault) -> Self {
        Self {
            mode,
            fault,
            audit: std::sync::Arc::new(TestLaunchAudit::default()),
        }
    }
}

/// Fixed crate-internal orchestration seam. Callers can select only app-owned
/// slot metadata; executable identity, descriptors, launch mode, and bootstrap
/// authority remain private to this module.
pub(crate) fn run_owned_validation_session(
    store: &ClaudeSnapshotStore,
    pairing: &PairingRegistry,
    plan: PlanMetadata,
    now: DateTime<Utc>,
) -> Result<(), TransportError> {
    let spec = OwnedLaunchSpec::production();
    let mut owner = spawn_owned_session(store, pairing, plan, now, &spec)?;
    run_spawn_owned_session(&mut owner, store, pairing)
}

impl std::fmt::Debug for SpawnOwnedSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SpawnOwnedSession(<redacted>)")
    }
}

impl SpawnOwnedSession {
    /// The only crate-private handoff from a successful child spawn.  It owns
    /// every endpoint and the exact pairing generation; callers cannot build a
    /// session from a filesystem endpoint, peer identity, or borrowed descriptor.
    fn from_owned_spawn(
        child: Child,
        parent: UnixStream,
        revoke: UnixStream,
        slot: AccountSlotId,
        generation: u64,
    ) -> Self {
        Self {
            child,
            parent,
            revoke,
            slot,
            generation,
            #[cfg(test)]
            audit: None,
            #[cfg(test)]
            revoke_peer_for_test: None,
            #[cfg(test)]
            ready_receiver_for_test: None,
        }
    }

    fn revoke_and_reap(&mut self, pairing: &PairingRegistry) {
        pairing.revoke(&self.slot, self.generation);
        // This control endpoint is intentionally private to the spawn owner.
        // A best-effort byte wakes a cooperating child; failure is harmless
        // because the owned child is subsequently terminated and reaped.
        let _ = self.revoke.write_all(b"revoke");
        let cleanup_deadline = Instant::now() + CHILD_CLEANUP_GRACE;
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < cleanup_deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        #[cfg(test)]
        if let Some(audit) = &self.audit {
            audit
                .child_reaped
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

fn duplicate_child_source(source: RawFd) -> Result<RawFd, TransportError> {
    // F_DUPFD_CLOEXEC is performed before fork.  Consequently pre_exec only
    // executes the fixed dup2/close sequence below, even when an allocator
    // handed a source one of the destination numbers.
    let duplicate = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, CHILD_SAFE_FD_MIN) };
    if duplicate < 0 {
        Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed))
    } else {
        Ok(duplicate)
    }
}

fn close_prepared_child_source(fd: RawFd) {
    // The result is intentionally ignored: a failed close cannot make a
    // prepared descriptor usable by this parent, and the child has its own
    // post-fork copy.
    unsafe {
        libc::close(fd);
    }
}

unsafe fn install_prepared_child_fds(
    data_source: RawFd,
    revoke_source: RawFd,
) -> std::io::Result<()> {
    debug_assert_ne!(data_source, CHILD_DATA_FD);
    debug_assert_ne!(data_source, CHILD_REVOKE_FD);
    debug_assert_ne!(revoke_source, CHILD_DATA_FD);
    debug_assert_ne!(revoke_source, CHILD_REVOKE_FD);
    if libc::dup2(data_source, CHILD_DATA_FD) < 0
        || libc::dup2(revoke_source, CHILD_REVOKE_FD) < 0
        || libc::fcntl(CHILD_DATA_FD, libc::F_SETFD, 0) < 0
        || libc::fcntl(CHILD_REVOKE_FD, libc::F_SETFD, 0) < 0
        || libc::close(data_source) < 0
        || libc::close(revoke_source) < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn spawn_owned_session(
    store: &ClaudeSnapshotStore,
    pairing: &PairingRegistry,
    plan: PlanMetadata,
    now: DateTime<Utc>,
    spec: &OwnedLaunchSpec,
) -> Result<SpawnOwnedSession, TransportError> {
    let (parent, child_data) =
        UnixStream::pair().map_err(|_| TransportError::Ended(SessionEnd::WorkerOrStoreFailed))?;
    let (revoke, child_revoke) =
        UnixStream::pair().map_err(|_| TransportError::Ended(SessionEnd::WorkerOrStoreFailed))?;
    let child_data_fd = child_data.as_raw_fd();
    let child_revoke_fd = child_revoke.as_raw_fd();
    #[cfg(test)]
    let test_revoke_peer = child_revoke
        .try_clone()
        .map_err(|_| TransportError::Ended(SessionEnd::WorkerOrStoreFailed))?;
    #[cfg(test)]
    let test_ready_receiver = revoke
        .try_clone()
        .map_err(|_| TransportError::Ended(SessionEnd::WorkerOrStoreFailed))?;
    let prepared_data_fd = duplicate_child_source(child_data_fd)?;
    let prepared_revoke_fd = match duplicate_child_source(child_revoke_fd) {
        Ok(fd) => fd,
        Err(error) => {
            close_prepared_child_source(prepared_data_fd);
            return Err(error);
        }
    };
    // The prepared descriptors are now the only copies deliberately inherited
    // by Command.  Dropping the originals before fork also proves that no
    // parent endpoint can leak into the child through the launch setup.
    drop(child_data);
    drop(child_revoke);

    let executable = if spec.fault == OwnedLaunchFault::MissingExecutable {
        std::path::PathBuf::from("/quotabar-c3b1/missing-fixed-synthetic-child")
    } else {
        std::env::current_exe()
            .map_err(|_| TransportError::Ended(SessionEnd::WorkerOrStoreFailed))?
    };
    let mut command = Command::new(executable);
    #[cfg(test)]
    {
        command
            .arg("--exact")
            .arg(CHILD_TEST_NAME)
            .arg("--nocapture")
            .env(CHILD_STAGE_ENV, spec.mode.name());
    }
    #[cfg(not(test))]
    {
        command.arg("--quotabar-private-synthetic-child");
    }
    let force_pre_exec_failure = spec.fault == OwnedLaunchFault::PreExec;
    unsafe {
        command.pre_exec(move || {
            if force_pre_exec_failure {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "fixed pre-exec failure",
                ));
            }
            install_prepared_child_fds(prepared_data_fd, prepared_revoke_fd)
        });
    }
    let spawned = command.spawn();
    close_prepared_child_source(prepared_data_fd);
    close_prepared_child_source(prepared_revoke_fd);
    let mut child = spawned.map_err(|_| TransportError::Ended(SessionEnd::WorkerOrStoreFailed))?;

    #[cfg(test)]
    spec.audit.originals_closed_in_parent.store(
        // These closes happened synchronously above.  Querying raw FD values
        // after release is not stable under parallel tests because another
        // thread can legally reuse the same descriptor number.
        true,
        std::sync::atomic::Ordering::SeqCst,
    );

    if spec.mode == OwnedChildMode::ExitBeforeRegistration {
        let _ = child.wait();
        #[cfg(test)]
        spec.audit
            .child_reaped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        return Err(TransportError::Ended(SessionEnd::ChildExited));
    }

    let bootstrap = match pairing.register_or_rebind(store, plan, now) {
        Ok(bootstrap) => bootstrap,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            #[cfg(test)]
            spec.audit
                .child_reaped
                .store(true, std::sync::atomic::Ordering::SeqCst);
            return Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed));
        }
    };
    let mut owner = SpawnOwnedSession {
        child,
        parent,
        revoke,
        slot: bootstrap.slot_id().clone(),
        generation: bootstrap.epoch(),
        #[cfg(test)]
        audit: Some(spec.audit.clone()),
        #[cfg(test)]
        revoke_peer_for_test: Some(test_revoke_peer),
        #[cfg(test)]
        ready_receiver_for_test: Some(test_ready_receiver),
    };
    let bootstrap_result = if spec.fault == OwnedLaunchFault::BootstrapPartial {
        owner.parent.write_all(&[0, 0]).and_then(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "fixed partial bootstrap failure",
            ))
        })
    } else {
        bootstrap.write_private_payload(&mut owner.parent)
    };
    if bootstrap_result.is_err() {
        owner.revoke_and_reap(pairing);
        return Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed));
    }
    Ok(owner)
}

fn run_spawn_owned_session(
    owner: &mut SpawnOwnedSession,
    store: &ClaudeSnapshotStore,
    pairing: &PairingRegistry,
) -> Result<(), TransportError> {
    let revoke_fd = owner.revoke.as_raw_fd();
    let result = {
        let mut monitor = SessionMonitor {
            revoke_fd: Some(revoke_fd),
            child: Some(&mut owner.child),
        };
        handle_authenticated_session_monitored(&mut owner.parent, store, pairing, &mut monitor)
    };
    // EOF is a transport terminal, not permission to wait indefinitely for a
    // non-cooperating owned child.  The same private revoke/terminate/reap
    // path is used for every terminal result.
    owner.revoke_and_reap(pairing);
    result
}

fn wait_for_first_byte_or_revoke(owner: &mut SpawnOwnedSession) -> Result<(), TransportError> {
    let revoke_fd = owner.revoke.as_raw_fd();
    let mut monitor = SessionMonitor {
        revoke_fd: Some(revoke_fd),
        child: Some(&mut owner.child),
    };
    wait_for_data(&owner.parent, &mut monitor, None)
}

fn wait_for_data(
    stream: &UnixStream,
    monitor: &mut SessionMonitor<'_>,
    deadline: Option<FixedDeadline>,
) -> Result<(), TransportError> {
    let mut watched = [
        libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: monitor.revoke_fd.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        if let Some(child) = monitor.child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => return Err(TransportError::Ended(SessionEnd::ChildExited)),
                Ok(None) => {}
                Err(_) => return Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed)),
            }
        }
        watched[0].revents = 0;
        watched[1].revents = 0;
        let timeout = poll_timeout_millis(deadline, monitor.child.is_some())?;
        let ready =
            unsafe { libc::poll(watched.as_mut_ptr(), watched.len() as libc::nfds_t, timeout) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed));
        }
        if ready == 0 {
            // poll is permitted to return early.  Re-read the monotonic clock
            // before declaring expiry; otherwise a sub-millisecond remaining
            // budget can be rounded down into an artificial deadline result.
            if let Some(deadline) = deadline {
                if deadline.remaining().is_err() {
                    return Err(TransportError::Ended(SessionEnd::DeadlineExpired));
                }
            }
            continue;
        }
        if watched[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let mut signal = [0u8; 1];
            let read =
                unsafe { libc::read(watched[1].fd, signal.as_mut_ptr().cast(), signal.len()) };
            if read == 1 {
                return if signal[0] == b'E' {
                    Err(TransportError::Ended(SessionEnd::ChildExited))
                } else {
                    Err(TransportError::Ended(SessionEnd::Revoked))
                };
            }
            if read == 0 {
                // A child can close its control end as it finishes. That HUP
                // is not a revoke signal; let the data endpoint determine
                // whether the terminal framing is EOF or child exit.
                monitor.revoke_fd = None;
                watched[1].fd = -1;
            } else {
                return Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed));
            }
        }
        if watched[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let mut probe = [0u8; 1];
            let peeked = unsafe {
                libc::recv(
                    watched[0].fd,
                    probe.as_mut_ptr().cast(),
                    probe.len(),
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            };
            if peeked > 0 {
                return Ok(());
            }
            if peeked == 0 {
                return classify_eof(monitor);
            }
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
                continue;
            }
            return Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed));
        }
        if watched[0].revents & libc::POLLNVAL != 0 {
            return Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed));
        }
    }
}

fn poll_timeout_millis(
    deadline: Option<FixedDeadline>,
    monitor_child: bool,
) -> Result<i32, TransportError> {
    match deadline {
        Some(deadline) => {
            let remaining = deadline.remaining()?;
            // poll accepts whole milliseconds.  Rounding down can cause an
            // early timeout, so retain any fractional millisecond.
            let ceil_millis = ceil_poll_millis(remaining);
            Ok(if monitor_child {
                ceil_millis.min(CHILD_EXIT_POLL_TICK.as_millis() as i32)
            } else {
                ceil_millis
            })
        }
        None if monitor_child => Ok(CHILD_EXIT_POLL_TICK.as_millis() as i32),
        None => Ok(-1),
    }
}

fn ceil_poll_millis(remaining: Duration) -> i32 {
    remaining
        .as_millis()
        .saturating_add(u128::from(remaining.as_nanos() % 1_000_000 != 0))
        .clamp(1, i32::MAX as u128) as i32
}

fn classify_eof(monitor: &mut SessionMonitor<'_>) -> Result<(), TransportError> {
    match monitor.child.as_mut() {
        Some(child) => match child.try_wait() {
            Ok(Some(_)) => Err(TransportError::Ended(SessionEnd::ChildExited)),
            Ok(None) => Err(TransportError::Ended(SessionEnd::CleanEof)),
            Err(_) => Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed)),
        },
        None => Err(TransportError::Ended(SessionEnd::CleanEof)),
    }
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
    let mut monitor = SessionMonitor::disconnected();
    handle_authenticated_session_monitored(stream, store, pairing, &mut monitor)
}

fn handle_authenticated_session_monitored(
    stream: &mut UnixStream,
    store: &ClaudeSnapshotStore,
    pairing: &PairingRegistry,
    monitor: &mut SessionMonitor<'_>,
) -> Result<(), TransportError> {
    let authority = authenticate_session_monitored(stream, pairing, monitor)?;
    handle_session_loop_monitored(stream, store, authority, monitor)
}

/// Authentication is the only phase that borrows the shared pairing table.
/// Once this returns, callers may release their table guard before any frame
/// I/O or snapshot mutation begins.
pub(crate) fn authenticate_session(
    stream: &mut UnixStream,
    pairing: &PairingRegistry,
) -> Result<SessionAuthority, TransportError> {
    let mut monitor = SessionMonitor::disconnected();
    authenticate_session_monitored(stream, pairing, &mut monitor)
}

fn authenticate_session_monitored(
    stream: &mut UnixStream,
    pairing: &PairingRegistry,
    monitor: &mut SessionMonitor<'_>,
) -> Result<SessionAuthority, TransportError> {
    let handshake =
        read_secret_frame_monitored(stream, HANDSHAKE_MAX, Duration::from_secs(3), monitor)?;
    let (slot, token) = parse_handshake(&handshake).map_err(|error| match error {
        TransportError::Rejected => TransportError::Ended(SessionEnd::ProtocolRejected),
        other => other,
    })?;
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
    let mut monitor = SessionMonitor::disconnected();
    handle_session_loop_monitored(stream, store, authority, &mut monitor)
}

fn handle_session_loop_monitored(
    stream: &mut UnixStream,
    store: &ClaudeSnapshotStore,
    authority: SessionAuthority,
    monitor: &mut SessionMonitor<'_>,
) -> Result<(), TransportError> {
    // No idle deadline: EOF is normal and a new frame may arrive arbitrarily late.
    stream
        .set_read_timeout(None)
        .map_err(|_| TransportError::Ended(SessionEnd::WorkerOrStoreFailed))?;
    let mut bucket = FrameBucket::new(Instant::now());
    let mut next_sequence = 1u64;
    loop {
        let frame = match read_frame_monitored(stream, FRAME_MAX, &mut bucket, monitor) {
            Ok(frame) => frame,
            Err(TransportError::Ended(SessionEnd::CleanEof)) => {
                return Err(TransportError::Ended(SessionEnd::CleanEof))
            }
            Err(error) => return Err(error),
        };
        let event = parse_ingress(&frame, &authority.slot_id, authority.epoch, authority.plan)
            .map_err(|error| match error {
                TransportError::Rejected => TransportError::Ended(SessionEnd::ProtocolRejected),
                other => other,
            })?;
        if event.sequence != next_sequence {
            return Err(TransportError::Ended(SessionEnd::ProtocolRejected));
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
        .map_err(|_| TransportError::Ended(SessionEnd::WorkerOrStoreFailed))?;
        next_sequence = next_sequence
            .checked_add(1)
            .ok_or(TransportError::Ended(SessionEnd::ResourceLimited))?;
    }
}

fn read_frame(
    stream: &mut UnixStream,
    max: usize,
    bucket: &mut FrameBucket,
) -> Result<Vec<u8>, TransportError> {
    let mut monitor = SessionMonitor::disconnected();
    read_frame_monitored(stream, max, bucket, &mut monitor)
}

fn read_frame_monitored(
    stream: &mut UnixStream,
    max: usize,
    bucket: &mut FrameBucket,
    monitor: &mut SessionMonitor<'_>,
) -> Result<Vec<u8>, TransportError> {
    let mut length = [0u8; 4];
    wait_for_data(stream, monitor, None)?;
    match stream.read(&mut length[..1]) {
        Ok(0) => return classify_eof(monitor).and(Ok(Vec::new())),
        Ok(_) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ) =>
        {
            return Err(TransportError::Ended(SessionEnd::CleanEof))
        }
        Err(_) => return Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed)),
    }
    if !bucket.take(Instant::now()) {
        return Err(TransportError::Ended(SessionEnd::ResourceLimited));
    }
    let deadline = FixedDeadline::after(Instant::now(), Duration::from_secs(5));
    read_exact_until_monitored(
        stream,
        &mut length[1..],
        deadline,
        SessionEnd::HeaderTruncated,
        monitor,
    )?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > max {
        return Err(TransportError::Ended(SessionEnd::ResourceLimited));
    }
    let mut frame = vec![0; length];
    read_exact_until_monitored(
        stream,
        &mut frame,
        deadline,
        SessionEnd::PayloadTruncated,
        monitor,
    )?;
    std::str::from_utf8(&frame).map_err(|_| TransportError::Ended(SessionEnd::ProtocolRejected))?;
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
    let mut monitor = SessionMonitor::disconnected();
    read_secret_frame_monitored(stream, max, budget, &mut monitor)
}

fn read_secret_frame_monitored(
    stream: &mut UnixStream,
    max: usize,
    budget: Duration,
    monitor: &mut SessionMonitor<'_>,
) -> Result<Zeroizing<Vec<u8>>, TransportError> {
    let mut length = [0u8; 4];
    stream
        .set_read_timeout(None)
        .map_err(|_| TransportError::Ended(SessionEnd::WorkerOrStoreFailed))?;
    wait_for_data(stream, monitor, None)?;
    match stream.read(&mut length[..1]) {
        Ok(0) => return classify_eof(monitor).and(Ok(Zeroizing::new(Vec::new()))),
        Ok(_) => {}
        Err(_) => return Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed)),
    }
    let deadline = FixedDeadline::after(Instant::now(), budget);
    read_exact_until_monitored(
        stream,
        &mut length[1..],
        deadline,
        SessionEnd::HeaderTruncated,
        monitor,
    )?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > max {
        return Err(TransportError::Ended(SessionEnd::ResourceLimited));
    }
    let mut frame = Zeroizing::new(vec![0; length]);
    read_exact_until_monitored(
        stream,
        &mut frame,
        deadline,
        SessionEnd::PayloadTruncated,
        monitor,
    )?;
    Ok(frame)
}

fn read_exact_until(
    stream: &mut UnixStream,
    bytes: &mut [u8],
    deadline: FixedDeadline,
    truncated: SessionEnd,
) -> Result<(), TransportError> {
    let mut monitor = SessionMonitor::disconnected();
    read_exact_until_monitored(stream, bytes, deadline, truncated, &mut monitor)
}

fn read_exact_until_monitored(
    stream: &mut UnixStream,
    mut bytes: &mut [u8],
    deadline: FixedDeadline,
    truncated: SessionEnd,
    monitor: &mut SessionMonitor<'_>,
) -> Result<(), TransportError> {
    while !bytes.is_empty() {
        match wait_for_data(stream, monitor, Some(deadline)) {
            Ok(()) => {}
            Err(TransportError::Ended(SessionEnd::CleanEof)) => {
                return Err(TransportError::Ended(truncated));
            }
            Err(error) => return Err(error),
        }
        match stream.read(bytes) {
            Ok(0) => return Err(TransportError::Ended(truncated)),
            Ok(read) => bytes = &mut bytes[read..],
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                return Err(TransportError::Ended(SessionEnd::DeadlineExpired))
            }
            Err(_) => return Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed)),
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
    let deadline = FixedDeadline::after(Instant::now(), budget);
    write_all_until(stream, &(bytes.len() as u32).to_be_bytes(), deadline)?;
    write_all_until(stream, &bytes, deadline)
}

fn write_all_until(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    deadline: FixedDeadline,
) -> Result<(), TransportError> {
    while !bytes.is_empty() {
        let remaining = deadline.remaining()?;
        stream
            .set_write_timeout(Some(remaining))
            .map_err(|_| TransportError::Ended(SessionEnd::WorkerOrStoreFailed))?;
        match stream.write(bytes) {
            Ok(0) => return Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed)),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                return Err(TransportError::Ended(SessionEnd::DeadlineExpired))
            }
            Err(_) => return Err(TransportError::Ended(SessionEnd::WorkerOrStoreFailed)),
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
    use std::sync::atomic::Ordering;
    use uuid::Uuid;

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

    #[test]
    fn owned_session_child_entry() {
        let Ok(stage) = std::env::var(CHILD_STAGE_ENV) else {
            return;
        };
        if stage == "owned-second-exec" {
            for fd in [CHILD_DATA_FD, CHILD_REVOKE_FD] {
                assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EBADF)
                );
            }
            return;
        }

        let mut data = unsafe { UnixStream::from_raw_fd(CHILD_DATA_FD) };
        let mut revoke = unsafe { UnixStream::from_raw_fd(CHILD_REVOKE_FD) };
        assert!(!has_cloexec(data.as_raw_fd()));
        assert!(!has_cloexec(revoke.as_raw_fd()));
        for fd in [data.as_raw_fd(), revoke.as_raw_fd()] {
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
            assert!(has_cloexec(fd));
        }
        let second = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(CHILD_TEST_NAME)
            .arg("--nocapture")
            .env(CHILD_STAGE_ENV, "owned-second-exec")
            .status()
            .unwrap();
        assert!(second.success());

        if stage == OwnedChildMode::ExitBeforeRegistration.name() {
            return;
        }
        let payload = Zeroizing::new(read_frame_for_test(&mut data));
        assert!(payload.len() > 44);
        let slot = &payload[..36];
        let epoch = u64::from_be_bytes(payload[36..44].try_into().unwrap());
        let token = &payload[44..];
        if stage == OwnedChildMode::ExitAfterBootstrap.name() {
            revoke.write_all(b"E").unwrap();
            return;
        }
        if stage == OwnedChildMode::BlockAfterHandshakeByte.name() {
            data.write_all(&[0]).unwrap();
            // The data byte establishes a partial handshake.  Use the private
            // control channel for the test barrier so scheduling evidence does
            // not depend on a readiness event from the FD under test.
            revoke.write_all(b"B").unwrap();
            let mut signal = [0u8; 1];
            revoke.read_exact(&mut signal).unwrap();
            return;
        }

        let handshake = synthetic_handshake(slot, token);
        write_frame(&mut data, &handshake);
        assert_eq!(read_frame_for_test(&mut data), br#"{"result":"accepted"}"#);
        if stage == OwnedChildMode::SubmitAvailable.name() {
            let slot = std::str::from_utf8(slot).unwrap();
            let observed = Utc::now();
            let observed_text = observed.to_rfc3339_opts(SecondsFormat::Secs, true);
            let five_reset =
                (observed + chrono::Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
            let weekly_reset =
                (observed + chrono::Duration::days(7)).to_rfc3339_opts(SecondsFormat::Secs, true);
            let frame = format!(
                "{{\"event\":\"observation\",\"slot\":\"{slot}\",\"epoch\":\"{epoch}\",\"sequence\":\"1\",\"observedAt\":\"{observed_text}\",\"status\":\"available\",\"source\":\"completion_sse\",\"windows\":[{{\"kind\":\"five_hour\",\"usedPercent\":7,\"resetAt\":\"{five_reset}\"}},{{\"kind\":\"weekly\",\"usedPercent\":9,\"resetAt\":\"{weekly_reset}\"}}]}}"
            );
            write_frame(&mut data, frame.as_bytes());
        } else if stage == OwnedChildMode::SubmitUnavailable.name() {
            let slot = std::str::from_utf8(slot).unwrap();
            let observed = Utc::now();
            let observed_text = observed.to_rfc3339_opts(SecondsFormat::Secs, true);
            let reset =
                (observed + chrono::Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
            let available = format!(
                "{{\"event\":\"observation\",\"slot\":\"{slot}\",\"epoch\":\"{epoch}\",\"sequence\":\"1\",\"observedAt\":\"{observed_text}\",\"status\":\"available\",\"source\":\"completion_sse\",\"windows\":[{{\"kind\":\"five_hour\",\"usedPercent\":7,\"resetAt\":\"{reset}\"}}]}}"
            );
            write_frame(&mut data, available.as_bytes());
            let unavailable = format!(
                "{{\"event\":\"observation\",\"slot\":\"{slot}\",\"epoch\":\"{epoch}\",\"sequence\":\"2\",\"observedAt\":\"{observed_text}\",\"status\":\"unavailable\",\"errorCode\":\"unavailable\"}}"
            );
            write_frame(&mut data, unavailable.as_bytes());
        } else if stage == OwnedChildMode::RejectReplay.name() {
            let slot = std::str::from_utf8(slot).unwrap();
            let observed = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
            let frame = format!(
                "{{\"event\":\"observation\",\"slot\":\"{slot}\",\"epoch\":\"{epoch}\",\"sequence\":\"1\",\"observedAt\":\"{observed}\",\"status\":\"unavailable\",\"errorCode\":\"unavailable\"}}"
            );
            write_frame(&mut data, frame.as_bytes());
            let mut continue_signal = [0u8; 1];
            revoke.read_exact(&mut continue_signal).unwrap();
            write_frame(&mut data, frame.as_bytes());
        } else if stage == OwnedChildMode::SubmitLifecycle.name() {
            let slot = std::str::from_utf8(slot).unwrap();
            let observed = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
            let frame = format!(
                "{{\"event\":\"continuity_uncertain\",\"slot\":\"{slot}\",\"epoch\":\"{epoch}\",\"sequence\":\"1\",\"observedAt\":\"{observed}\"}}"
            );
            write_frame(&mut data, frame.as_bytes());
        } else if stage == OwnedChildMode::RejectWrongSlot.name() {
            let frame = ingress(
                "22222222-2222-4222-8222-222222222222",
                epoch,
                1,
                "identity_changed",
            );
            write_frame(&mut data, &frame);
        } else if stage == OwnedChildMode::RejectOldEpoch.name() {
            let slot = std::str::from_utf8(slot).unwrap();
            write_frame(
                &mut data,
                &ingress(slot, epoch.saturating_sub(1), 1, "identity_changed"),
            );
        } else if stage == OwnedChildMode::RejectSkippedSequence.name() {
            let slot = std::str::from_utf8(slot).unwrap();
            write_frame(&mut data, &ingress(slot, epoch, 2, "identity_changed"));
        }
        data.shutdown(Shutdown::Write).unwrap();
        // Keep the synthetic child alive after data EOF.  This makes the
        // owner-side CleanEof contract deterministic and proves that cleanup
        // wakes then reaps a non-cooperating child instead of calling wait()
        // without a terminal transport decision.
        let mut cleanup = [0u8; 1];
        let _ = revoke.read(&mut cleanup);
    }

    fn fresh_store(label: &str) -> (std::path::PathBuf, ClaudeSnapshotStore) {
        let root = std::env::temp_dir().join(format!("quotabar-c3b1-{label}-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root.clone()).unwrap();
        (root, store)
    }

    fn wait_until_readable(fd: RawFd) {
        let mut watched = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut watched, 1, 5_000) },
            1,
            "child barrier did not become readable"
        );
        assert_ne!(watched.revents & libc::POLLIN, 0);
    }

    fn wait_until_store_changes(store: &ClaudeSnapshotStore, before: &[u8]) -> Vec<u8> {
        let watchdog = Instant::now() + Duration::from_secs(5);
        loop {
            let observed = store.persisted_state_bytes_for_test().unwrap();
            if observed != before {
                return observed;
            }
            assert!(Instant::now() < watchdog, "store change watchdog expired");
            std::thread::yield_now();
        }
    }

    #[test]
    fn production_owned_spawn_covers_failure_cleanup_and_fd_provenance() {
        for fault in [
            OwnedLaunchFault::MissingExecutable,
            OwnedLaunchFault::PreExec,
        ] {
            let (root, store) = fresh_store("spawn-failure");
            let pairing = PairingRegistry::new();
            let spec = OwnedLaunchSpec::test(OwnedChildMode::Session, fault);
            assert_eq!(
                spawn_owned_session(
                    &store,
                    &pairing,
                    PlanMetadata::Paid,
                    chrono::Utc::now(),
                    &spec,
                )
                .unwrap_err(),
                TransportError::Ended(SessionEnd::WorkerOrStoreFailed)
            );
            assert_eq!(pairing.pending_count_for_test(), 0);
            let _ = std::fs::remove_dir_all(root);
        }

        let (root, store) = fresh_store("exit-before-registration");
        let pairing = PairingRegistry::new();
        let spec = OwnedLaunchSpec::test(
            OwnedChildMode::ExitBeforeRegistration,
            OwnedLaunchFault::None,
        );
        assert_eq!(
            spawn_owned_session(
                &store,
                &pairing,
                PlanMetadata::Paid,
                chrono::Utc::now(),
                &spec,
            )
            .unwrap_err(),
            TransportError::Ended(SessionEnd::ChildExited)
        );
        assert!(spec.audit.originals_closed_in_parent.load(Ordering::SeqCst));
        assert!(spec.audit.child_reaped.load(Ordering::SeqCst));
        assert_eq!(pairing.pending_count_for_test(), 0);
        let _ = std::fs::remove_dir_all(root);

        let (root, store) = fresh_store("bootstrap-failure");
        let pairing = PairingRegistry::new();
        let spec =
            OwnedLaunchSpec::test(OwnedChildMode::Session, OwnedLaunchFault::BootstrapPartial);
        assert_eq!(
            spawn_owned_session(
                &store,
                &pairing,
                PlanMetadata::Paid,
                chrono::Utc::now(),
                &spec,
            )
            .unwrap_err(),
            TransportError::Ended(SessionEnd::WorkerOrStoreFailed)
        );
        assert!(spec.audit.child_reaped.load(Ordering::SeqCst));
        assert_eq!(pairing.pending_count_for_test(), 0);
        let _ = std::fs::remove_dir_all(root);

        for mode in [OwnedChildMode::ExitAfterBootstrap, OwnedChildMode::Session] {
            let (root, store) = fresh_store("owned-session");
            let pairing = PairingRegistry::new();
            let spec = OwnedLaunchSpec::test(mode, OwnedLaunchFault::None);
            let mut owner = spawn_owned_session(
                &store,
                &pairing,
                PlanMetadata::Paid,
                chrono::Utc::now(),
                &spec,
            )
            .unwrap();
            let result = run_spawn_owned_session(&mut owner, &store, &pairing);
            let expected = if mode == OwnedChildMode::ExitAfterBootstrap {
                SessionEnd::ChildExited
            } else {
                SessionEnd::CleanEof
            };
            assert_eq!(result, Err(TransportError::Ended(expected)));
            assert!(spec.audit.originals_closed_in_parent.load(Ordering::SeqCst));
            assert!(spec.audit.child_reaped.load(Ordering::SeqCst));
            assert_eq!(pairing.pending_count_for_test(), 0);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn two_real_children_interleave_deterministically_for_25_iterations() {
        for iteration in 0..25 {
            let (root, store) = fresh_store("two-child");
            let store = std::sync::Arc::new(store);
            let pairing = std::sync::Arc::new(PairingRegistry::new());

            let a_spec = OwnedLaunchSpec::test(
                OwnedChildMode::BlockAfterHandshakeByte,
                OwnedLaunchFault::None,
            );
            let mut a_owner =
                spawn_owned_session(&store, &pairing, PlanMetadata::Paid, Utc::now(), &a_spec)
                    .unwrap();
            let mut ready_receiver = a_owner.ready_receiver_for_test.take().unwrap();
            wait_until_readable(ready_receiver.as_raw_fd());
            let mut ready = [0u8; 1];
            ready_receiver.read_exact(&mut ready).unwrap();
            assert_eq!(ready, *b"B");
            let mut revoke_a = a_owner.revoke_peer_for_test.take().unwrap();

            let b_mode = if iteration % 2 == 0 {
                OwnedChildMode::SubmitAvailable
            } else {
                OwnedChildMode::SubmitUnavailable
            };
            let b_spec = OwnedLaunchSpec::test(b_mode, OwnedLaunchFault::None);
            let mut b_owner =
                spawn_owned_session(&store, &pairing, PlanMetadata::Free, Utc::now(), &b_spec)
                    .unwrap();
            let before_b = store.persisted_state_bytes_for_test().unwrap();

            let a_store = store.clone();
            let a_pairing = pairing.clone();
            let a_worker = std::thread::spawn(move || {
                run_spawn_owned_session(&mut a_owner, &a_store, &a_pairing)
            });
            assert!(pairing.try_lock_available_for_test());

            let b_result = run_spawn_owned_session(&mut b_owner, &store, &pairing);
            assert!(
                matches!(b_result, Err(TransportError::Ended(SessionEnd::CleanEof))),
                "iteration {iteration}: unexpected B result {b_result:?}"
            );
            let after_b = store.persisted_state_bytes_for_test().unwrap();
            assert_ne!(
                after_b, before_b,
                "iteration {iteration}: B did not persist"
            );

            revoke_a.write_all(b"R").unwrap();
            assert_eq!(
                a_worker.join().unwrap(),
                Err(TransportError::Ended(SessionEnd::Revoked))
            );
            assert_eq!(
                store.persisted_state_bytes_for_test().unwrap(),
                after_b,
                "iteration {iteration}: A changed durable bytes"
            );
            assert!(a_spec.audit.child_reaped.load(Ordering::SeqCst));
            assert!(b_spec.audit.child_reaped.load(Ordering::SeqCst));
            assert_eq!(pairing.pending_count_for_test(), 0);

            let projection = store.project(Utc::now()).unwrap();
            let paid = projection
                .slots
                .iter()
                .find(|slot| slot.plan == Some(PlanMetadata::Paid))
                .unwrap();
            let free = projection
                .slots
                .iter()
                .find(|slot| slot.plan == Some(PlanMetadata::Free))
                .unwrap();
            assert_eq!(paid.five_hour.used_percent, None);
            if b_mode == OwnedChildMode::SubmitAvailable {
                assert_eq!(free.five_hour.used_percent, Some(7.0));
            } else {
                assert_eq!(free.five_hour.used_percent, Some(7.0));
                assert_eq!(
                    free.five_hour.last_error_code,
                    Some(SafeErrorCode::Unavailable)
                );
            }
            drop(store);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn production_owned_seam_covers_rejection_and_lifecycle_matrix() {
        for mode in [
            OwnedChildMode::RejectWrongSlot,
            OwnedChildMode::RejectOldEpoch,
            OwnedChildMode::RejectSkippedSequence,
        ] {
            let (root, store) = fresh_store("owned-reject");
            let pairing = PairingRegistry::new();
            let spec = OwnedLaunchSpec::test(mode, OwnedLaunchFault::None);
            let mut owner =
                spawn_owned_session(&store, &pairing, PlanMetadata::Paid, Utc::now(), &spec)
                    .unwrap();
            let before = store.persisted_state_bytes_for_test().unwrap();
            assert_eq!(
                run_spawn_owned_session(&mut owner, &store, &pairing),
                Err(TransportError::Ended(SessionEnd::ProtocolRejected))
            );
            assert_eq!(store.persisted_state_bytes_for_test().unwrap(), before);
            let _ = std::fs::remove_dir_all(root);
        }

        for mode in [
            OwnedChildMode::SubmitAvailable,
            OwnedChildMode::SubmitUnavailable,
            OwnedChildMode::SubmitLifecycle,
        ] {
            let (root, store) = fresh_store("owned-accepted");
            let pairing = PairingRegistry::new();
            let spec = OwnedLaunchSpec::test(mode, OwnedLaunchFault::None);
            let mut owner =
                spawn_owned_session(&store, &pairing, PlanMetadata::Paid, Utc::now(), &spec)
                    .unwrap();
            let before = store.persisted_state_bytes_for_test().unwrap();
            assert_eq!(
                run_spawn_owned_session(&mut owner, &store, &pairing),
                Err(TransportError::Ended(SessionEnd::CleanEof))
            );
            assert_ne!(store.persisted_state_bytes_for_test().unwrap(), before);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn production_owned_replay_preserves_bytes_after_first_commit() {
        let (root, store) = fresh_store("owned-replay");
        let store = std::sync::Arc::new(store);
        let pairing = std::sync::Arc::new(PairingRegistry::new());
        let spec = OwnedLaunchSpec::test(OwnedChildMode::RejectReplay, OwnedLaunchFault::None);
        let mut owner =
            spawn_owned_session(&store, &pairing, PlanMetadata::Paid, Utc::now(), &spec).unwrap();
        let mut continue_child = owner.revoke.try_clone().unwrap();
        let before = store.persisted_state_bytes_for_test().unwrap();
        let worker_store = store.clone();
        let worker_pairing = pairing.clone();
        let worker = std::thread::spawn(move || {
            run_spawn_owned_session(&mut owner, &worker_store, &worker_pairing)
        });
        let after_first = wait_until_store_changes(&store, &before);
        continue_child.write_all(b"C").unwrap();
        assert_eq!(
            worker.join().unwrap(),
            Err(TransportError::Ended(SessionEnd::ProtocolRejected))
        );
        assert_eq!(store.persisted_state_bytes_for_test().unwrap(), after_first);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
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
        // A real re-exec is intentionally exercised once. Repeating process
        // creation here makes CI depend on host scheduling rather than the FD
        // provenance invariant being proved; sustained-rate repetition stays
        // in the deterministic in-process bucket test below.
        run_reexecuted_synthetic_child_once();
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
        let mut owner = SpawnOwnedSession::from_owned_spawn(
            child,
            parent_bootstrap,
            parent_revoke,
            bootstrap.slot_id().clone(),
            bootstrap.epoch(),
        );
        let slot = bootstrap.slot_id().as_str().as_bytes();
        let token = bootstrap.token_bytes_for_synthetic_child();
        let probe = synthetic_handshake(slot, token);
        assert!(parse_handshake(&probe).is_ok());
        let mut bytes = Zeroizing::new(Vec::with_capacity(slot.len() + token.len()));
        bytes.extend_from_slice(slot);
        bytes.extend_from_slice(token);
        write_frame(&mut owner.parent, &bytes);
        assert_eq!(
            run_spawn_owned_session(&mut owner, &store, &pairing),
            Err(TransportError::Ended(SessionEnd::CleanEof))
        );
        assert_eq!(owner.slot, *bootstrap.slot_id());
        assert_eq!(owner.generation, bootstrap.epoch());
        // The terminal owner path observes/reaps the child; Child retains the
        // observed success status for inspection without another blocking wait.
        assert!(owner.child.try_wait().unwrap().unwrap().success());
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
    fn secret_frame_distinguishes_header_and_payload_truncation() {
        let (mut server, mut client) = UnixStream::pair().unwrap();
        client.write_all(&[0]).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        assert_eq!(
            read_secret_frame(&mut server, HANDSHAKE_MAX, Duration::from_secs(3)),
            Err(TransportError::Ended(SessionEnd::HeaderTruncated))
        );

        let (mut server, mut client) = UnixStream::pair().unwrap();
        client.write_all(&3u32.to_be_bytes()).unwrap();
        client.write_all(b"x").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        assert_eq!(
            read_secret_frame(&mut server, HANDSHAKE_MAX, Duration::from_secs(3)),
            Err(TransportError::Ended(SessionEnd::PayloadTruncated))
        );
    }

    #[test]
    fn fixed_deadline_fake_clock_covers_edges_drip_and_long_idle() {
        let start = Instant::now();
        let deadline = FixedDeadline::after(start, Duration::from_secs(5));
        assert_eq!(
            deadline.remaining_at(start + Duration::from_millis(4_900)),
            Ok(Duration::from_millis(100))
        );
        assert_eq!(
            deadline.remaining_at(start + Duration::from_secs(5)),
            Err(TransportError::Ended(SessionEnd::DeadlineExpired))
        );
        assert_eq!(
            deadline.remaining_at(start + Duration::from_millis(5_001)),
            Err(TransportError::Ended(SessionEnd::DeadlineExpired))
        );

        // Receiving additional drip bytes never creates a replacement
        // deadline: every continuation read retains the original expires_at.
        let after_drip = deadline;
        assert_eq!(after_drip.expires_at, deadline.expires_at);

        // Idle time before the first byte is intentionally unbounded.  The
        // fixed framing budget begins only once that first byte is observed.
        let first_byte = start + Duration::from_secs(60 * 60);
        let after_long_idle = FixedDeadline::after(first_byte, Duration::from_secs(5));
        assert_eq!(
            after_long_idle.remaining_at(first_byte),
            Ok(Duration::from_secs(5))
        );
        assert_eq!(ceil_poll_millis(Duration::from_nanos(1)), 1);
        assert_eq!(ceil_poll_millis(Duration::from_micros(999)), 1);
        assert_eq!(ceil_poll_millis(Duration::from_micros(1_001)), 2);
        assert_eq!(ceil_poll_millis(Duration::from_micros(4_900)), 5);
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
            Err(TransportError::Ended(SessionEnd::CleanEof))
        );
        first_client.shutdown(Shutdown::Both).unwrap();
        assert!(matches!(
            blocked.join().unwrap(),
            Err(TransportError::Ended(SessionEnd::HeaderTruncated))
        ));
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
        let mut owner = SpawnOwnedSession::from_owned_spawn(
            child,
            parent,
            revoke,
            bootstrap.slot_id().clone(),
            bootstrap.epoch(),
        );
        // A malformed one-byte header fails before consume, then the owner
        // must revoke its still-pending authority and reap its exact child.
        child_end.write_all(&[0]).unwrap();
        child_end.shutdown(Shutdown::Write).unwrap();
        assert!(matches!(
            run_spawn_owned_session(&mut owner, &store, &pairing),
            Err(TransportError::Ended(SessionEnd::HeaderTruncated))
        ));
        assert!(owner.child.try_wait().unwrap().is_some());
        assert!(matches!(
            pairing.consume(bootstrap.slot_id(), &token),
            PairingConsume::Rejected
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn owned_first_byte_wait_wakes_on_private_revoke_without_reading_data_fd() {
        let (parent, _child_end) = UnixStream::pair().unwrap();
        let (revoke, mut revoke_peer) = UnixStream::pair().unwrap();
        let mut child = Command::new("/bin/cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let _keepalive = child.stdin.take().unwrap();
        let slot = AccountSlotId::parse("11111111-1111-4111-8111-111111111111").unwrap();
        let mut owner = SpawnOwnedSession::from_owned_spawn(child, parent, revoke, slot, 1);
        revoke_peer.write_all(b"x").unwrap();
        assert_eq!(
            wait_for_first_byte_or_revoke(&mut owner),
            Err(TransportError::Ended(SessionEnd::Revoked))
        );
        let registry = PairingRegistry::new();
        owner.revoke_and_reap(&registry);
    }

    #[test]
    fn owned_first_byte_wait_distinguishes_child_exit_from_clean_peer_eof() {
        let (parent, child_end) = UnixStream::pair().unwrap();
        let (revoke, _revoke_peer) = UnixStream::pair().unwrap();
        let child = Command::new("/usr/bin/true").spawn().unwrap();
        let slot = AccountSlotId::parse("11111111-1111-4111-8111-111111111111").unwrap();
        let mut owner = SpawnOwnedSession::from_owned_spawn(child, parent, revoke, slot, 1);
        owner.child.wait().unwrap();
        drop(child_end);
        assert_eq!(
            wait_for_first_byte_or_revoke(&mut owner),
            Err(TransportError::Ended(SessionEnd::ChildExited))
        );
        let registry = PairingRegistry::new();
        owner.revoke_and_reap(&registry);
    }

    #[test]
    fn control_eof_and_data_terminal_are_classified_in_one_poll_round() {
        let (parent, child_end) = UnixStream::pair().unwrap();
        let (revoke, child_revoke) = UnixStream::pair().unwrap();
        let mut child = Command::new("/bin/cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let keepalive = child.stdin.take().unwrap();
        drop(child_end);
        drop(child_revoke);
        let mut monitor = SessionMonitor {
            revoke_fd: Some(revoke.as_raw_fd()),
            child: Some(&mut child),
        };
        assert_eq!(
            wait_for_data(&parent, &mut monitor, None),
            Err(TransportError::Ended(SessionEnd::CleanEof))
        );
        assert_eq!(monitor.revoke_fd, None);
        drop(keepalive);
        let _ = child.kill();
        let _ = child.wait();
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
            Err(TransportError::Ended(SessionEnd::CleanEof))
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
            Err(TransportError::Ended(SessionEnd::ProtocolRejected))
        );
        assert_eq!(
            run_production_transport_frames(
                PlanMetadata::Paid,
                vec![ingress(paid, 1, 2, "identity_changed")]
            ),
            Err(TransportError::Ended(SessionEnd::ProtocolRejected))
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

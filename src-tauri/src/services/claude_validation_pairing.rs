//! Foreground-only, memory-only pairing for the C3-B1A synthetic boundary.
//!
//! This module has no Tauri command, filesystem persistence, child launcher,
//! provider, or credential access.  A later separately gated launcher may use
//! the crate-private bootstrap object; B1A tests use only a synthetic child.

use super::claude_snapshot::{
    AccountSlotId, BindingCapability, ClaudeSnapshotStore, PlanMetadata, SnapshotError,
    ValidationBindingIssuance,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};
use std::sync::Mutex;
use std::time::{Duration as MonotonicDuration, Instant};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

const PAID_SLOT: &str = "11111111-1111-4111-8111-111111111111";
const FREE_SLOT: &str = "22222222-2222-4222-8222-222222222222";
const TOKEN_BYTES: usize = 32;
const TOKEN_BASE64_BYTES: usize = 43;
const TOKEN_LIFETIME: MonotonicDuration = MonotonicDuration::from_secs(15 * 60);
const MAX_SLOTS: usize = 2;

/// Owned authority detached from the shared pairing table after a successful
/// one-shot consume.  Frame I/O must use this value, never hold table state.
pub(crate) struct SessionAuthority {
    pub(crate) slot_id: AccountSlotId,
    pub(crate) capability: BindingCapability,
    pub(crate) epoch: u64,
    pub(crate) plan: PlanMetadata,
}

impl fmt::Debug for SessionAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionAuthority(<redacted>)")
    }
}

#[derive(Debug)]
pub(crate) enum PairingConsume {
    Accepted(SessionAuthority),
    Rejected,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairingError {
    Rejected,
    Expired,
    Random,
    Store,
}

impl From<SnapshotError> for PairingError {
    fn from(_: SnapshotError) -> Self {
        Self::Store
    }
}

/// Secret bytes never implement Debug, Display, or serde traits.
struct TransportSecret([u8; TOKEN_BYTES]);

/// Canonical unpadded base64url transport authority.  This representation is
/// never a String and is wiped with the underlying random secret.
struct BootstrapToken([u8; TOKEN_BASE64_BYTES]);

impl TransportSecret {
    fn generate() -> Result<Self, PairingError> {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| PairingError::Random)?;
        Ok(Self(bytes))
    }

    fn canonical_bootstrap(&self) -> Result<BootstrapToken, PairingError> {
        let mut encoded = BootstrapToken([0; TOKEN_BASE64_BYTES]);
        URL_SAFE_NO_PAD
            .encode_slice(self.0, &mut encoded.0)
            .map_err(|_| PairingError::Rejected)?;
        Ok(encoded)
    }

    fn matches(&self, candidate: &[u8]) -> bool {
        candidate.len() == TOKEN_BYTES && self.0.ct_eq(candidate).into()
    }
}

impl Drop for TransportSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl BootstrapToken {
    fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    fn decode(&self) -> Result<Zeroizing<Vec<u8>>, PairingError> {
        let mut decoded = Zeroizing::new(vec![0; TOKEN_BYTES]);
        let written = URL_SAFE_NO_PAD
            .decode_slice(self.as_bytes(), &mut decoded)
            .map_err(|_| PairingError::Rejected)?;
        if written != TOKEN_BYTES {
            return Err(PairingError::Rejected);
        }
        Ok(decoded)
    }
}

impl Drop for BootstrapToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Never serializable; only a synthetic bootstrap FD test may read its
/// canonical base64url representation before it is consumed by the child.
pub(crate) struct PairingBootstrapV1 {
    token: BootstrapToken,
    slot_id: AccountSlotId,
    epoch: u64,
}

impl fmt::Debug for PairingBootstrapV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingBootstrapV1(<redacted>)")
    }
}

impl PairingBootstrapV1 {
    /// Delivers the bootstrap authority directly to an already-owned private
    /// descriptor without exposing a production token accessor.
    pub(crate) fn write_private_payload(&self, writer: &mut impl Write) -> io::Result<()> {
        let slot = self.slot_id.as_str().as_bytes();
        let length = slot
            .len()
            .checked_add(std::mem::size_of::<u64>())
            .and_then(|length| length.checked_add(self.token.as_bytes().len()))
            .and_then(|length| u32::try_from(length).ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bootstrap overflow"))?;
        writer.write_all(&length.to_be_bytes())?;
        writer.write_all(slot)?;
        writer.write_all(&self.epoch.to_be_bytes())?;
        writer.write_all(self.token.as_bytes())
    }

    #[cfg(test)]
    pub(crate) fn token_bytes_for_synthetic_child(&self) -> &[u8] {
        self.token.as_bytes()
    }

    #[cfg(test)]
    pub(crate) fn decoded_token_for_test(&self) -> Zeroizing<Vec<u8>> {
        self.token
            .decode()
            .expect("generated bootstrap token is canonical")
    }

    pub(crate) fn slot_id(&self) -> &AccountSlotId {
        &self.slot_id
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }
}

struct Session {
    token: TransportSecret,
    epoch: u64,
    capability: BindingCapability,
    plan: PlanMetadata,
    expires_at: Instant,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Session(<redacted>)")
    }
}

/// In-memory table keyed by stable slot, not by user identity or provider data.
struct PairingTable {
    sessions: HashMap<AccountSlotId, Session>,
}

impl PairingTable {
    pub(crate) fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    pub(crate) fn stable_slot(plan: PlanMetadata) -> AccountSlotId {
        let value = match plan {
            PlanMetadata::Paid => PAID_SLOT,
            PlanMetadata::Free => FREE_SLOT,
        };
        AccountSlotId::parse(value).expect("fixed C3-B1A slot UUID")
    }

    /// Issues a single-use in-memory transport session.  The child identity
    /// boundary is private FD delivery by the spawn owner; socket peer
    /// credentials are intentionally not treated as the later writer PID.
    fn insert(
        &mut self,
        slot_id: AccountSlotId,
        issuance: ValidationBindingIssuance,
        plan: PlanMetadata,
        now: DateTime<Utc>,
    ) -> Result<PairingBootstrapV1, PairingError> {
        if self.sessions.len() >= MAX_SLOTS && !self.sessions.contains_key(&slot_id) {
            return Err(PairingError::Rejected);
        }
        let token = TransportSecret::generate()?;
        let _ = now;
        let bootstrap = PairingBootstrapV1 {
            token: token.canonical_bootstrap()?,
            slot_id: slot_id.clone(),
            epoch: issuance.epoch(),
        };
        self.sessions.insert(
            slot_id.clone(),
            Session {
                token,
                epoch: issuance.epoch(),
                capability: issuance.capability(),
                plan,
                expires_at: Instant::now() + TOKEN_LIFETIME,
            },
        );
        Ok(bootstrap)
    }

    fn consume(&mut self, slot_id: &AccountSlotId, token: &[u8]) -> PairingConsume {
        let Some(session) = self.sessions.get(slot_id) else {
            return PairingConsume::Rejected;
        };
        if Instant::now() > session.expires_at {
            self.sessions.remove(slot_id);
            return PairingConsume::Expired;
        }
        if !session.token.matches(token) {
            return PairingConsume::Rejected;
        }
        let session = self
            .sessions
            .remove(slot_id)
            .expect("checked pairing session remains present");
        PairingConsume::Accepted(SessionAuthority {
            slot_id: slot_id.clone(),
            capability: session.capability,
            epoch: session.epoch,
            plan: session.plan,
        })
    }
}

/// Owns the only pairing lock.  Its public crate API returns owned authority
/// and never exposes a guard, table reference, or callback under the lock.
pub(crate) struct PairingRegistry {
    sessions: Mutex<PairingTable>,
}

impl PairingRegistry {
    pub(crate) fn new() -> Self {
        Self {
            sessions: Mutex::new(PairingTable::new()),
        }
    }

    pub(crate) fn stable_slot(plan: PlanMetadata) -> AccountSlotId {
        PairingTable::stable_slot(plan)
    }

    /// Durable store work deliberately precedes the brief in-memory insertion.
    pub(crate) fn register_or_rebind(
        &self,
        store: &ClaudeSnapshotStore,
        plan: PlanMetadata,
        now: DateTime<Utc>,
    ) -> Result<PairingBootstrapV1, PairingError> {
        let slot_id = Self::stable_slot(plan);
        let issuance = store.register_or_rebind_validation_slot(
            slot_id.clone(),
            match plan {
                PlanMetadata::Paid => "paid".into(),
                PlanMetadata::Free => "free".into(),
            },
            plan,
            now,
        )?;
        self.sessions
            .lock()
            .map_err(|_| PairingError::Rejected)?
            .insert(slot_id, issuance, plan, now)
    }

    /// Atomically consumes after parsing, with no stream or store I/O held.
    pub(crate) fn consume(&self, slot_id: &AccountSlotId, token: &[u8]) -> PairingConsume {
        self.sessions
            .lock()
            .map(|mut table| table.consume(slot_id, token))
            .unwrap_or(PairingConsume::Rejected)
    }

    pub(crate) fn clear(&self) {
        if let Ok(mut table) = self.sessions.lock() {
            table.sessions.clear();
        }
    }

    /// Revokes only the generation held by a failed spawn owner. A newer
    /// foreground rebind for the same stable slot must remain untouched.
    pub(crate) fn revoke(&self, slot_id: &AccountSlotId, epoch: u64) {
        if let Ok(mut table) = self.sessions.lock() {
            if table
                .sessions
                .get(slot_id)
                .is_some_and(|session| session.epoch == epoch)
            {
                table.sessions.remove(slot_id);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn try_lock_available_for_test(&self) -> bool {
        self.sessions.try_lock().is_ok()
    }

    #[cfg(test)]
    pub(crate) fn pending_count_for_test(&self) -> usize {
        self.sessions
            .lock()
            .map(|table| table.sessions.len())
            .unwrap_or(usize::MAX)
    }

    #[cfg(test)]
    fn expire_for_test(&self, slot_id: &AccountSlotId) {
        if let Ok(mut table) = self.sessions.lock() {
            if let Some(session) = table.sessions.get_mut(slot_id) {
                session.expires_at = Instant::now() - MonotonicDuration::from_secs(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }
    fn store() -> ClaudeSnapshotStore {
        ClaudeSnapshotStore::at_root(
            std::env::temp_dir().join(format!("quotabar-c3b1-pair-{}", Uuid::new_v4())),
        )
        .unwrap()
    }

    #[test]
    fn token_is_canonical_single_use_and_bound_to_private_delivery() {
        let store = store();
        let table = PairingRegistry::new();
        let bootstrap = table
            .register_or_rebind(&store, PlanMetadata::Paid, now())
            .unwrap();
        let token = bootstrap.token.decode().unwrap();
        assert_eq!(token.len(), TOKEN_BYTES);
        let wrong = [0u8; TOKEN_BYTES];
        assert!(matches!(
            table.consume(bootstrap.slot_id(), &wrong),
            PairingConsume::Rejected
        ));
        assert!(matches!(
            table.consume(bootstrap.slot_id(), &token),
            PairingConsume::Accepted(_)
        ));
        assert!(!format!("{bootstrap:?}").contains("token"));
    }

    #[test]
    fn expiry_and_restart_fail_closed() {
        let store = store();
        let table = PairingRegistry::new();
        let bootstrap = table
            .register_or_rebind(&store, PlanMetadata::Free, now())
            .unwrap();
        let token = bootstrap.token.decode().unwrap();
        table.expire_for_test(bootstrap.slot_id());
        assert!(matches!(
            table.consume(bootstrap.slot_id(), &token),
            PairingConsume::Expired
        ));
        table.clear();
        assert!(matches!(
            table.consume(bootstrap.slot_id(), &token),
            PairingConsume::Rejected
        ));
    }

    #[test]
    fn durable_rebind_after_restart_rejects_old_authority() {
        let root = std::env::temp_dir().join(format!("quotabar-c3b1-restart-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root.clone()).unwrap();
        let first = PairingRegistry::new();
        let first_bootstrap = first
            .register_or_rebind(&store, PlanMetadata::Paid, now())
            .unwrap();
        let first_token = first_bootstrap.token.decode().unwrap();
        let old_authority = match first.consume(first_bootstrap.slot_id(), &first_token) {
            PairingConsume::Accepted(authority) => authority,
            _ => panic!("first authority must be accepted"),
        };
        drop(first);
        drop(store);

        let resumed_store = ClaudeSnapshotStore::at_root(root).unwrap();
        let resumed = PairingRegistry::new();
        let fresh_bootstrap = resumed
            .register_or_rebind(&resumed_store, PlanMetadata::Paid, now())
            .unwrap();
        assert!(fresh_bootstrap.epoch() > old_authority.epoch);
        assert!(matches!(
            resumed.consume(first_bootstrap.slot_id(), &first_token),
            PairingConsume::Rejected
        ));
        let fresh_token = fresh_bootstrap.token.decode().unwrap();
        assert!(matches!(
            resumed.consume(fresh_bootstrap.slot_id(), &fresh_token),
            PairingConsume::Accepted(_)
        ));
        assert!(resumed_store
            .apply_correlated_lifecycle_transition(
                &old_authority.capability,
                &old_authority.slot_id,
                old_authority.epoch,
                1,
                now(),
            )
            .is_err());
    }

    #[test]
    fn paid_and_free_sessions_consume_independently() {
        let store = store();
        let table = std::sync::Arc::new(PairingRegistry::new());
        let paid = {
            table
                .register_or_rebind(&store, PlanMetadata::Paid, now())
                .unwrap()
        };
        let free = {
            table
                .register_or_rebind(&store, PlanMetadata::Free, now())
                .unwrap()
        };
        let paid_slot = paid.slot_id().clone();
        let paid_token = paid.token.decode().unwrap();
        let free_slot = free.slot_id().clone();
        let free_token = free.token.decode().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let paid_table = table.clone();
        let paid_barrier = barrier.clone();
        let paid_worker = std::thread::spawn(move || {
            let authority = match paid_table.consume(&paid_slot, &paid_token) {
                PairingConsume::Accepted(authority) => authority,
                _ => panic!("paid pairing must be independent"),
            };
            paid_barrier.wait();
            authority
        });
        let free_table = table.clone();
        let free_barrier = barrier.clone();
        let free_worker = std::thread::spawn(move || {
            let authority = match free_table.consume(&free_slot, &free_token) {
                PairingConsume::Accepted(authority) => authority,
                _ => panic!("free pairing must be independent"),
            };
            free_barrier.wait();
            authority
        });
        barrier.wait();
        let paid = paid_worker.join().unwrap();
        let free = free_worker.join().unwrap();
        assert_ne!(paid.slot_id, free.slot_id);
        assert_ne!(paid.epoch, 0);
        assert_ne!(free.epoch, 0);
    }

    #[test]
    fn revocation_is_exact_to_the_failed_generation() {
        let store = store();
        let registry = PairingRegistry::new();
        let old = registry
            .register_or_rebind(&store, PlanMetadata::Paid, now())
            .unwrap();
        let fresh = registry
            .register_or_rebind(&store, PlanMetadata::Paid, now())
            .unwrap();
        assert!(fresh.epoch() > old.epoch());
        registry.revoke(old.slot_id(), old.epoch());
        assert!(matches!(
            registry.consume(fresh.slot_id(), &fresh.decoded_token_for_test()),
            PairingConsume::Accepted(_)
        ));
    }
}

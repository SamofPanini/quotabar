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
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use std::fmt;
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

const PAID_SLOT: &str = "11111111-1111-4111-8111-111111111111";
const FREE_SLOT: &str = "22222222-2222-4222-8222-222222222222";
const TOKEN_BYTES: usize = 32;
const TOKEN_LIFETIME: Duration = Duration::minutes(15);
const MAX_SLOTS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PairingResult {
    Accepted,
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

impl TransportSecret {
    fn generate() -> Result<Self, PairingError> {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| PairingError::Random)?;
        Ok(Self(bytes))
    }

    fn canonical_bootstrap(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
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

/// Never serializable; only a synthetic bootstrap FD test may read its
/// canonical base64url representation before it is consumed by the child.
pub(crate) struct PairingBootstrapV1 {
    token: TransportSecret,
    slot_id: AccountSlotId,
    epoch: u64,
    expires_at: DateTime<Utc>,
}

impl fmt::Debug for PairingBootstrapV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingBootstrapV1(<redacted>)")
    }
}

impl PairingBootstrapV1 {
    pub(crate) fn canonical_token_for_synthetic_child(&self) -> String {
        self.token.canonical_bootstrap()
    }

    pub(crate) fn slot_id(&self) -> &AccountSlotId {
        &self.slot_id
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

struct Session {
    token: TransportSecret,
    slot_id: AccountSlotId,
    epoch: u64,
    capability: BindingCapability,
    expected_uid: u32,
    expected_pid: u32,
    expires_at: DateTime<Utc>,
    consumed: bool,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Session(<redacted>)")
    }
}

/// In-memory table keyed by stable slot, not by user identity or provider data.
pub(crate) struct PairingTable {
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

    /// Issues a single-use in-memory transport session after the caller's
    /// foreground action has supplied the exact synthetic child PID.
    pub(crate) fn register_or_rebind(
        &mut self,
        store: &ClaudeSnapshotStore,
        plan: PlanMetadata,
        expected_uid: u32,
        expected_pid: u32,
        now: DateTime<Utc>,
    ) -> Result<PairingBootstrapV1, PairingError> {
        if expected_pid == 0
            || self.sessions.len() >= MAX_SLOTS
                && !self.sessions.contains_key(&Self::stable_slot(plan))
        {
            return Err(PairingError::Rejected);
        }
        let slot_id = Self::stable_slot(plan);
        let issuance = if self.sessions.contains_key(&slot_id) {
            store.rebind_validation_slot(&slot_id, now)?
        } else {
            store.register_validation_slot(
                slot_id.clone(),
                match plan {
                    PlanMetadata::Paid => "paid".into(),
                    PlanMetadata::Free => "free".into(),
                },
                Some(plan),
                now,
            )?
        };
        self.insert(slot_id, issuance, expected_uid, expected_pid, now)
    }

    fn insert(
        &mut self,
        slot_id: AccountSlotId,
        issuance: ValidationBindingIssuance,
        expected_uid: u32,
        expected_pid: u32,
        now: DateTime<Utc>,
    ) -> Result<PairingBootstrapV1, PairingError> {
        let token = TransportSecret::generate()?;
        let expires_at = now + TOKEN_LIFETIME;
        let bootstrap = PairingBootstrapV1 {
            token: TransportSecret::from_bytes_for_clone(&token),
            slot_id: slot_id.clone(),
            epoch: issuance.epoch(),
            expires_at,
        };
        self.sessions.insert(
            slot_id.clone(),
            Session {
                token,
                slot_id,
                epoch: issuance.epoch(),
                capability: issuance.capability(),
                expected_uid,
                expected_pid,
                expires_at,
                consumed: false,
            },
        );
        Ok(bootstrap)
    }

    /// Atomically consumes a session only after transport peer checks pass.
    pub(crate) fn consume(
        &mut self,
        slot_id: &AccountSlotId,
        token: &[u8],
        peer_uid: u32,
        peer_pid: u32,
        now: DateTime<Utc>,
    ) -> PairingResult {
        let Some(session) = self.sessions.get_mut(slot_id) else {
            return PairingResult::Rejected;
        };
        if now > session.expires_at {
            return PairingResult::Expired;
        }
        if session.consumed
            || peer_uid != session.expected_uid
            || peer_pid != session.expected_pid
            || !session.token.matches(token)
        {
            return PairingResult::Rejected;
        }
        session.consumed = true;
        PairingResult::Accepted
    }

    pub(crate) fn session_authority(
        &self,
        slot_id: &AccountSlotId,
    ) -> Option<(BindingCapability, u64)> {
        self.sessions
            .get(slot_id)
            .filter(|session| session.consumed)
            .map(|session| (session.capability.clone(), session.epoch))
    }

    pub(crate) fn clear(&mut self) {
        self.sessions.clear();
    }
}

impl TransportSecret {
    fn from_bytes_for_clone(secret: &TransportSecret) -> Self {
        Self(secret.0)
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
    fn token_is_canonical_single_use_and_bound_to_peer() {
        let store = store();
        let mut table = PairingTable::new();
        let bootstrap = table
            .register_or_rebind(&store, PlanMetadata::Paid, 501, 77, now())
            .unwrap();
        let token = URL_SAFE_NO_PAD
            .decode(bootstrap.canonical_token_for_synthetic_child())
            .unwrap();
        assert_eq!(token.len(), TOKEN_BYTES);
        assert_eq!(
            table.consume(bootstrap.slot_id(), &token, 501, 78, now()),
            PairingResult::Rejected
        );
        assert_eq!(
            table.consume(bootstrap.slot_id(), &token, 501, 77, now()),
            PairingResult::Accepted
        );
        assert_eq!(
            table.consume(bootstrap.slot_id(), &token, 501, 77, now()),
            PairingResult::Rejected
        );
        assert!(table.session_authority(bootstrap.slot_id()).is_some());
        assert!(
            !format!("{bootstrap:?}").contains(&bootstrap.canonical_token_for_synthetic_child())
        );
    }

    #[test]
    fn expiry_and_restart_fail_closed() {
        let store = store();
        let mut table = PairingTable::new();
        let bootstrap = table
            .register_or_rebind(&store, PlanMetadata::Free, 501, 88, now())
            .unwrap();
        let token = URL_SAFE_NO_PAD
            .decode(bootstrap.canonical_token_for_synthetic_child())
            .unwrap();
        assert_eq!(
            table.consume(
                bootstrap.slot_id(),
                &token,
                501,
                88,
                bootstrap.expires_at() + Duration::seconds(1)
            ),
            PairingResult::Expired
        );
        table.clear();
        assert_eq!(
            table.consume(bootstrap.slot_id(), &token, 501, 88, now()),
            PairingResult::Rejected
        );
    }
}

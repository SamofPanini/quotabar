//! Synthetic-only adapter contract for the current-only Claude snapshot store.
//!
//! This module accepts no provider payload, identity, path, or credential data.
//! It is deliberately internal: callers can only submit a fixed, sanitized
//! observation or request an explicit fail-closed lifecycle transition.

use super::claude_snapshot::{
    AccountSlotId, BindingCapability, ClaudeSnapshotStore, ObservationEnvelopeV1,
    ObservationStatus, ObservationWindow, PlanMetadata, SafeErrorCode, SnapshotError, SourceClass,
    WindowKind,
};
use chrono::{DateTime, Utc};

#[derive(Clone, Debug)]
pub(crate) struct SyntheticWindow {
    pub(crate) kind: WindowKind,
    pub(crate) used_percent: f64,
    pub(crate) reset_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub(crate) enum SyntheticDisposition {
    Available { windows: Vec<SyntheticWindow> },
    Unavailable { error: SafeErrorCode },
    ContinuityUncertain,
    IdentityChanged,
}

#[derive(Clone, Debug)]
pub(crate) struct SyntheticDesktopObservation {
    pub(crate) slot_id: AccountSlotId,
    pub(crate) capability: BindingCapability,
    pub(crate) binding_epoch: u64,
    pub(crate) sequence: u64,
    pub(crate) observed_at: DateTime<Utc>,
    pub(crate) plan: Option<PlanMetadata>,
    pub(crate) disposition: SyntheticDisposition,
}

/// Normalizes only fixed safe fields then delegates all durable-state policy to C3-A.
pub(crate) fn submit_synthetic_observation(
    store: &ClaudeSnapshotStore,
    input: SyntheticDesktopObservation,
    received_at: DateTime<Utc>,
) -> Result<(), SnapshotError> {
    match input.disposition {
        SyntheticDisposition::ContinuityUncertain | SyntheticDisposition::IdentityChanged => {
            store.mark_unverified(&input.slot_id, received_at)
        }
        SyntheticDisposition::Available { windows } => store.apply_correlated_observation(
            &input.capability,
            ObservationEnvelopeV1 {
                slot_id: input.slot_id,
                binding_epoch: input.binding_epoch,
                sequence: input.sequence,
                observed_at: input.observed_at,
                status: ObservationStatus::Available,
                source: Some(SourceClass::SyntheticFixture),
                windows: windows
                    .into_iter()
                    .map(|window| ObservationWindow {
                        kind: window.kind,
                        used_percent: window.used_percent,
                        reset_at: window.reset_at,
                    })
                    .collect(),
                error_code: None,
            },
            input.plan,
            received_at,
        ),
        SyntheticDisposition::Unavailable { error } => store.apply_correlated_observation(
            &input.capability,
            ObservationEnvelopeV1 {
                slot_id: input.slot_id,
                binding_epoch: input.binding_epoch,
                sequence: input.sequence,
                observed_at: input.observed_at,
                status: ObservationStatus::Unavailable,
                source: None,
                windows: vec![],
                error_code: Some(error),
            },
            input.plan,
            received_at,
        ),
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    use chrono::Duration;
    use uuid::Uuid;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }
    fn slot(value: &str) -> AccountSlotId {
        AccountSlotId::parse(value).unwrap()
    }
    fn observation(
        slot_id: AccountSlotId,
        capability: BindingCapability,
        sequence: u64,
    ) -> SyntheticDesktopObservation {
        SyntheticDesktopObservation {
            slot_id,
            capability,
            binding_epoch: 1,
            sequence,
            observed_at: now(),
            plan: Some(PlanMetadata::Paid),
            disposition: SyntheticDisposition::Available {
                windows: vec![SyntheticWindow {
                    kind: WindowKind::Weekly,
                    used_percent: 34.0,
                    reset_at: Some(now() + Duration::hours(1)),
                }],
            },
        }
    }

    #[test]
    fn issued_capability_is_required_and_redacted() {
        let root = std::env::temp_dir().join(format!("quotabar-c3b0-cap-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root).unwrap();
        let a = slot("00000000-0000-4000-8000-000000000001");
        let b = slot("00000000-0000-4000-8000-000000000002");
        let a_capability = store
            .register_slot(a.clone(), "a".into(), None, now())
            .unwrap();
        let b_capability = store
            .register_slot(b.clone(), "b".into(), None, now())
            .unwrap();
        let before = serde_json::to_vec(&store.project(now()).unwrap()).unwrap();
        assert!(submit_synthetic_observation(
            &store,
            observation(a.clone(), b_capability, 1),
            now()
        )
        .is_err());
        assert_eq!(
            serde_json::to_vec(&store.project(now()).unwrap()).unwrap(),
            before
        );
        assert_eq!(format!("{a_capability:?}"), "BindingCapability(<redacted>)");
        submit_synthetic_observation(&store, observation(a, a_capability, 1), now()).unwrap();
    }

    #[test]
    fn rebind_invalidates_old_capability() {
        let root = std::env::temp_dir().join(format!("quotabar-c3b0-rebind-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root).unwrap();
        let slot_id = slot("00000000-0000-4000-8000-000000000003");
        let old = store
            .register_slot(slot_id.clone(), "safe".into(), None, now())
            .unwrap();
        store.mark_unverified(&slot_id, now()).unwrap();
        let new = store.rebind(&slot_id, now()).unwrap();
        let mut old_input = observation(slot_id.clone(), old, 1);
        old_input.binding_epoch = 2;
        assert!(submit_synthetic_observation(&store, old_input, now()).is_err());
        let mut new_input = observation(slot_id, new, 1);
        new_input.binding_epoch = 2;
        submit_synthetic_observation(&store, new_input, now()).unwrap();
    }

    #[test]
    fn unpair_invalidates_capability_without_mutating_state() {
        let root = std::env::temp_dir().join(format!("quotabar-c3b0-unpair-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root).unwrap();
        let slot_id = slot("00000000-0000-4000-8000-000000000004");
        let capability = store
            .register_slot(slot_id.clone(), "safe".into(), None, now())
            .unwrap();
        store.unpair(&slot_id, now()).unwrap();
        let before = serde_json::to_vec(&store.project(now()).unwrap()).unwrap();
        assert!(
            submit_synthetic_observation(&store, observation(slot_id, capability, 1), now())
                .is_err()
        );
        assert_eq!(
            serde_json::to_vec(&store.project(now()).unwrap()).unwrap(),
            before
        );
    }

    #[test]
    fn uncertainty_invalidates_capability() {
        let root =
            std::env::temp_dir().join(format!("quotabar-c3b0-unverified-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root).unwrap();
        let slot_id = slot("00000000-0000-4000-8000-000000000005");
        let capability = store
            .register_slot(slot_id.clone(), "safe".into(), None, now())
            .unwrap();
        store.mark_unverified(&slot_id, now()).unwrap();
        assert!(
            submit_synthetic_observation(&store, observation(slot_id, capability, 1), now())
                .is_err()
        );
    }

    #[test]
    fn two_slots_remain_independent_when_one_is_rejected() {
        let root = std::env::temp_dir().join(format!("quotabar-c3b0-slots-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root).unwrap();
        let a = slot("00000000-0000-4000-8000-000000000006");
        let b = slot("00000000-0000-4000-8000-000000000007");
        let a_capability = store
            .register_slot(a.clone(), "a".into(), None, now())
            .unwrap();
        let b_capability = store
            .register_slot(b.clone(), "b".into(), None, now())
            .unwrap();
        submit_synthetic_observation(
            &store,
            observation(a.clone(), a_capability.clone(), 1),
            now(),
        )
        .unwrap();
        submit_synthetic_observation(&store, observation(b, b_capability, 1), now()).unwrap();
        assert!(
            submit_synthetic_observation(&store, observation(a, a_capability, 1), now()).is_err()
        );
        assert_eq!(
            store.project(now()).unwrap().slots[1].weekly.used_percent,
            Some(34.0)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use uuid::Uuid;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }
    fn slot() -> AccountSlotId {
        AccountSlotId::parse("00000000-0000-4000-8000-000000000001").unwrap()
    }
    thread_local! {
        static TEST_CAPABILITY: std::cell::RefCell<Option<BindingCapability>> = const { std::cell::RefCell::new(None) };
    }
    fn store() -> ClaudeSnapshotStore {
        let root = std::env::temp_dir().join(format!("quotabar-c3b0-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root).unwrap();
        let capability = store
            .register_slot(slot(), "safe".into(), Some(PlanMetadata::Paid), now())
            .unwrap();
        TEST_CAPABILITY.with(|saved| *saved.borrow_mut() = Some(capability));
        store
    }
    fn input(sequence: u64, disposition: SyntheticDisposition) -> SyntheticDesktopObservation {
        SyntheticDesktopObservation {
            slot_id: slot(),
            capability: TEST_CAPABILITY.with(|saved| {
                saved
                    .borrow()
                    .as_ref()
                    .expect("test store capability")
                    .clone()
            }),
            binding_epoch: 1,
            sequence,
            observed_at: now(),
            plan: Some(PlanMetadata::Paid),
            disposition,
        }
    }

    #[test]
    fn maps_synthetic_windows_without_exposing_correlation() {
        let store = store();
        submit_synthetic_observation(
            &store,
            input(
                1,
                SyntheticDisposition::Available {
                    windows: vec![
                        SyntheticWindow {
                            kind: WindowKind::FiveHour,
                            used_percent: 12.0,
                            reset_at: Some(now() + Duration::hours(1)),
                        },
                        SyntheticWindow {
                            kind: WindowKind::Weekly,
                            used_percent: 34.0,
                            reset_at: Some(now() + Duration::days(1)),
                        },
                    ],
                },
            ),
            now(),
        )
        .unwrap();
        let json = serde_json::to_string(&store.project(now()).unwrap()).unwrap();
        assert!(json.contains("12.0") && json.contains("34.0"));
        assert!(!json.contains("00000000-0000-4000-8000-000000000002"));
    }

    #[test]
    fn uncertainty_fails_closed_before_rebind() {
        let store = store();
        submit_synthetic_observation(
            &store,
            input(
                1,
                SyntheticDisposition::Available {
                    windows: vec![SyntheticWindow {
                        kind: WindowKind::Weekly,
                        used_percent: 34.0,
                        reset_at: Some(now() + Duration::days(1)),
                    }],
                },
            ),
            now(),
        )
        .unwrap();
        submit_synthetic_observation(
            &store,
            input(2, SyntheticDisposition::ContinuityUncertain),
            now(),
        )
        .unwrap();
        let projection = store.project(now()).unwrap();
        assert!(projection.slots[0].weekly.used_percent.is_none());
        assert!(submit_synthetic_observation(
            &store,
            input(
                3,
                SyntheticDisposition::Available {
                    windows: vec![SyntheticWindow {
                        kind: WindowKind::Weekly,
                        used_percent: 50.0,
                        reset_at: Some(now() + Duration::days(1))
                    }]
                }
            ),
            now()
        )
        .is_err());
    }

    #[test]
    fn capability_debug_is_redacted() {
        let _store = store();
        TEST_CAPABILITY.with(|saved| {
            assert_eq!(
                format!("{:?}", saved.borrow().as_ref().unwrap()),
                "BindingCapability(<redacted>)"
            )
        });
    }

    #[test]
    fn partial_sequence_preserves_other_window_and_replay_is_rejected() {
        let store = store();
        submit_synthetic_observation(
            &store,
            input(
                1,
                SyntheticDisposition::Available {
                    windows: vec![
                        SyntheticWindow {
                            kind: WindowKind::FiveHour,
                            used_percent: 12.0,
                            reset_at: Some(now() + Duration::hours(1)),
                        },
                        SyntheticWindow {
                            kind: WindowKind::Weekly,
                            used_percent: 34.0,
                            reset_at: Some(now() + Duration::days(1)),
                        },
                    ],
                },
            ),
            now(),
        )
        .unwrap();
        submit_synthetic_observation(
            &store,
            input(
                2,
                SyntheticDisposition::Available {
                    windows: vec![SyntheticWindow {
                        kind: WindowKind::FiveHour,
                        used_percent: 56.0,
                        reset_at: Some(now() + Duration::hours(1)),
                    }],
                },
            ),
            now(),
        )
        .unwrap();
        let before = serde_json::to_vec(&store.project(now()).unwrap()).unwrap();
        assert_eq!(
            store.project(now()).unwrap().slots[0].weekly.used_percent,
            Some(34.0)
        );
        assert!(submit_synthetic_observation(
            &store,
            input(
                2,
                SyntheticDisposition::Unavailable {
                    error: SafeErrorCode::Unavailable
                }
            ),
            now()
        )
        .is_err());
        assert_eq!(
            serde_json::to_vec(&store.project(now()).unwrap()).unwrap(),
            before
        );
    }

    #[test]
    fn metadata_is_atomic_with_a_valid_observation() {
        let store = store();
        let mut valid = input(
            1,
            SyntheticDisposition::Available {
                windows: vec![SyntheticWindow {
                    kind: WindowKind::Weekly,
                    used_percent: 34.0,
                    reset_at: Some(now() + Duration::days(1)),
                }],
            },
        );
        valid.plan = Some(PlanMetadata::Free);
        submit_synthetic_observation(&store, valid, now()).unwrap();
        assert_eq!(
            store.project(now()).unwrap().slots[0].plan,
            Some(PlanMetadata::Free)
        );

        let before = serde_json::to_vec(&store.project(now()).unwrap()).unwrap();
        let mut invalid = input(
            2,
            SyntheticDisposition::Available {
                windows: vec![SyntheticWindow {
                    kind: WindowKind::Weekly,
                    used_percent: 101.0,
                    reset_at: Some(now() + Duration::days(1)),
                }],
            },
        );
        invalid.plan = Some(PlanMetadata::Paid);
        assert!(submit_synthetic_observation(&store, invalid, now()).is_err());
        assert_eq!(
            serde_json::to_vec(&store.project(now()).unwrap()).unwrap(),
            before
        );
    }
}

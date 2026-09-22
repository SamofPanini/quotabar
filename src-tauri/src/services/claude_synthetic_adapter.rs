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
        SyntheticDisposition::ContinuityUncertain | SyntheticDisposition::IdentityChanged => store
            .apply_correlated_lifecycle_transition(
                &input.capability,
                &input.slot_id,
                input.binding_epoch,
                input.sequence,
                received_at,
            ),
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

    fn lifecycle_observation(
        slot_id: AccountSlotId,
        capability: BindingCapability,
        binding_epoch: u64,
        sequence: u64,
        disposition: SyntheticDisposition,
    ) -> SyntheticDesktopObservation {
        SyntheticDesktopObservation {
            slot_id,
            capability,
            binding_epoch,
            sequence,
            observed_at: now(),
            plan: None,
            disposition,
        }
    }

    fn raw_bytes(store: &ClaudeSnapshotStore) -> Vec<u8> {
        store.persisted_state_bytes_for_test().unwrap()
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
        store
            .apply_correlated_lifecycle_transition(&old, &slot_id, 1, 1, now())
            .unwrap();
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
        store
            .apply_correlated_lifecycle_transition(&capability, &slot_id, 1, 1, now())
            .unwrap();
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

    #[test]
    fn wrong_slot_lifecycle_dispositions_preserve_durable_state_and_sequence() {
        let root =
            std::env::temp_dir().join(format!("quotabar-c3b0-lifecycle-slots-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root).unwrap();
        let a = slot("00000000-0000-4000-8000-000000000008");
        let b = slot("00000000-0000-4000-8000-000000000009");
        let a_capability = store
            .register_slot(a.clone(), "a".into(), Some(PlanMetadata::Paid), now())
            .unwrap();
        let b_capability = store
            .register_slot(b.clone(), "b".into(), Some(PlanMetadata::Free), now())
            .unwrap();
        submit_synthetic_observation(
            &store,
            observation(a.clone(), a_capability.clone(), 1),
            now(),
        )
        .unwrap();
        let mut b_observation = observation(b.clone(), b_capability.clone(), 1);
        b_observation.plan = Some(PlanMetadata::Free);
        submit_synthetic_observation(&store, b_observation, now()).unwrap();

        for (offset, disposition) in [
            SyntheticDisposition::ContinuityUncertain,
            SyntheticDisposition::IdentityChanged,
        ]
        .into_iter()
        .enumerate()
        {
            let sequence = 2 + offset as u64;
            let before_bytes = raw_bytes(&store);
            let before_projection = serde_json::to_vec(&store.project(now()).unwrap()).unwrap();
            assert_eq!(
                submit_synthetic_observation(
                    &store,
                    lifecycle_observation(
                        a.clone(),
                        b_capability.clone(),
                        1,
                        sequence,
                        disposition,
                    ),
                    now(),
                ),
                Err(SnapshotError::Rejected)
            );
            assert_eq!(raw_bytes(&store), before_bytes);
            assert_eq!(
                serde_json::to_vec(&store.project(now()).unwrap()).unwrap(),
                before_projection
            );
            submit_synthetic_observation(
                &store,
                observation(a.clone(), a_capability.clone(), sequence),
                now(),
            )
            .unwrap();
        }
        let slots = store.project(now()).unwrap().slots;
        let a_slot = slots.iter().find(|slot| slot.alias == "a").unwrap();
        let b_slot = slots.iter().find(|slot| slot.alias == "b").unwrap();
        assert_eq!(a_slot.plan, Some(PlanMetadata::Paid));
        assert_eq!(a_slot.weekly.used_percent, Some(34.0));
        assert_eq!(b_slot.plan, Some(PlanMetadata::Free));
        assert_eq!(b_slot.weekly.used_percent, Some(34.0));
    }

    #[test]
    fn stale_unverified_and_unpaired_lifecycle_inputs_are_durable_noops() {
        let root =
            std::env::temp_dir().join(format!("quotabar-c3b0-lifecycle-stale-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root).unwrap();
        let slot_id = slot("00000000-0000-4000-8000-00000000000a");
        let old = store
            .register_slot(slot_id.clone(), "safe".into(), None, now())
            .unwrap();
        submit_synthetic_observation(
            &store,
            lifecycle_observation(
                slot_id.clone(),
                old.clone(),
                1,
                1,
                SyntheticDisposition::ContinuityUncertain,
            ),
            now(),
        )
        .unwrap();
        let before_unverified = raw_bytes(&store);
        assert_eq!(
            submit_synthetic_observation(
                &store,
                lifecycle_observation(
                    slot_id.clone(),
                    old.clone(),
                    2,
                    1,
                    SyntheticDisposition::IdentityChanged,
                ),
                now(),
            ),
            Err(SnapshotError::Rejected)
        );
        assert_eq!(raw_bytes(&store), before_unverified);

        let current = store.rebind(&slot_id, now()).unwrap();
        let before_stale = raw_bytes(&store);
        assert_eq!(
            submit_synthetic_observation(
                &store,
                lifecycle_observation(
                    slot_id.clone(),
                    old,
                    2,
                    1,
                    SyntheticDisposition::ContinuityUncertain,
                ),
                now(),
            ),
            Err(SnapshotError::Rejected)
        );
        assert_eq!(raw_bytes(&store), before_stale);
        submit_synthetic_observation(
            &store,
            lifecycle_observation(
                slot_id.clone(),
                current,
                2,
                1,
                SyntheticDisposition::IdentityChanged,
            ),
            now(),
        )
        .unwrap();

        let unpaired = store.rebind(&slot_id, now()).unwrap();
        store.unpair(&slot_id, now()).unwrap();
        let before_unpaired = raw_bytes(&store);
        assert_eq!(
            submit_synthetic_observation(
                &store,
                lifecycle_observation(
                    slot_id,
                    unpaired,
                    3,
                    1,
                    SyntheticDisposition::ContinuityUncertain,
                ),
                now(),
            ),
            Err(SnapshotError::Rejected)
        );
        assert_eq!(raw_bytes(&store), before_unpaired);
    }

    fn assert_rejected_lifecycle_case(
        case: &str,
        binding_epoch: u64,
        sequence: u64,
        disposition: SyntheticDisposition,
    ) {
        let root =
            std::env::temp_dir().join(format!("quotabar-c3b0-lifecycle-{case}-{}", Uuid::new_v4()));
        let store = ClaudeSnapshotStore::at_root(root).unwrap();
        let slot_id = slot("00000000-0000-4000-8000-00000000000b");
        let capability = store
            .register_slot(slot_id.clone(), "safe".into(), None, now())
            .unwrap();
        submit_synthetic_observation(
            &store,
            observation(slot_id.clone(), capability.clone(), 1),
            now(),
        )
        .unwrap();
        let before_bytes = raw_bytes(&store);
        let before_projection = serde_json::to_vec(&store.project(now()).unwrap()).unwrap();
        assert_eq!(
            submit_synthetic_observation(
                &store,
                lifecycle_observation(
                    slot_id.clone(),
                    capability.clone(),
                    binding_epoch,
                    sequence,
                    disposition
                ),
                now(),
            ),
            Err(SnapshotError::Rejected),
            "{case} must be a fixed rejection"
        );
        assert_eq!(
            raw_bytes(&store),
            before_bytes,
            "{case} must not alter disk bytes"
        );
        assert_eq!(
            serde_json::to_vec(&store.project(now()).unwrap()).unwrap(),
            before_projection,
            "{case} must preserve binding, plan, and windows"
        );
        submit_synthetic_observation(&store, observation(slot_id, capability, 2), now()).unwrap();
    }

    #[test]
    fn complete_lifecycle_rejection_matrix_preserves_bytes_and_next_sequence() {
        for (case, epoch, sequence, disposition) in [
            (
                "wrong-epoch-continuity",
                2,
                2,
                SyntheticDisposition::ContinuityUncertain,
            ),
            (
                "wrong-epoch-identity",
                2,
                2,
                SyntheticDisposition::IdentityChanged,
            ),
            (
                "replay-continuity",
                1,
                1,
                SyntheticDisposition::ContinuityUncertain,
            ),
            (
                "replay-identity",
                1,
                1,
                SyntheticDisposition::IdentityChanged,
            ),
            (
                "skipped-continuity",
                1,
                3,
                SyntheticDisposition::ContinuityUncertain,
            ),
            (
                "skipped-identity",
                1,
                3,
                SyntheticDisposition::IdentityChanged,
            ),
        ] {
            assert_rejected_lifecycle_case(case, epoch, sequence, disposition);
        }
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

    fn raw_bytes(store: &ClaudeSnapshotStore) -> Vec<u8> {
        store.persisted_state_bytes_for_test().unwrap()
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
    fn accepted_unavailable_preserves_windows_persists_safe_error_and_consumes_sequence() {
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
        let before = raw_bytes(&store);

        submit_synthetic_observation(
            &store,
            input(
                2,
                SyntheticDisposition::Unavailable {
                    error: SafeErrorCode::Unavailable,
                },
            ),
            now(),
        )
        .unwrap();

        let projection = store.project(now()).unwrap();
        let slot = projection
            .slots
            .iter()
            .find(|slot| slot.alias == "safe")
            .unwrap();
        assert_eq!(slot.plan, Some(PlanMetadata::Paid));
        assert_eq!(slot.five_hour.used_percent, Some(12.0));
        assert_eq!(slot.weekly.used_percent, Some(34.0));
        assert_eq!(
            slot.five_hour.last_error_code,
            Some(SafeErrorCode::Unavailable)
        );
        assert_eq!(
            slot.weekly.last_error_code,
            Some(SafeErrorCode::Unavailable)
        );

        let after = raw_bytes(&store);
        assert_ne!(after, before);
        let durable = String::from_utf8(after).unwrap();
        assert!(durable.contains("\"last_error_code\":\"unavailable\""));
        assert!(!durable.contains("provider_error"));

        submit_synthetic_observation(
            &store,
            input(
                3,
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

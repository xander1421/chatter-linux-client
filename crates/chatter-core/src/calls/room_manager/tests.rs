use super::*;

#[cfg(test)]
mod loss_policy_tests {
    use super::loss_is_retryable;
    use livekit::DisconnectReason as R;

    #[test]
    fn only_transport_losses_earn_an_application_reconnect() {
        for reason in [R::SignalClose, R::ConnectionTimeout, R::UnknownReason] {
            assert!(loss_is_retryable(reason), "{reason:?}");
        }
        for reason in [
            R::ClientInitiated,
            R::DuplicateIdentity,
            R::ParticipantRemoved,
            R::RoomDeleted,
            R::RoomClosed,
            R::ServerShutdown,
            R::UserRejected,
        ] {
            assert!(!loss_is_retryable(reason), "{reason:?}");
        }
    }
}

#[cfg(test)]
mod receiver_cryptor_evidence_tests {
    use super::ReceiverCryptorEvidence;
    use livekit::webrtc::native::frame_cryptor::EncryptionState as State;

    #[test]
    fn local_encryption_and_remote_key_changes_are_not_receiver_success() {
        let mut evidence = ReceiverCryptorEvidence::default();
        assert!(!evidence.observe(false, State::Ok));
        for state in [
            State::New,
            State::KeyRatcheted,
            State::MissingKey,
            State::DecryptionFailed,
            State::EncryptionFailed,
            State::InternalError,
        ] {
            assert!(!evidence.observe(true, state));
        }
        assert!(evidence.observe(true, State::Ok));
        assert!(!evidence.observe(true, State::Ok));
        assert!(!evidence.observe(true, State::DecryptionFailed));
        assert!(!evidence.observe(true, State::Ok));
        // A fresh connection gets its own evidence; no cross-call inheritance.
        assert!(ReceiverCryptorEvidence::default().observe(true, State::Ok));
    }
}

#[cfg(test)]
mod key_ring_tests {
    use super::key_ring_index;

    #[test]
    fn key_ring_index_never_overflows() {
        assert_eq!(key_ring_index(0), 0);
        assert_eq!(key_ring_index(17), 1);
        assert_eq!(key_ring_index(u32::MAX), 15);
        assert_eq!(
            key_ring_index(u32::MAX.wrapping_add(1)),
            0,
            "next slot after the last epoch"
        );
    }
}

#[cfg(test)]
mod timing_lifecycle_tests {
    use super::{CallTiming, RwLock, replace_timing_slot, retire_timing_slot};

    #[test]
    fn disconnect_then_failed_reconnect_rejects_queued_key_milestone() {
        let old = CallTiming::new("join_requested_worker");
        let slot = RwLock::new(Some(old.clone()));
        retire_timing_slot(&slot, None);
        // Token refresh fails before connect_timed can install a new observer.
        assert!(slot.read().is_none());
        assert!(!old.mark("call_key_accepted_publish_gate_open"));
        assert!(!old.first_decoded_frame());
    }

    #[test]
    fn failed_old_connection_does_not_retire_replacement() {
        let old = CallTiming::new("test");
        let next = CallTiming::new("test");
        let slot = RwLock::new(Some(old.clone()));
        replace_timing_slot(&slot, next.clone());
        assert!(!old.first_decoded_frame());
        retire_timing_slot(&slot, Some(&old));
        assert!(!old.mark("remote_participant_present"));
        assert!(slot.read().as_ref().unwrap().same_attempt(&next));
        assert!(next.first_decoded_frame());
        retire_timing_slot(&slot, Some(&next));
        assert!(slot.read().is_none());
        assert!(!next.mark("call_key_accepted_publish_gate_open"));
    }
}

#[cfg(test)]
mod reconnect_initialization_tests {
    use super::*;
    use crate::crypto::call_key_provider::CallKeyState;

    #[test]
    fn provisional_partial_rejection_rolls_back_native_provider_and_retries() {
        // No SDK Room or sender cryptor fixture is constructed here. Real
        // KeyProvider setters mutate native slots; callback rejection supplies
        // the bounded fault after current selection or before any mutation.
        for failure_epoch in [0, 2] {
            for reject_next in [false, true] {
                for reject_rollback in [false, true] {
                    let original = CallKeyState::new(&[9; 32], "provisional-native").unwrap();
                    let provider = KeyProvider::with_shared_key(
                        KeyProviderOptions {
                            key_ring_size: KEY_RING_SIZE,
                            ratchet_window_size: 0,
                            ..KeyProviderOptions::default()
                        },
                        original.current_key_vec(),
                    );
                    let snapshot = ReconnectKeys {
                        provider: provider.clone(),
                        epoch: original.epoch(),
                        current: zeroize::Zeroizing::new(original.current_key_vec()),
                        next: zeroize::Zeroizing::new(original.peek_next_key().unwrap().to_vec()),
                    };
                    let reference = KeyProvider::with_shared_key(
                        KeyProviderOptions {
                            key_ring_size: KEY_RING_SIZE,
                            ratchet_window_size: 0,
                            ..KeyProviderOptions::default()
                        },
                        original.current_key_vec(),
                    );
                    reference.set_shared_key(snapshot.next.to_vec(), 1);
                    reference.set_shared_key(snapshot.current.to_vec(), 0);
                    let mut candidate = original.clone();
                    let mut failed = false;
                    let mut rollback_seen = false;
                    let result = initialize_provisional_keys(
                        Some(&snapshot),
                        Some((&mut candidate, 3)),
                        |epoch, current, next| {
                            if failed {
                                rollback_seen = true;
                                assert_eq!(epoch, snapshot.epoch);
                                if reject_rollback {
                                    return false;
                                }
                            }
                            if epoch == failure_epoch && !failed {
                                failed = true;
                                if reject_next {
                                    provider.set_shared_key(current, key_ring_index(epoch));
                                    assert_eq!(
                                        provider.get_latest_key_index(),
                                        key_ring_index(epoch)
                                    );
                                }
                                return false;
                            }
                            // set_shared_key selects data metadata as well, so restore
                            // current selection after installing future receive material.
                            provider.set_shared_key(next, key_ring_index(epoch + 1));
                            provider.set_shared_key(current, key_ring_index(epoch));
                            true
                        },
                    );
                    assert!(result.is_err());
                    assert!(rollback_seen);
                    assert_eq!(candidate.epoch(), original.epoch());
                    assert_eq!(candidate.current_key(), original.current_key());
                    assert_eq!(candidate.anchor(), original.anchor());
                    assert_eq!(
                        provider.get_latest_key_index(),
                        key_ring_index(original.epoch())
                    );
                    let current_material = provider
                        .get_shared_key(key_ring_index(original.epoch()))
                        .unwrap();
                    assert_eq!(Some(current_material), reference.get_shared_key(0));
                    if !reject_rollback {
                        assert_eq!(provider.get_shared_key(1), reference.get_shared_key(1));
                    }
                    // Retrying the same production helper reconstructs all elapsed
                    // slots and commits only after every callback succeeds.
                    assert_eq!(
                        initialize_provisional_keys(
                            Some(&snapshot),
                            Some((&mut candidate, 3)),
                            |epoch, current, next| {
                                provider.set_shared_key(next, key_ring_index(epoch + 1));
                                provider.set_shared_key(current, key_ring_index(epoch));
                                true
                            }
                        )
                        .unwrap(),
                        Some(3)
                    );
                    assert_eq!(candidate.epoch(), 3);
                    assert_eq!(provider.get_latest_key_index(), 3);
                    for epoch in 0..=4 {
                        assert!(
                            !provider
                                .get_shared_key(key_ring_index(epoch))
                                .unwrap()
                                .is_empty()
                        );
                    }
                    // Compare derived native material against an independently
                    // reconstructed provider, never reimport native exports.
                    let mut expected = original.clone();
                    for epoch in 0..=4 {
                        reference.set_shared_key(expected.current_key_vec(), key_ring_index(epoch));
                        assert_eq!(
                            provider.get_shared_key(key_ring_index(epoch)),
                            reference.get_shared_key(key_ring_index(epoch))
                        );
                        if epoch < 4 {
                            expected.ratchet().unwrap();
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn repeated_default_microphone_publish_preserves_muted_capture() {
        let (manager, _) = RoomManager::new();
        manager.microphone_requested.store(true, std::sync::atomic::Ordering::SeqCst);
        manager.set_microphone_muted(true);
        // No room is installed: success proves a repeated default request did
        // not enter the replacement/unpublication path (which needs a room).
        manager.publish_microphone(None).await.unwrap();
        assert!(manager.is_microphone_muted());
        assert!(matches!(manager.publish_microphone(Some("other")).await,
            Err(CallError::NotInCall)));
    }

    #[tokio::test]
    async fn muted_microphone_request_survives_teardown_until_explicit_stop() {
        let (manager, _) = RoomManager::new();
        manager
            .microphone_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        manager.set_microphone_muted(true);
        assert_eq!(manager.reconnect_microphone_device(), Some(None));
        assert!(manager.is_microphone_muted());
        manager.disconnect().await;
        assert_eq!(manager.reconnect_microphone_device(), Some(None));
        manager.stop_microphone().await;
        assert!(manager.reconnect_microphone_device().is_none());
    }

    #[tokio::test]
    async fn fresh_room_failed_connect_preserves_absolute_epoch_and_native_ring() {
        for epoch in [7, 15, 16, 17] {
            let (manager, _events) = RoomManager::new();
            let mut chain = CallKeyState::new(&[3; 32], "reconnect-boundary").unwrap();
            let provider = KeyProvider::with_shared_key(
                KeyProviderOptions {
                    ratchet_window_size: 0,
                    key_ring_size: KEY_RING_SIZE,
                    ..KeyProviderOptions::default()
                },
                chain.current_key_vec(),
            );
            for _ in 0..epoch {
                chain.ratchet().unwrap();
                provider.set_shared_key(chain.current_key_vec(), key_ring_index(chain.epoch()));
            }
            let before = zeroize::Zeroizing::new(
                (0..KEY_RING_SIZE)
                    .map(|i| provider.get_shared_key(i))
                    .collect::<Vec<_>>(),
            );
            {
                let mut state = manager.e2ee.write();
                state.provider = Some(provider.clone());
                state.epoch = epoch;
            }
            let snapshot = manager.snapshot_reconnect_keys(&chain).unwrap();
            manager.disconnect().await;
            // Exercise the production fresh-room initialization path without
            // a server. A malformed URL fails before any network connection.
            let mut candidate = chain.clone();
            assert!(
                manager
                    .connect_reusing_keys_aligned("invalid://", "", snapshot, &mut candidate, None)
                    .await
                    .is_err()
            );
            let before_chain = chain.clone();
            for _ in 0..2 {
                let snapshot = manager.snapshot_reconnect_keys(&chain).unwrap();
                let mut candidate = chain.clone();
                assert!(
                    manager
                        .connect_reusing_keys_aligned(
                            "invalid://",
                            "",
                            snapshot,
                            &mut candidate,
                            Some(chrono::Utc::now() - chrono::Duration::seconds(900)),
                        )
                        .await
                        .is_err()
                );
                assert_eq!(candidate.epoch(), before_chain.epoch());
                assert_eq!(candidate.anchor(), before_chain.anchor());
                assert_eq!(candidate.current_key(), before_chain.current_key());
                // Provider is retained, but a boundary while the room is absent
                // must fail without changing manager authority.
                assert!(!manager.set_ratcheted_shared_key(
                    epoch + 1,
                    chain.peek_next_key().unwrap().to_vec(),
                    vec![0; 32],
                ));
            }
            assert_eq!(manager.key_epoch(), epoch);
            assert_eq!(
                manager
                    .e2ee
                    .read()
                    .provider
                    .as_ref()
                    .unwrap()
                    .get_latest_key_index(),
                key_ring_index(epoch)
            );
            for i in 0..KEY_RING_SIZE {
                let held = zeroize::Zeroizing::new(provider.get_shared_key(i));
                assert!(*held == before[i as usize]);
            }
            // Snapshot staging must reject mismatched authority before teardown.
            chain.ratchet().unwrap();
            assert!(manager.snapshot_reconnect_keys(&chain).is_err());
            assert_eq!(manager.key_epoch(), epoch);
        }
    }
}

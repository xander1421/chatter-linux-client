use super::*;

#[cfg(test)]
mod ring_timeout_tests {
    use super::{RING_TIMEOUT, offer_is_stale};

    #[test]
    fn only_offers_past_the_ring_timeout_are_stale() {
        let now = 1_800_000_000_000_000_000u64;
        let timeout = RING_TIMEOUT.as_nanos() as u64;
        assert!(!offer_is_stale(now - 5_000_000_000, now));
        assert!(offer_is_stale(now - timeout, now));
        assert!(offer_is_stale(now - timeout - 1, now));
        // The F56 case: an offer from 40 minutes ago, drained on app start.
        assert!(offer_is_stale(now - 40 * 60 * 1_000_000_000, now));
        // Sender clock ahead of ours, or no timestamp at all: ring.
        assert!(!offer_is_stale(now + 10_000_000_000, now));
        assert!(!offer_is_stale(0, now));
    }
}

#[cfg(test)]
mod call_events_tx_tests {
    use super::{CallEndReason, CallEventsTx, CallHandle, CallOrchestratorEvent};

    fn ended(i: usize) -> CallOrchestratorEvent {
        CallOrchestratorEvent::ActiveEnded {
            call: CallHandle::from(format!("call-{i}").as_str()),
            reason: CallEndReason::RemoteHangup,
        }
    }

    #[test]
    fn a_lossless_tap_gets_every_event_the_broadcast_drops() {
        let events = CallEventsTx::new(4);
        let mut lagging = events.subscribe();
        let mut lossless = events.subscribe_lossless();
        for i in 0..20 {
            let _ = events.send(ended(i));
        }
        assert!(matches!(
            lagging.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
        ));
        for i in 0..20 {
            assert_eq!(lossless.try_recv().unwrap(), ended(i));
        }
        assert!(lossless.try_recv().is_err());
    }

    #[test]
    fn a_dropped_tap_is_pruned_and_the_channel_closes_with_its_senders() {
        let events = CallEventsTx::new(4);
        drop(events.subscribe_lossless());
        let mut live = events.subscribe_lossless();
        let _ = events.send(ended(0));
        assert_eq!(events.lossless.lock().len(), 1);
        drop(events);
        assert_eq!(live.try_recv().unwrap(), ended(0));
        assert!(matches!(
            live.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
    }
}

#[cfg(test)]
mod joined_channel_tests {
    use super::{ChannelId, UserId, joined_channel_id};

    #[test]
    fn callee_files_a_channel_free_dm_call_under_the_caller() {
        let (caller, callee) = (UserId::new(), UserId::new());
        // The caller scoped it to us: file under our DM with the caller, not Saved Messages.
        assert_eq!(
            joined_channel_id(callee.into(), &caller, &callee),
            ChannelId::from(caller)
        );
        // A real channel id (group, or channel-backed DM) is kept.
        let channel = ChannelId::new();
        assert_eq!(joined_channel_id(channel, &caller, &callee), channel);
    }
}

#[cfg(test)]
mod orchestrator_kind_tests {
    use super::{
        CallHandle, OrchestratedActive, OrchestratedCallKind as K, OrchestratorState,
        PendingOutgoing, UserId,
    };

    /// `end_active_call` dispatches on this kind; `None` returns early with no
    /// hangup sent. A DM call cancelled while it rings must resolve to
    /// `DmStandard`, whose end path sends `CallHangup` and leaves.
    #[test]
    fn a_ringing_outgoing_call_resolves_to_its_kind() {
        let mut orch = OrchestratorState::default();
        assert_eq!(orch.call_kind(), None);

        let call = CallHandle::from("ringing");
        orch.outgoing.insert(
            call.clone(),
            PendingOutgoing {
                session_revision: None,
                call: call.clone(),
                kind: K::DmStandard,
                peer_id: UserId::new(),
                peer_name: String::new(),
                is_video: false,
            },
        );
        assert_eq!(orch.call_kind(), Some(K::DmStandard));

        // Connected: the active slot is what counts.
        orch.outgoing.remove(&call);
        orch.active = Some(OrchestratedActive {
            peer_id: None,
            session_revision: None,
            call,
            kind: K::DmStandard,
        });
        assert_eq!(orch.call_kind(), Some(K::DmStandard));
    }
}

#[cfg(test)]
mod peer_left_tests {
    use super::{OrchestratedCallKind as K, peer_left_ends_call};

    #[test]
    fn one_to_one_ends_only_when_the_room_is_empty() {
        let gone = vec!["peer".to_string()];
        assert!(peer_left_ends_call(Some(K::DmStandard), &[], "peer"));
        assert!(peer_left_ends_call(Some(K::DmStandard), &gone, "peer"));
        assert!(!peer_left_ends_call(
            Some(K::DmStandard),
            &["other".into()],
            "peer"
        ));
        // Channel calls and no-call are never ended by a departure.
        assert!(!peer_left_ends_call(Some(K::GroupChannel), &[], "peer"));
        assert!(!peer_left_ends_call(None, &[], "peer"));
    }
}

#[cfg(test)]
mod ratchet_align_tests {
    use super::{KEY_RATCHET_INTERVAL_SECS, aligned_ratchet_schedule};

    #[test]
    fn two_peers_align_to_same_boundaries() {
        let iv = KEY_RATCHET_INTERVAL_SECS; // 30
        // Peer A connects ~at call start, peer B 4s later. Both still target
        // epoch 0 now and the SAME absolute first boundary (call_start + 30).
        let (ea, da) = aligned_ratchet_schedule(0.2, iv);
        let (eb, db) = aligned_ratchet_schedule(4.2, iv);
        assert_eq!(ea, 0);
        assert_eq!(eb, 0);
        // call_start + da (from A) == call_start + 4.0 + db (from B) == boundary 30.
        assert!((da - 29.8).abs() < 1e-6, "A delay {}", da);
        assert!((db - 25.8).abs() < 1e-6, "B delay {}", db);
        assert!(
            ((0.2 + da) - (4.2 + db)).abs() < 1e-6,
            "boundaries must match"
        );
    }

    #[test]
    fn late_joiner_fast_forwards_to_current_epoch() {
        let iv = KEY_RATCHET_INTERVAL_SECS;
        // Joined 95s in: epochs 0,1,2 already passed -> current epoch 3,
        // next boundary at 120s -> 25s away.
        let (epoch, delay) = aligned_ratchet_schedule(95.0, iv);
        assert_eq!(epoch, 3);
        assert!((delay - 25.0).abs() < 1e-6, "delay {}", delay);
    }

    #[test]
    fn negative_elapsed_clamps_to_zero() {
        // Clock skew: local clock behind server anchor.
        let (epoch, delay) = aligned_ratchet_schedule(-3.0, KEY_RATCHET_INTERVAL_SECS);
        assert_eq!(epoch, 0);
        assert!((delay - 30.0).abs() < 1e-6);
    }
}

#[cfg(test)]
mod failed_accept_tests {
    use super::{
        CallEndReason, CallHandle, CallOrchestratorEvent, CallWorkerHandle, OrchestratedCallKind,
        UserId,
    };

    /// An accept takes the call out of the ringing set before it connects, so
    /// a failed connect leaves it in no slot. `accept_presented_incoming_call`
    /// ends it with `orch_mark_ended(Error)`, which must still reach the
    /// history recorder (else the answered call stays "missed") and the UI
    /// (else the answering view never comes down).
    #[tokio::test]
    async fn ending_an_accepted_call_that_never_connected_emits_active_ended() {
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
        let worker = CallWorkerHandle::spawn(crate::network::grpc::CallClient::new(
            channel.into(),
            crate::network::grpc::interceptors::AuthInterceptor::new(),
        ));
        let mut events = worker.subscribe_call_events();
        let call = CallHandle::from("accepted-then-failed");

        worker.orch_present_incoming(
            call.clone(),
            OrchestratedCallKind::DmStandard,
            UserId::new(),
            "Peer".into(),
            false,
            true,
            String::new(),
            Some([0; 32]),
            None,
            String::new(),
        );
        worker
            .orch_accept_incoming(call.clone())
            .await
            .expect("presented call accepts");
        worker.orch_mark_ended(
            call.clone(),
            CallEndReason::Error("connect timed out".into()),
        );

        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await {
                Ok(Ok(CallOrchestratorEvent::ActiveEnded {
                    call: ended,
                    reason,
                })) => {
                    assert_eq!(ended, call);
                    assert!(matches!(reason, CallEndReason::Error(_)));
                    break;
                }
                Ok(Ok(CallOrchestratorEvent::IncomingPresented { .. })) => continue,
                other => panic!("expected ActiveEnded, got {other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod publish_gate_tests {
    use super::{DeferredMedia, PublishGate};

    /// §124 1k item 2: a group joiner publishes nothing until it holds the
    /// call's key; what it asked for meanwhile is published then, once.
    #[test]
    fn a_joiner_publishes_only_once_keyed() {
        let mut gate = PublishGate::awaiting_key(false);
        assert!(gate.is_closed());
        assert!(
            gate.defer_mic(Some("usb".into())),
            "mic held back while keyless"
        );
        assert!(gate.defer_camera(), "camera held back while keyless");
        assert_eq!(
            gate.open(),
            Some(DeferredMedia {
                mic: Some(Some("usb".into())),
                camera: true
            })
        );
        assert!(!gate.is_closed());
        assert_eq!(gate.open(), None, "published once");
        assert!(!gate.defer_mic(None), "open: publishes go straight out");
        assert!(!gate.defer_camera());
    }

    #[test]
    fn a_video_joiner_holds_its_camera_and_stopping_drops_it() {
        let mut gate = PublishGate::awaiting_key(true);
        gate.drop_camera();
        gate.drop_mic();
        assert_eq!(
            gate.open(),
            Some(DeferredMedia {
                mic: None,
                camera: false
            })
        );
    }

    #[test]
    fn a_keyed_participant_is_never_held() {
        let mut gate = PublishGate::default();
        assert!(!gate.is_closed());
        assert!(!gate.defer_mic(None));
        assert_eq!(gate.open(), None);
    }
}

#[cfg(test)]
mod group_key_install_tests {
    use super::{
        ActiveCallState, Call, CallError, CallId, CallKeyState, CallType, ChannelId,
        MAX_RATCHET_CATCH_UP, PublishGate, UserId, adopt_group_key, adopt_into_call, chain_install,
        ratchet_to,
    };

    const CALL: &str = "11111111-2222-4333-8444-555555555555";

    fn active(root: [u8; 32]) -> ActiveCallState {
        let mut call = Call::new(ChannelId::new(), UserId::new(), CallType::Voice);
        call.id = CALL.parse().unwrap();
        ActiveCallState {
            reconnect_media: None,
            fresh_rejoin_used: false,
            call,
            key_state: CallKeyState::new(&root, CALL).unwrap(),
            livekit_url: String::new(),
            publish_gate: PublishGate::default(),
        }
    }

    /// The rotator rotates (to its epoch + 1) and sends that `(key, epoch)`;
    /// a receiver whose chain has ratcheted differently
    /// installs it. Both must hand LiveKit the same index and bytes, and keep
    /// agreeing on the next 30 s ratchet.
    #[test]
    fn received_key_installs_the_rotators_bytes() {
        let root = [7u8; 32];
        let new_key = [9u8; 32];
        let mut rotator = CallKeyState::new(&root, CALL).unwrap();
        rotator.ratchet().unwrap();
        rotator.ratchet().unwrap();
        let epoch = rotator.rotate(new_key).unwrap();
        let sent = chain_install(&rotator);

        let mut receiver = CallKeyState::new(&root, CALL).unwrap();
        let installed = adopt_group_key(&mut receiver, new_key, epoch).unwrap();
        assert_eq!(sent, installed);
        assert_eq!(sent.0, 3);
        assert_eq!(rotator.current_key(), receiver.current_key());

        rotator.ratchet().unwrap();
        receiver.ratchet().unwrap();
        assert_eq!(rotator.current_key(), receiver.current_key());
        assert_eq!(rotator.epoch(), receiver.epoch());
    }

    /// Rotations put the chain ahead of the clock only until the clock
    /// catches up, so however many a call has had, a joiner (on the
    /// wall-clock epoch) can adopt the join rotation's key and then ratchets
    /// in step.
    #[test]
    fn rotations_do_not_drift_the_chain_from_the_clock() {
        let root = [7u8; 32];
        let mut rotator = CallKeyState::new(&root, CALL).unwrap();
        ratchet_to(&mut rotator, 10).unwrap();
        for _ in 0..3 {
            rotator
                .rotate(crate::crypto::generate_group_call_key())
                .unwrap();
        }
        assert_eq!(rotator.epoch(), 13);
        assert!(
            ratchet_to(&mut rotator, 11).is_none(),
            "ahead of the clock: wait for it"
        );
        assert!(ratchet_to(&mut rotator, 12).is_none());

        let mut joiner = CallKeyState::new(&root, CALL).unwrap();
        ratchet_to(&mut joiner, 12).unwrap();
        let join_key = crate::crypto::generate_group_call_key();
        let issued = rotator.rotate(join_key).unwrap();
        assert!(adopt_group_key(&mut joiner, join_key, issued).is_some());

        for wall in 13..20 {
            ratchet_to(&mut rotator, wall);
            ratchet_to(&mut joiner, wall);
            assert_eq!(rotator.epoch(), wall.max(14));
            assert_eq!(
                joiner.current_key(),
                rotator.current_key(),
                "wall epoch {wall}"
            );
        }
    }

    /// Security review F1: a member who left and rejoins is sent only the
    /// join rotation's fresh key, from which no key used while they were away
    /// follows — nor does one from the chain they kept.
    #[test]
    fn rejoiner_cannot_derive_the_key_used_while_away() {
        use crate::crypto::generate_group_call_key as fresh;
        let root = [7u8; 32];
        let mut rotator = CallKeyState::new(&root, CALL).unwrap();
        let mut member = CallKeyState::new(&root, CALL).unwrap();
        ratchet_to(&mut rotator, 4).unwrap();
        ratchet_to(&mut member, 4).unwrap();
        let key = fresh();
        let at = rotator.rotate(key).unwrap();
        assert!(
            adopt_group_key(&mut member, key, at).is_some(),
            "the member's own join rotation"
        );

        // They leave: the leave rotation keys the call while they are away.
        rotator.rotate(fresh()).unwrap();
        let mut while_away = Vec::new();
        for wall in 6..12 {
            ratchet_to(&mut rotator, wall);
            while_away.push(*rotator.current_key());
            ratchet_to(&mut member, wall);
            assert!(
                !while_away.contains(member.current_key()),
                "their old chain, wall epoch {wall}"
            );
        }

        // They rejoin: the join rotation's key is all they are sent.
        let rejoin_key = fresh();
        let at = rotator.rotate(rejoin_key).unwrap();
        assert!(adopt_group_key(&mut member, rejoin_key, at).is_some());
        assert_eq!(member.current_key(), rotator.current_key());
        for wall in 12..40 {
            ratchet_to(&mut rotator, wall);
            ratchet_to(&mut member, wall);
            assert_eq!(member.current_key(), rotator.current_key());
            assert!(
                !while_away.contains(member.current_key()),
                "after rejoining, wall epoch {wall}"
            );
        }
    }

    /// A joiner is sent the join rotation's key — fresh, so nothing the call
    /// was keyed with before the join follows from it.
    #[test]
    fn joiner_is_sent_no_key_from_before_its_join() {
        use crate::crypto::generate_group_call_key as fresh;
        let mut rotator = CallKeyState::new(&[7u8; 32], CALL).unwrap();
        rotator.rotate(fresh()).unwrap();
        let mut before_join = Vec::new();
        for wall in 2..10 {
            ratchet_to(&mut rotator, wall);
            before_join.push(*rotator.current_key());
        }
        before_join.push(rotator.peek_next_key().unwrap());

        let join_key = fresh();
        let at = rotator.rotate(join_key).unwrap();
        let (_, sent, _) = chain_install(&rotator);
        assert_eq!(
            sent,
            join_key.to_vec(),
            "what goes to the joiner is the rotation's key"
        );
        assert!(!before_join.contains(&join_key));

        let mut joiner = CallKeyState::new(&[7u8; 32], CALL).unwrap();
        ratchet_to(&mut joiner, 9).unwrap();
        assert!(adopt_group_key(&mut joiner, join_key, at).is_some());
        for wall in 10..40 {
            ratchet_to(&mut joiner, wall);
            assert!(
                !before_join.contains(joiner.current_key()),
                "wall epoch {wall}"
            );
        }
    }

    #[test]
    fn ratchet_catch_up_is_bounded() {
        let mut state = CallKeyState::new(&[7u8; 32], CALL).unwrap();
        assert!(ratchet_to(&mut state, MAX_RATCHET_CATCH_UP + 1).is_none());
        assert!(ratchet_to(&mut state, u32::MAX).is_none());
        assert_eq!(state.epoch(), 0);
    }

    #[test]
    fn stale_epoch_changes_nothing() {
        let mut state = active([7u8; 32]);
        let call: CallId = CALL.parse().unwrap();
        assert!(
            adopt_into_call(Some(&mut state), call, [9u8; 32], 2)
                .unwrap()
                .is_some()
        );
        let held = *state.key_state.current_key();
        for stale in [0, 1, 2] {
            assert_eq!(
                adopt_into_call(Some(&mut state), call, [5u8; 32], stale).unwrap(),
                None
            );
        }
        assert_eq!(*state.key_state.current_key(), held);
        assert_eq!(state.key_state.epoch(), 2);
    }

    #[test]
    fn key_for_another_call_is_refused() {
        let mut state = active([7u8; 32]);
        let other = CallId(uuid::Uuid::new_v4());
        let before = *state.key_state.current_key();
        assert!(matches!(
            adopt_into_call(Some(&mut state), other, [9u8; 32], 5),
            Err(CallError::NotInCall)
        ));
        assert!(matches!(
            adopt_into_call(None, other, [9u8; 32], 5),
            Err(CallError::NotInCall)
        ));
        assert_eq!(*state.key_state.current_key(), before);
        assert_eq!(state.key_state.epoch(), 0);
    }
}

#[cfg(test)]
mod media_key_install_failure_tests {
    use super::*;

    fn awaiting_call() -> Rc<RefCell<Option<ActiveCallState>>> {
        let mut call = Call::new(ChannelId::new(), UserId::new(), CallType::Voice);
        call.id = "11111111-2222-4333-8444-555555555555".parse().unwrap();
        let key_state = CallKeyState::new(&[1; 32], &call.id.to_string()).unwrap();
        Rc::new(RefCell::new(Some(ActiveCallState {
            reconnect_media: None,
            fresh_rejoin_used: false,
            call,
            key_state,
            livekit_url: String::new(),
            publish_gate: PublishGate::awaiting_key(false),
        })))
    }

    #[test]
    fn failed_native_install_does_not_open_gate_or_consume_authority_epoch() {
        let active = awaiting_call();
        // Actual RoomManager rejects installation without an active room/provider.
        // No fabricated setter result or duplicated installation algorithm.
        let (room, _events) = RoomManager::new();
        let room = Arc::new(room);
        let call_id = active.borrow().as_ref().unwrap().call.id;
        let original = zeroize::Zeroizing::new(
            active
                .borrow()
                .as_ref()
                .unwrap()
                .key_state
                .current_key_vec(),
        );
        for _ in 0..2 {
            // Both attempts must return the real installation error. If the
            // first consumed epoch 2, the retry would incorrectly be Ok(false).
            assert!(matches!(
                install_group_media_key(&active, &room, None, call_id, [9; 32], 2),
                Err(CallError::CryptoError(_))
            ));
            let guard = active.borrow();
            let state = guard.as_ref().unwrap();
            assert_eq!(state.key_state.epoch(), 0);
            assert_eq!(state.key_state.anchor(), 0);
            assert!(state.key_state.current_key_vec() == *original);
            assert!(state.publish_gate.is_closed());
            assert!(state.publish_gate.mic.is_some());
        }
        assert!(matches!(
            install_group_media_key(&active, &room, None, CallId::new(), [9; 32], 2),
            Err(CallError::NotInCall)
        ));
        assert!(!room.is_connected());
        assert_eq!(room.key_epoch(), 0);
    }

    #[test]
    fn failed_dm_reroot_preserves_chain_and_same_root_retry() {
        let active = awaiting_call();
        let (room, _events) = RoomManager::new();
        let room = Arc::new(room);
        {
            let mut guard = active.borrow_mut();
            guard.as_mut().unwrap().key_state.ratchet().unwrap();
        }
        let original = active.borrow().as_ref().unwrap().key_state.clone();
        for _ in 0..2 {
            // Invoke the same production helper as SetEncryptionKey, including
            // the real disconnected RoomManager reset failure.
            assert!(!reset_dm_media_key(&active, &room, vec![9; 32]));
            let guard = active.borrow();
            let state = guard.as_ref().unwrap();
            assert!(state.key_state.current_key() == original.current_key());
            assert_eq!(state.key_state.epoch(), original.epoch());
            assert_eq!(state.key_state.anchor(), original.anchor());
            assert!(state.publish_gate.is_closed());
            assert!(state.publish_gate.mic.is_some());
        }
    }

    #[test]
    fn failed_rotation_keeps_the_current_chain_and_deferred_media() {
        let active = awaiting_call();
        let (room, _events) = RoomManager::new();
        let room = Arc::new(room);
        for _ in 0..2 {
            assert!(matches!(
                rotate_group_media_key(&active, &room, None, [7; 32]),
                Err(CallError::CryptoError(_))
            ));
            let guard = active.borrow();
            let state = guard.as_ref().unwrap();
            assert_eq!(state.key_state.epoch(), 0);
            assert_eq!(state.key_state.anchor(), 0);
            assert!(state.publish_gate.is_closed());
            assert!(state.publish_gate.mic.is_some());
        }
        assert!(!room.is_connected());
    }
}


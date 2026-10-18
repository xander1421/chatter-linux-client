use super::*;

#[cfg(test)]
mod reconnect_timer_boundary_tests {
    use super::*;
    use livekit::e2ee::key_provider::{KeyProvider, KeyProviderOptions};
    use std::cell::Cell;

    fn active(start: chrono::DateTime<chrono::Utc>) -> Rc<RefCell<Option<ActiveCallState>>> {
        let mut call = Call::new(ChannelId::new(), UserId::new(), CallType::Voice);
        call.started_at = Some(start);
        let mut chain = CallKeyState::new(&[7; 32], &call.id.to_string()).unwrap();
        chain.rotate([9; 32]).unwrap();
        Rc::new(RefCell::new(Some(ActiveCallState {
            call,
            key_state: chain,
            livekit_url: String::new(),
            publish_gate: PublishGate::awaiting_key(false),
            reconnect_media: None,
            fresh_rejoin_used: false,
        })))
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancelled_before_first_poll_never_installs_initial_catch_up() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let start = chrono::Utc::now() - chrono::Duration::seconds(120);
                let active = active(start);
                let cancel = Rc::new(RefCell::new(None));
                let calls = Rc::new(Cell::new(0));
                let count = calls.clone();
                start_key_ratchet_scheduler(
                    &cancel,
                    &active,
                    Rc::new(move |_, _, _| {
                        count.set(count.get() + 1);
                        true
                    }),
                    Rc::new(chrono::Utc::now),
                );
                stop_key_ratchet_timer(&cancel);
                tokio::task::yield_now().await;
                assert_eq!(calls.get(), 0);
                assert_eq!(active.borrow().as_ref().unwrap().key_state.epoch(), 1);
            })
            .await;
    }

    /// Executes the production actor suspension/restart and local timer using
    /// virtual elapsed time. Failure uses the actual absent-room installer;
    /// success substitutes the SDK connection boundary with a native provider,
    /// rather than reproducing any scheduling or catch-up decisions in a fake.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn absent_room_failed_retry_then_multi_epoch_success_resumes_one_timer() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let anchor = chrono::Utc::now();
                let base = tokio::time::Instant::now();
                let clock: Rc<dyn Fn() -> chrono::DateTime<chrono::Utc>> =
                    Rc::new(move || anchor + chrono::Duration::from_std(base.elapsed()).unwrap());
                let active = active(anchor);
                let original = active.borrow().as_ref().unwrap().key_state.clone();
                let cancel = Rc::new(RefCell::new(None));
                let present = Rc::new(Cell::new(true));
                let installs = Rc::new(RefCell::new(Vec::new()));
                let (manager, _) = RoomManager::new();
                let manager = Arc::new(manager);
                let provider = KeyProvider::with_shared_key(
                    KeyProviderOptions {
                        ratchet_window_size: 0,
                        key_ring_size: 16,
                        ..Default::default()
                    },
                    original.current_key_vec(),
                );
                let sink: Rc<dyn Fn(u32, Vec<u8>, Vec<u8>) -> bool> = {
                    let present = present.clone();
                    let installs = installs.clone();
                    let manager = manager.clone();
                    let provider = provider.clone();
                    Rc::new(move |epoch, current, next| {
                        installs.borrow_mut().push(epoch);
                        if !present.get() {
                            return manager.set_ratcheted_shared_key(epoch, current, next);
                        }
                        // Native receive bytes are checked below. This is an SDK
                        // boundary substitute, not a successful Room::connect test.
                        provider.set_shared_key(next, ((epoch + 1) % 16) as i32);
                        provider.set_shared_key(current, (epoch % 16) as i32);
                        true
                    })
                };
                start_key_ratchet_scheduler(&cancel, &active, sink.clone(), clock.clone());
                tokio::task::yield_now().await;
                tokio::time::advance(std::time::Duration::from_secs(29)).await;
                tokio::task::yield_now().await;
                let before = installs.borrow().len();
                let result = suspend_for_reconnect(
                    &cancel,
                    async {
                        assert!(
                            cancel.borrow().is_none(),
                            "actor cancelled before teardown await"
                        );
                        present.set(false);
                        manager.disconnect().await;
                        // Boundary at 30 and subsequent boundary at 60 fall while absent.
                        tokio::time::advance(std::time::Duration::from_secs(32)).await;
                        tokio::task::yield_now().await;
                        Err(CallError::ConnectionFailed("token fetch failed".into()))
                    },
                    || panic!("a failed retry must not restart"),
                )
                .await;
                assert!(result.is_err());
                assert_eq!(
                    installs.borrow().len(),
                    before,
                    "suspended timer never installs against absent room"
                );
                let guard = active.borrow();
                let state = guard.as_ref().unwrap();
                assert_eq!(state.key_state.current_key(), original.current_key());
                assert_eq!(state.key_state.anchor(), original.anchor());
                assert_eq!(state.call.started_at, Some(anchor));
                assert!(state.publish_gate.is_closed());
                drop(guard);
                let mut candidate = original.clone();
                // Retry crosses additional epochs during the SDK-connect await.
                let result = suspend_for_reconnect(
                    &cancel,
                    async {
                        tokio::time::advance(std::time::Duration::from_secs(61)).await;
                        tokio::task::yield_now().await;
                        let target = aligned_ratchet_schedule(
                            (clock() - anchor).num_milliseconds() as f64 / 1000.0,
                            30,
                        )
                        .0;
                        assert_eq!(target, 4);
                        present.set(true); // provisional room, publication still gated
                        install_reconnect_catch_up(&mut candidate, target, |e, c, n| {
                            sink(e, c, n)
                        })?;
                        let mut guard = active.borrow_mut();
                        let state = guard.as_mut().unwrap();
                        assert!(state.publish_gate.is_closed());
                        state.key_state = candidate.clone();
                        Ok(())
                    },
                    || start_key_ratchet_scheduler(&cancel, &active, sink.clone(), clock.clone()),
                )
                .await;
                assert!(result.is_ok());
                tokio::task::yield_now().await;
                assert_eq!(&*installs.borrow(), &[1, 2, 3, 4]);
                let mut expected = original.clone();
                while expected.epoch() <= 5 {
                    let reference = KeyProvider::with_shared_key(
                        KeyProviderOptions::default(),
                        expected.current_key_vec(),
                    );
                    assert_eq!(
                        provider
                            .get_shared_key((expected.epoch() % 16) as i32)
                            .expect("installed native receive key"),
                        reference.get_shared_key(0).expect("native expected key"),
                        "native receive material for outage epoch {}",
                        expected.epoch()
                    );
                    expected.ratchet().unwrap();
                }
                assert_eq!(provider.get_latest_key_index(), 4);
                // Replacing a live timer still leaves exactly one boundary install.
                start_key_ratchet_scheduler(&cancel, &active, sink.clone(), clock.clone());
                tokio::task::yield_now().await;
                tokio::time::advance(std::time::Duration::from_secs(28)).await;
                tokio::task::yield_now().await;
                assert_eq!(&*installs.borrow(), &[1, 2, 3, 4, 5]);
                tokio::time::advance(std::time::Duration::from_secs(30)).await;
                tokio::task::yield_now().await;
                assert_eq!(&*installs.borrow(), &[1, 2, 3, 4, 5, 6]);
                let guard = active.borrow();
                let state = guard.as_ref().unwrap();
                assert_eq!(state.key_state.epoch(), 6);
                assert_eq!(state.key_state.anchor(), original.anchor());
                assert_eq!(state.call.started_at, Some(anchor));
                assert!(state.publish_gate.is_closed());
                assert!(state.publish_gate.mic.is_some());
                stop_key_ratchet_timer(&cancel);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn multi_epoch_outage_across_ring_wrap_installs_previous_current_next() {
        let mut chain = CallKeyState::new(&[7; 32], "ring-wrap").unwrap();
        let original = chain.clone();
        let provider = KeyProvider::with_shared_key(
            KeyProviderOptions {
                ratchet_window_size: 0,
                key_ring_size: 16,
                ..Default::default()
            },
            chain.current_key_vec(),
        );
        install_reconnect_catch_up(&mut chain, 18, |epoch, current, next| {
            provider.set_shared_key(next, ((epoch + 1) % 16) as i32);
            provider.set_shared_key(current, (epoch % 16) as i32);
            true
        })
        .unwrap();
        assert_eq!(chain.epoch(), 18);
        assert_eq!(chain.anchor(), original.anchor());
        assert_eq!(provider.get_latest_key_index(), 2);
        let mut expected = original;
        for epoch in 0..=19 {
            if epoch >= 4 {
                // last 16 receive epochs: previous window + current + next
                let reference = KeyProvider::with_shared_key(
                    KeyProviderOptions::default(),
                    expected.current_key_vec(),
                );
                assert_eq!(
                    provider
                        .get_shared_key((epoch % 16) as i32)
                        .expect("installed wrapped receive key"),
                    reference
                        .get_shared_key(0)
                        .expect("native expected wrapped key"),
                    "epoch {epoch}"
                );
            }
            expected.ratchet().unwrap();
        }
    }

    #[test]
    fn partial_catch_up_rejection_keeps_authority_and_retries_all_receive_steps() {
        let mut chain = CallKeyState::new(&[7; 32], "retry").unwrap();
        chain.rotate([9; 32]).unwrap();
        let original = chain.clone();
        assert!(install_reconnect_catch_up(&mut chain, 4, |epoch, _, _| epoch != 3).is_err());
        assert_eq!(chain.epoch(), original.epoch());
        assert_eq!(chain.anchor(), original.anchor());
        assert_eq!(chain.current_key(), original.current_key());
        let mut installed = Vec::new();
        install_reconnect_catch_up(&mut chain, 4, |epoch, _, _| {
            installed.push(epoch);
            true
        })
        .unwrap();
        assert_eq!(installed, [1, 2, 3, 4]);
        assert_eq!(chain.epoch(), 4);
        assert_eq!(chain.anchor(), 1);
        let before = chain.clone();
        assert!(
            install_reconnect_catch_up(&mut chain, MAX_RATCHET_CATCH_UP + 5, |_, _, _| panic!(
                "preflight must reject before native mutation"
            ))
            .is_err()
        );
        assert_eq!(chain.current_key(), before.current_key());
    }
}

#[cfg(test)]
mod reconnect_camera_tests {
    use super::*;

    #[tokio::test]
    async fn nondefault_camera_arguments_survive_failed_attempts_and_restore() {
        let config = VideoCaptureConfig {
            width: 1920,
            height: 1080,
            fps: 30,
        };
        let mut saved = None;
        remember_reconnect_media(&mut saved, || ReconnectMedia {
            microphone: Some("selected-mic".into()),
            camera: Some(("/dev/video7".into(), config.clone())),
            muted: true,
        });
        // Production retry boundary must not consult torn-down capture state.
        for _ in 0..2 {
            remember_reconnect_media(&mut saved, || panic!("replaced preserved intent"));
        }
        let media = saved.as_ref().unwrap();
        assert!(media.muted);
        assert_eq!(media.microphone.as_deref(), Some("selected-mic"));
        let mut observed = None;
        restore_camera_with(media.camera.clone(), |device, settings| {
            observed = Some((device, settings));
            std::future::ready(Ok(()))
        })
        .await
        .unwrap();
        assert_eq!(observed, Some(("/dev/video7".into(), config)));
    }
}

#[cfg(test)]
mod verification_snapshot_tests {
    use super::*;

    fn active(root: [u8; 32]) -> ActiveCallState {
        let mut call = Call::new(ChannelId::new(), UserId::new(), CallType::Video);
        call.id = "11111111-2222-4333-8444-555555555555".parse().unwrap();
        ActiveCallState {
            key_state: CallKeyState::new(&root, &call.id.to_string()).unwrap(),
            call,
            livekit_url: String::new(),
            publish_gate: PublishGate::default(),
            reconnect_media: None,
            fresh_rejoin_used: false,
        }
    }

    fn orchestrator(state: &ActiveCallState, kind: OrchestratedCallKind) -> OrchestratorState {
        OrchestratorState {
            active: Some(OrchestratedActive {
                peer_id: None,
                session_revision: None,
                call: derive_call_handle(state),
                kind,
            }),
            ..Default::default()
        }
    }

    fn verification(state: &ActiveCallState) -> CallEncryptionVerification {
        call_verification_snapshot(
            Some(state),
            &orchestrator(state, OrchestratedCallKind::GroupChannel),
            true,
            state.key_state.epoch(),
        )
        .unwrap()
        .verification
    }

    #[test]
    fn peers_compare_installed_current_key_and_call_identity() {
        let mut first = active([1; 32]);
        let mut peer = active([1; 32]);
        assert_eq!(verification(&first), verification(&peer));
        let expected =
            crate::services::call_verification::CallVerification::derive_verification_emojis(
                first.key_state.current_key(),
                &first.call.id.to_string(),
            );
        if let CallEncryptionVerification::MediaKey { emojis, .. } = verification(&first) {
            assert_eq!(emojis.as_slice(), expected.emojis.as_slice());
        } else {
            panic!("connected keyed channel must expose comparison code");
        }
        assert!(matches!(
            verification(&first),
            CallEncryptionVerification::MediaKey { epoch: 0, .. }
        ));
        first.key_state.rotate([2; 32]).unwrap();
        assert_ne!(verification(&first), verification(&peer));
        adopt_group_key(&mut peer.key_state, [2; 32], 1).unwrap();
        assert_eq!(verification(&first), verification(&peer));
        first.key_state.ratchet().unwrap();
        peer.key_state.ratchet().unwrap();
        assert_eq!(verification(&first), verification(&peer));
        peer.call.id = "22222222-2222-4333-8444-555555555555".parse().unwrap();
        assert_eq!(
            verification(&peer),
            CallEncryptionVerification::WaitingForKey
        );
        peer.key_state = CallKeyState::new(&[1; 32], &peer.call.id.to_string()).unwrap();
        assert_ne!(verification(&active([1; 32])), verification(&peer));
        let wrong_key = active([3; 32]);
        assert_ne!(verification(&active([1; 32])), verification(&wrong_key));
    }

    #[test]
    fn keyless_disconnected_and_epoch_mismatch_suppress_code() {
        let mut state = active([1; 32]);
        let orch = orchestrator(&state, OrchestratedCallKind::GroupChannel);
        assert_eq!(
            call_verification_snapshot(Some(&state), &orch, false, 0)
                .unwrap()
                .verification,
            CallEncryptionVerification::WaitingForKey
        );
        assert_eq!(
            call_verification_snapshot(Some(&state), &orch, true, 1)
                .unwrap()
                .verification,
            CallEncryptionVerification::WaitingForKey
        );
        state.publish_gate = PublishGate::awaiting_key(false);
        assert_eq!(
            verification(&state),
            CallEncryptionVerification::WaitingForKey
        );
    }

    #[test]
    fn failed_install_then_committed_retry_and_reconnect_refresh_snapshot() {
        let mut state = active([1; 32]);
        state.publish_gate = PublishGate::awaiting_key(false);
        let orch = orchestrator(&state, OrchestratedCallKind::GroupChannel);
        let id = state.call.id;
        let active = Rc::new(RefCell::new(Some(state)));
        let (room, _events) = RoomManager::new();
        let room = Arc::new(room);
        assert!(install_group_media_key(&active, &room, None, id, [2; 32], 1).is_err());
        assert_eq!(
            call_verification_snapshot(
                active.borrow().as_ref(),
                &orch,
                room.is_connected(),
                room.key_epoch()
            )
            .unwrap()
            .verification,
            CallEncryptionVerification::WaitingForKey
        );
        // Model the authoritative state committed only after a successful SDK retry.
        let mut held = active.borrow_mut();
        let state = held.as_mut().unwrap();
        adopt_group_key(&mut state.key_state, [2; 32], 1).unwrap();
        state.publish_gate.open();
        assert_eq!(
            call_verification_snapshot(Some(state), &orch, false, 1)
                .unwrap()
                .verification,
            CallEncryptionVerification::WaitingForKey
        );
        assert!(matches!(
            call_verification_snapshot(Some(state), &orch, true, 1)
                .unwrap()
                .verification,
            CallEncryptionVerification::MediaKey { epoch: 1, .. }
        ));
    }

    #[test]
    fn ended_replaced_and_handshake_modes_follow_authoritative_identity() {
        let state = active([1; 32]);
        let orch = orchestrator(&state, OrchestratedCallKind::GroupChannel);
        assert!(call_verification_snapshot(None, &orch, true, 0).is_none());
        let mut replacement = active([2; 32]);
        replacement.call.id = CallId::new();
        assert_eq!(
            call_verification_snapshot(Some(&replacement), &orch, true, 0)
                .unwrap()
                .verification,
            CallEncryptionVerification::WaitingForKey
        );
        assert_eq!(
            call_verification_snapshot(
                Some(&state),
                &orchestrator(&state, OrchestratedCallKind::DmStandard),
                false,
                0
            )
            .unwrap()
            .verification,
            CallEncryptionVerification::ExistingHandshake
        );
    }
}


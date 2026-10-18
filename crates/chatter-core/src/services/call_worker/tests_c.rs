use super::*;

#[cfg(test)]
mod group_ring_cancel_tests {
    use super::*;

    #[derive(Default)]
    struct NotificationLog(parking_lot::Mutex<Vec<String>>);

    impl crate::system::notification_sink::NotificationSink for NotificationLog {
        fn show(
            &self,
            _: &str,
            _: &str,
            _: crate::domain::notification::NotificationPriority,
        ) -> Result<(), crate::error::SystemError> {
            Ok(())
        }
        fn show_incoming_call(
            &self,
            call: &str,
            _: &str,
            _: &str,
        ) -> Result<(), crate::error::SystemError> {
            self.0.lock().push(format!("show {call}"));
            Ok(())
        }
        fn cancel_incoming_call(&self, call: &str) -> Result<(), crate::error::SystemError> {
            self.0.lock().push(format!("cancel {call}"));
            Ok(())
        }
    }

    fn worker() -> CallWorkerHandle {
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
        CallWorkerHandle::spawn(crate::network::grpc::CallClient::new(
            channel.into(),
            crate::network::grpc::interceptors::AuthInterceptor::new(),
        ))
    }

    async fn present(worker: &CallWorkerHandle, call: &CallHandle) {
        let mut events = worker.subscribe_call_events();
        worker.orch_present_incoming(
            call.clone(),
            OrchestratedCallKind::GroupChannel,
            UserId::new(),
            "Caller".into(),
            false,
            false,
            String::new(),
            None,
            Some("Room".into()),
            "channel".into(),
        );
        assert!(matches!(
            events.recv().await.unwrap(),
            CallOrchestratorEvent::IncomingPresented { .. }
        ));
    }

    #[tokio::test]
    async fn real_actor_rejects_retired_completion_and_reset_across_a_b_a_sessions() {
        let auth = crate::state::AuthState::new();
        let worker = worker();
        let alice = UserId::new();
        let bob = UserId::new();
        auth.set_logged_in(alice, 1, "Alice".into());
        let a = auth.get().session_revision;
        worker.incoming_presentation_state().begin_session(a);
        let old = worker.for_presentation_session(a);
        present(&old, &CallHandle::from("old-ring")).await;
        auth.invalidate_session();
        worker.reset_presentation_session(a).await.unwrap();
        auth.set_logged_in(bob, 1, "Bob".into());
        let b = auth.get().session_revision;
        worker.incoming_presentation_state().begin_session(b);
        let current = worker.for_presentation_session(b);
        present(&current, &CallHandle::from("new-ring")).await;
        current.orch_mark_connected(
            CallHandle::from("new-active"),
            OrchestratedCallKind::DmStandard,
            bob,
            false,
        );
        worker.orch_active_kind().await;
        // Deliberately enqueue after teardown: actor execution must reject it,
        // regardless of what an earlier detached future checked before await.
        old.orch_mark_connected(
            CallHandle::from("old-active"),
            OrchestratedCallKind::DmStandard,
            alice,
            false,
        );
        old.orch_mark_ended(CallHandle::from("new-active"), CallEndReason::RemoteHangup);
        worker.orch_active_kind().await;
        assert!(matches!(
            old.orch_mark_connected_reliably(
                CallHandle::from("late-ack"),
                OrchestratedCallKind::DmStandard,
                alice,
                false
            )
            .await,
            Err(CallError::WorkerShutdown)
        ));
        let mut restored = false;
        assert!(
            current
                .with_current_presentation_session(|| {
                    restored = true;
                })
                .is_some()
        );
        assert!(restored);
        assert!(worker.is_current_active_presentation(b, "new-active"));
        assert_eq!(
            worker.reset_presentation_session(a).await.unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            worker
                .incoming_presentation_snapshot()
                .await
                .unwrap()
                .presentations
                .len(),
            1
        );
        auth.invalidate_session();
        worker.reset_presentation_session(b).await.unwrap();
        auth.set_logged_in(alice, 1, "Alice again".into());
        let again = auth.get().session_revision;
        assert_ne!(again, a);
        worker.incoming_presentation_state().begin_session(again);
        assert!(!auth.session_is_current(a));
        assert!(
            old.with_current_presentation_session(|| panic!("stale secret restore"))
                .is_none()
        );
        present(
            &worker.for_presentation_session(again),
            &CallHandle::from("alice-new-ring"),
        )
        .await;
        worker.reset_presentation_session(a).await.unwrap();
        assert_eq!(
            worker
                .incoming_presentation_snapshot()
                .await
                .unwrap()
                .presentations
                .len(),
            1
        );
        worker.shutdown();
    }

    #[tokio::test]
    async fn saturated_queue_withdrawal_waits_then_completes_once() {
        let worker = worker();
        let log = Arc::new(NotificationLog::default());
        let bridge = super::super::call_notification_bridge::spawn_call_notification_bridge(
            worker.clone(),
            Arc::new(super::super::notification_service::NotificationService::new(log.clone())),
        );
        worker.orch_mark_connected(
            CallHandle::from("unrelated-active"),
            OrchestratedCallKind::DmStandard,
            UserId::new(),
            false,
        );
        assert_eq!(
            worker.orch_active_kind().await,
            Some(OrchestratedCallKind::DmStandard)
        );
        let first = CallHandle::from("old-ring");
        let next = CallHandle::from("new-ring");
        present(&worker, &first).await;
        present(&worker, &next).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !log.0.lock().iter().any(|event| event == "show new-ring") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut events = worker.subscribe_call_events();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        worker
            .cmd_tx
            .send(CallCommand::TestPause {
                entered: entered_tx,
                resume: resume_rx,
            })
            .await
            .unwrap();
        entered_rx.await.unwrap();
        for _ in 0..256 {
            let (response, _) = oneshot::channel();
            worker
                .cmd_tx
                .try_send(CallCommand::OrchActiveKind { response })
                .unwrap();
        }
        assert_eq!(worker.cmd_tx.capacity(), 0);
        let withdrawal =
            worker.orch_withdraw_incoming_reliably(first.clone(), CallEndReason::RemoteHangup);
        tokio::pin!(withdrawal);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut withdrawal)
                .await
                .is_err()
        );
        resume_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut withdrawal)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(events.recv().await.unwrap(), CallOrchestratorEvent::IncomingWithdrawn { call, .. } if call == first)
        );
        worker
            .orch_withdraw_incoming_reliably(first, CallEndReason::NoAnswer)
            .await
            .unwrap();
        assert!(events.try_recv().is_err());
        assert_eq!(
            worker.orch_active_kind().await,
            Some(OrchestratedCallKind::DmStandard)
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if log.0.lock().iter().any(|event| event == "cancel old-ring") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            *log.0.lock(),
            ["show old-ring", "show new-ring", "cancel old-ring"]
        );
        worker
            .orch_accept_incoming_replacing_active(next)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !log.0.lock().iter().any(|event| event == "cancel new-ring") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            *log.0.lock(),
            [
                "show old-ring",
                "show new-ring",
                "cancel old-ring",
                "cancel new-ring"
            ]
        );
        bridge.abort();
    }

    #[tokio::test]
    async fn shared_invite_accept_cancel_orders_preserve_newest_keys_before_join_or_replace() {
        use super::super::call_signaling::GroupCallInvites;
        for replaces_active in [false, true] {
            for cancel_first in [false, true] {
                let worker = worker();
                let call = CallHandle::from("ring");
                present(&worker, &call).await;
                let unrelated = CallHandle::from("unrelated-active");
                if replaces_active {
                    worker.orch_mark_connected(
                        unrelated,
                        OrchestratedCallKind::DmStandard,
                        UserId::new(),
                        false,
                    );
                    assert_eq!(
                        worker.orch_active_kind().await,
                        Some(OrchestratedCallKind::DmStandard)
                    );
                }
                let sender = UserId::new();
                let invites = Arc::new(GroupCallInvites::default());
                invites.store_group_call_invite(call.as_str());
                assert!(invites.buffer_group_call_key(call.as_str(), sender, [8; 32], 8));
                assert!(invites.buffer_group_call_key(call.as_str(), sender, [9; 32], 9));
                assert!(invites.buffer_group_call_key(call.as_str(), sender, [7; 32], 7));
                if cancel_first {
                    let (entered_tx, entered_rx) = oneshot::channel();
                    let (resume_tx, resume_rx) = oneshot::channel();
                    worker
                        .cmd_tx
                        .send(CallCommand::TestPause {
                            entered: entered_tx,
                            resume: resume_rx,
                        })
                        .await
                        .unwrap();
                    entered_rx.await.unwrap();
                    let accept = worker.orch_accept_presented(
                        call.clone(),
                        replaces_active,
                        invites.clone(),
                    );
                    tokio::pin!(accept);
                    assert!(
                        tokio::time::timeout(std::time::Duration::from_millis(20), &mut accept)
                            .await
                            .is_err()
                    );
                    // Accept is enqueued, but cancellation wins the shared lock
                    // before the blocked actor commits it. Withdrawal is not yet queued.
                    invites.drop_ringing_group_call_invite(call.as_str());
                    resume_tx.send(()).unwrap();
                    assert!(matches!(accept.await, Err(CallError::NotInCall)));
                    assert_eq!(
                        worker.orch_active_kind().await,
                        replaces_active.then_some(OrchestratedCallKind::DmStandard)
                    );
                    worker
                        .orch_withdraw_incoming_reliably(call.clone(), CallEndReason::RemoteHangup)
                        .await
                        .unwrap();
                    assert!(invites.lock().is_empty());
                } else {
                    let (accepted_tx, accepted_rx) = oneshot::channel();
                    let (continue_tx, continue_rx) = oneshot::channel();
                    let accepting_worker = worker.clone();
                    let accepting_invites = invites.clone();
                    let accepting_call = call.clone();
                    let accept = tokio::spawn(async move {
                        let snapshot = accepting_worker
                            .orch_accept_presented(
                                accepting_call,
                                replaces_active,
                                accepting_invites,
                            )
                            .await
                            .unwrap();
                        accepted_tx.send(()).unwrap();
                        // Pause the actual production acceptance boundary. The
                        // client cannot enter end_call/join until this reply.
                        continue_rx.await.unwrap();
                        snapshot
                    });
                    accepted_rx.await.unwrap();
                    invites.drop_ringing_group_call_invite(call.as_str());
                    worker
                        .orch_withdraw_incoming_reliably(call.clone(), CallEndReason::NoAnswer)
                        .await
                        .unwrap();
                    continue_tx.send(()).unwrap();
                    let snapshot = accept.await.unwrap();
                    assert_eq!(snapshot.kind, OrchestratedCallKind::GroupChannel);
                    assert!(!invites.is_ringing_group_call_invite(call.as_str()));
                    let keys = invites.take_group_call_invite(call.as_str());
                    assert_eq!(keys.len(), 1);
                    assert_eq!(keys[0].sender, sender);
                    assert_eq!(keys[0].epoch, 9);
                    assert_eq!(*keys[0].key, [9; 32]);
                }
            }
        }
    }

    #[tokio::test]
    async fn failed_actor_accept_does_not_mark_signaling_accepted() {
        use super::super::call_signaling::GroupCallInvites;
        let worker = worker();
        let call = CallHandle::from("pending");
        present(&worker, &call).await;
        worker.orch_mark_connected(
            CallHandle::from("active"),
            OrchestratedCallKind::DmStandard,
            UserId::new(),
            false,
        );
        let invites = Arc::new(GroupCallInvites::default());
        invites.store_group_call_invite(call.as_str());
        assert!(matches!(
            worker
                .orch_accept_presented(call.clone(), false, invites.clone())
                .await,
            Err(CallError::AlreadyInCall)
        ));
        assert!(invites.is_ringing_group_call_invite(call.as_str()));
        invites.drop_ringing_group_call_invite(call.as_str());
        worker
            .orch_withdraw_incoming_reliably(call, CallEndReason::NoAnswer)
            .await
            .unwrap();
        assert!(invites.lock().is_empty());
        assert_eq!(
            worker.orch_active_kind().await,
            Some(OrchestratedCallKind::DmStandard)
        );
    }

    #[tokio::test]
    async fn cancellation_withdraws_matching_pending_and_preserves_active_orchestration() {
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
        let worker = CallWorkerHandle::spawn(crate::network::grpc::CallClient::new(
            channel.into(),
            crate::network::grpc::interceptors::AuthInterceptor::new(),
        ));
        let mut events = worker.subscribe_call_events();
        let first = CallHandle::from("group-first");
        let second = CallHandle::from("group-second");
        for call in [&first, &second] {
            worker.orch_present_incoming(
                call.clone(),
                OrchestratedCallKind::GroupChannel,
                UserId::new(),
                "Caller".into(),
                false,
                false,
                String::new(),
                None,
                Some("Shared room".into()),
                "channel".into(),
            );
            assert!(matches!(
                tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                    .await
                    .unwrap()
                    .unwrap(),
                CallOrchestratorEvent::IncomingPresented { .. }
            ));
        }
        worker.orch_withdraw_incoming(first.clone(), CallEndReason::RemoteHangup);
        assert!(
            matches!(tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await.unwrap().unwrap(), CallOrchestratorEvent::IncomingWithdrawn { call, reason: CallEndReason::RemoteHangup } if call == first)
        );
        // Duplicate/stale end cannot touch the next invite. A queued accept
        // wins against later cancellation: withdrawal owns incoming only.
        worker.orch_withdraw_incoming(first, CallEndReason::RemoteHangup);
        worker.orch_accept_incoming(second.clone()).await.unwrap();
        worker.orch_mark_connected(
            second.clone(),
            OrchestratedCallKind::GroupChannel,
            UserId::new(),
            false,
        );
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap(),
            CallOrchestratorEvent::ActiveConnected { .. }
        ));
        worker.orch_withdraw_incoming(second, CallEndReason::RemoteHangup);
        assert_eq!(
            worker.orch_active_kind().await,
            Some(OrchestratedCallKind::GroupChannel)
        );
        assert!(
            events.try_recv().is_err(),
            "no terminal orchestrator event after accepting"
        );
    }
}

#[cfg(test)]
mod setup_cancellation_actor_tests {
    use super::*;
    fn worker() -> CallWorkerHandle {
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
        CallWorkerHandle::spawn(crate::network::grpc::CallClient::new(
            channel.into(),
            crate::network::grpc::interceptors::AuthInterceptor::new(),
        ))
    }
    #[tokio::test]
    async fn cancelled_setup_command_never_enters_server_or_media_setup() {
        let worker = worker();
        let attempt = crate::client::calls::CallStartAttempt::default();
        attempt.cancel();
        let result = crate::client::calls::START_ATTEMPT
            .scope(
                attempt,
                worker.initiate_call(
                    ChannelId::new(),
                    "fixture",
                    CallType::Voice,
                    &[0; 32],
                    UserId::new(),
                ),
            )
            .await;
        assert!(matches!(result, Err(CallError::CallEnded)));
        assert!(worker.active_call().await.is_none());
    }
    #[tokio::test]
    async fn late_cancelled_setup_rollback_cannot_disconnect_replacement() {
        let worker = worker();
        let old = CallHandle::from(CallId::new().to_string().as_str());
        let replacement = Call::new(ChannelId::new(), UserId::new(), CallType::Voice);
        let replacement_id = replacement.id;
        let (tx, rx) = oneshot::channel();
        worker
            .cmd_tx
            .send(CallCommand::TestPrimeCall {
                call: replacement,
                response: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap();
        worker.rollback_setup(old.clone()).await;
        assert!(matches!(
            worker
                .orch_mark_connected_reliably(
                    old,
                    OrchestratedCallKind::DmStandard,
                    UserId::new(),
                    false
                )
                .await,
            Err(CallError::CallEnded)
        ));
        assert_eq!(worker.active_call().await.unwrap().id, replacement_id);
        assert_eq!(
            worker.orch_active_kind().await,
            Some(OrchestratedCallKind::DmStandard)
        );
    }
}

#[cfg(test)]
mod incoming_expiry_tests {
    use super::*;
    use std::time::Duration;

    fn worker() -> CallWorkerHandle {
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
        CallWorkerHandle::spawn(CallClient::new(channel.into(), crate::network::grpc::interceptors::AuthInterceptor::new()))
    }

    async fn expire(worker: &CallWorkerHandle, elapsed: Duration) {
        let (response, rx) = oneshot::channel();
        worker.cmd_tx.send(CallCommand::TestExpireIncoming { elapsed, response }).await.unwrap();
        rx.await.unwrap();
    }

    async fn present(worker: &CallWorkerHandle, call: &str, kind: OrchestratedCallKind, timestamp: u64) {
        worker.orch_present_incoming_offer(CallHandle::from(call), kind, UserId::new(), "Caller".into(), false,
            true, String::new(), None, None, "conversation".into(), timestamp);
        // FIFO snapshot is an actor barrier, not a wall-clock sleep.
        worker.incoming_presentation_snapshot().await.unwrap();
    }

    struct NotificationProbe(tokio::sync::mpsc::UnboundedSender<String>);
    impl crate::system::notification_sink::NotificationSink for NotificationProbe {
        fn show(&self, _: &str, _: &str, _: crate::domain::notification::NotificationPriority) -> Result<(), crate::error::SystemError> { Ok(()) }
        fn show_incoming_call(&self, call: &str, _: &str, _: &str) -> Result<(), crate::error::SystemError> {
            let _ = self.0.send(format!("show {call}")); Ok(())
        }
        fn cancel_incoming_call(&self, call: &str) -> Result<(), crate::error::SystemError> {
            let _ = self.0.send(format!("cancel {call}")); Ok(())
        }
    }

    #[tokio::test]
    async fn incoming_expiry_snapshot_cancels_native_notification() {
        let worker = worker();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = crate::services::call_notification_bridge::spawn_call_notification_bridge(
            worker.clone(), Arc::new(crate::services::notification_service::NotificationService::new(Arc::new(NotificationProbe(tx)))));
        present(&worker, "notification", OrchestratedCallKind::DmStandard, 0).await;
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap(), "show notification");
        expire(&worker, Duration::from_secs(60)).await;
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap(), "cancel notification");
        assert!(worker.incoming_presentation_snapshot().await.unwrap().presentations.is_empty());
        bridge.abort();
        worker.shutdown();
    }

    #[test]
    fn incoming_expiry_budget_bounds_clock_skew_and_boundary() {
        let now = 1_800_000_000_000_000_000;
        assert_eq!(incoming_ring_budget(now - 59_000_000_000, now), Duration::from_secs(1));
        assert_eq!(incoming_ring_budget(now - 60_000_000_000, now), Duration::ZERO);
        assert_eq!(incoming_ring_budget(now - 61_000_000_000, now), Duration::ZERO);
        for stamp in [0, now, now + 1, u64::MAX] {
            assert_eq!(incoming_ring_budget(stamp, now), RING_TIMEOUT);
        }
    }

    #[tokio::test]
    async fn incoming_expiry_real_actor_all_transports_missing_cancel_exactly_once() {
        for kind in [OrchestratedCallKind::DmStandard, OrchestratedCallKind::GroupChannel] {
            let worker = worker();
            let mut events = worker.subscribe_call_events();
            present(&worker, "missing-cancel", kind, 0).await;
            assert!(matches!(events.recv().await.unwrap(), CallOrchestratorEvent::IncomingPresented { .. }));
            expire(&worker, Duration::from_secs(59)).await;
            assert_eq!(worker.incoming_presentation_snapshot().await.unwrap().presentations.len(), 1);
            let (response, advanced) = oneshot::channel();
            worker.cmd_tx.send(CallCommand::TestAdvanceIncomingClock { elapsed: Duration::from_secs(60), response }).await.unwrap();
            advanced.await.unwrap();
            // Exercise the real sleep_until/select branch on the worker's own
            // paused clock, rather than invoking only the expiration helper.
            assert!(matches!(events.recv().await.unwrap(), CallOrchestratorEvent::IncomingWithdrawn { call, reason: CallEndReason::NoAnswer } if call.as_str() == "missing-cancel"));
            assert!(worker.incoming_presentation_snapshot().await.unwrap().presentations.is_empty());
            expire(&worker, Duration::from_secs(120)).await;
            assert!(events.try_recv().is_err(), "expiry must not record a second missed call");
            worker.shutdown();
        }
    }

    #[tokio::test]
    async fn incoming_expiry_age_duplicate_and_unrelated_active_are_preserved() {
        let worker = worker();
        let mut events = worker.subscribe_call_events();
        let active = CallHandle::from("active");
        worker.orch_mark_connected(active.clone(), OrchestratedCallKind::DmStandard, UserId::new(), false);
        let now = crate::util::time::TimestampNanos::now().as_u64();
        present(&worker, "old", OrchestratedCallKind::DmStandard, now - 59_000_000_000).await;
        present(&worker, "fresh", OrchestratedCallKind::DmStandard, 0).await;
        // Re-delivery with an unstamped/future clock cannot renew the old slot.
        present(&worker, "old", OrchestratedCallKind::DmStandard, u64::MAX).await;
        expire(&worker, Duration::from_secs(2)).await;
        let snapshot = worker.incoming_presentation_snapshot().await.unwrap();
        assert_eq!(snapshot.presentations.len(), 1);
        assert!(matches!(&snapshot.presentations[0], CallOrchestratorEvent::IncomingPresented { call, .. } if call.as_str() == "fresh"));
        assert_eq!(worker.orch_active_kind().await, Some(OrchestratedCallKind::DmStandard));
        let observed: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        assert_eq!(observed.iter().filter(|e| matches!(e, CallOrchestratorEvent::IncomingPresented { call, .. } if call.as_str() == "old")).count(), 1);
        assert_eq!(observed.iter().filter(|e| matches!(e, CallOrchestratorEvent::IncomingWithdrawn { call, reason: CallEndReason::NoAnswer } if call.as_str() == "old")).count(), 1);
        assert!(!observed.iter().any(|e| matches!(e, CallOrchestratorEvent::ActiveEnded { .. })));
        // Reusing the removed handle creates a new instance. The old short
        // deadline cannot target it because deadlines live in current slots.
        present(&worker, "old", OrchestratedCallKind::DmStandard, 0).await;
        expire(&worker, Duration::from_secs(2)).await;
        assert_eq!(worker.incoming_presentation_snapshot().await.unwrap().presentations.len(), 2);
        worker.shutdown();
    }

    #[tokio::test]
    async fn incoming_expiry_accept_decline_hangup_and_session_reset_fence_old_slots() {
        let worker = worker();
        let mut events = worker.subscribe_call_events();
        for call in ["accepted", "declined", "hung-up"] { present(&worker, call, OrchestratedCallKind::DmStandard, 0).await; }
        worker.orch_accept_incoming(CallHandle::from("accepted")).await.unwrap();
        worker.orch_decline_incoming(CallHandle::from("declined")).await.unwrap();
        worker.orch_withdraw_incoming_reliably(CallHandle::from("hung-up"), CallEndReason::RemoteHangup).await.unwrap();
        expire(&worker, Duration::from_secs(120)).await;
        assert!(!std::iter::from_fn(|| events.try_recv().ok()).any(|e| matches!(e, CallOrchestratorEvent::IncomingWithdrawn { reason: CallEndReason::NoAnswer, .. })));
        worker.incoming_presentation_state().begin_session(1);
        let old = worker.for_presentation_session(1);
        present(&old, "reused", OrchestratedCallKind::DmStandard, 0).await;
        worker.reset_presentation_session(1).await.unwrap();
        worker.incoming_presentation_state().begin_session(2);
        let next = worker.for_presentation_session(2);
        present(&next, "reused", OrchestratedCallKind::DmStandard, 0).await;
        // An old session's queued presentation cannot replace/renew the new slot.
        present(&old, "reused", OrchestratedCallKind::DmStandard, u64::MAX).await;
        expire(&worker, Duration::from_secs(59)).await;
        assert_eq!(next.incoming_presentation_snapshot().await.unwrap().presentations.len(), 1);
        expire(&worker, Duration::from_secs(60)).await;
        assert!(next.incoming_presentation_snapshot().await.unwrap().presentations.is_empty());
        worker.shutdown();
    }
}

use super::*;
#[cfg(feature = "media")]
use crate::calls::setup_diagnostics::{Phase, observe};

impl CallSignalingService {
    /// Start a DM call: generate ephemeral key, create call on server, send offer, store pending state.
    ///
    /// Returns the call ID. The call is not yet connected — that happens when
    /// `complete_dm_call()` is called after the peer sends their answer.
    #[cfg(feature = "media")]
    pub async fn start_dm_call(
        &self,
        peer_id: &UserId,
        channel_id: ChannelId,
        call_type: CallType,
        call_worker: &CallWorkerHandle,
        user_id: UserId,
    ) -> Result<CallId, CallSignalingError> {
        // Generate ephemeral keypair for PFS
        let ephemeral_key = EphemeralCallKey::generate();
        let ephemeral_public = ephemeral_key.public_key_bytes();

        let is_video = matches!(call_type, CallType::Video);
        let call_type_str = match call_type {
            CallType::Voice => "Voice Call",
            CallType::Video => "Video Call",
            CallType::Group => "Group Call",
        };

        tracing::info!(
            peer_id = %peer_id,
            "Generated ephemeral key for DM call, sending offer"
        );

        // Create call on server with placeholder key (will update when answer arrives)
        let placeholder_key = [0u8; 32];
        let call = call_worker
            .initiate_call(
                channel_id,
                call_type_str,
                call_type,
                &placeholder_key,
                user_id,
            )
            .await
            .map_err(|e| CallSignalingError::CallWorker(e.to_string()))?;

        if crate::client::calls::start_cancelled() {
            call_worker
                .rollback_setup(CallHandle::from(call.id.to_string().as_str()))
                .await;
            return Err(CallSignalingError::CallWorker(
                "call start cancelled".into(),
            ));
        }

        tracing::info!(
            "Created call on server: {:?}, waiting for peer's ephemeral key",
            call.id
        );

        // Send offer via sealed sender DM. `initiate_call` already connected
        // the LiveKit room and set the worker's active_call; if the offer
        // fails to send (e.g. first-contact DM bootstrap, or a network error)
        // we must roll that back. Otherwise the worker stays "in a call" with
        // a room nobody else joins, and every retry fails with
        // `AlreadyInCall` until the process restarts.
        let call_id_str = call.id.to_string();
        if let Err(e) = observe(
            Phase::DmOffer,
            self.send_call_offer(peer_id, &call_id_str, ephemeral_public, call_type_str),
        )
        .await
        {
            tracing::error!(error = %e, call_id = %call.id, "Call offer send failed — rolling back call");
            call_worker
                .rollback_setup(CallHandle::from(call_id_str.as_str()))
                .await;
            return Err(e);
        }

        tracing::info!(
            peer_id = %peer_id,
            call_id = %call.id,
            "Call offer sent, storing pending state"
        );

        self.register_start_attempt(&call_id_str);
        // Ringing from here on: register it so `end_active_call` can cancel
        // it (hangup + leave) before it connects. Registered before the
        // pending state is stored, so it precedes any answer's mark-connected
        // on the worker's queue.
        call_worker.orch_register_outgoing(
            CallHandle::from(call_id_str.as_str()),
            OrchestratedCallKind::DmStandard,
            *peer_id,
            String::new(),
            is_video,
        );

        // Store pending call state (ephemeral key kept until answer).
        self.set_pending_dm_call(PendingDmCall {
            call_id: call.id,
            channel_id,
            ephemeral_key,
            peer_id: *peer_id,
            call_type: call_type_str.to_string(),
            is_video,
        });

        // Signal's caller ring timeout: unanswered after RING_TIMEOUT, end it
        // exactly as a cancel does, so the callee stops ringing too. The
        // pending state is taken by the answer (`complete_dm_call`) and by any
        // end (`end_dm_call`), so still holding this call's means it rang out.
        let signaling = self.clone();
        let worker = call_worker.clone();
        let call_id = call.id;
        tokio::spawn(async move {
            tokio::time::sleep(RING_TIMEOUT).await;
            if signaling.start_was_cancelled(&call_id.to_string()) {
                return;
            }
            if signaling.pending_dm_call.lock().as_ref().map(|p| p.call_id) != Some(call_id) {
                return;
            }
            tracing::info!(call_id = %call_id, "Outgoing call unanswered — ending it");
            if let Err(e) = signaling
                .end_dm_call(&worker, CallEndReason::NoAnswer)
                .await
            {
                tracing::debug!(error = %e, "ring timeout: call already gone");
            }
        });

        Ok(call.id)
    }

    /// Complete an outgoing DM call after receiving the peer's ephemeral key answer.
    ///
    /// Consumes the pending call state, derives the shared secret via Signal service,
    /// updates LiveKit encryption, and returns verification data for the UI.
    #[cfg(feature = "media")]
    pub async fn complete_dm_call(
        &self,
        peer_ephemeral_pk: [u8; 32],
        call_id: &str,
        peer_id: &UserId,
        signal_service: &SignalService,
        call_worker: &CallWorkerHandle,
    ) -> Result<CallSetupResult, CallSignalingError> {
        self.complete_dm_call_with_crypto(call_id, peer_id, call_worker, |priv_key| async move {
            signal_service
                .actor
                .complete_call_key_exchange(
                    priv_key,
                    peer_ephemeral_pk,
                    *peer_id,
                    call_id.to_string(),
                    true,
                )
                .await
                .map_err(|e| CallSignalingError::KeyDerivationFailed(e.to_string()))
        })
        .await
    }

    #[cfg(feature = "media")]
    async fn complete_dm_call_with_crypto<F, Fut>(
        &self,
        call_id: &str,
        peer_id: &UserId,
        call_worker: &CallWorkerHandle,
        derive: F,
    ) -> Result<CallSetupResult, CallSignalingError>
    where
        F: FnOnce([u8; 32]) -> Fut,
        Fut: std::future::Future<Output = Result<[u8; 32], CallSignalingError>>,
    {
        let _ownership = self.start_gate.lock().await;
        let attempt = self.start_attempts.lock().get(call_id).cloned();
        let call_worker = &call_worker.for_dm_answer(CallHandle::from(call_id), attempt.clone());
        let cancelled = || {
            attempt.as_ref().is_some_and(|a| a.is_cancelled())
                || !call_worker.presentation_session_is_current()
        };
        if cancelled() {
            return Err(CallSignalingError::NoPendingCall);
        }
        // Take pending call state
        let pending = self
            .take_pending_dm_call()
            .ok_or(CallSignalingError::NoPendingCall)?;

        // Verify call ID matches
        if pending.call_id.to_string() != call_id {
            let expected = pending.call_id.to_string();
            // Put it back since this wasn't for us
            call_worker.with_current_presentation_session(|| self.set_pending_dm_call(pending));
            return Err(CallSignalingError::CallIdMismatch {
                expected,
                actual: call_id.to_string(),
            });
        }

        // `peer_id` comes from the authenticated Signal sender. A matching
        // server-assigned call ID alone is not proof this is our callee.
        if pending.peer_id != *peer_id {
            call_worker.with_current_presentation_session(|| self.set_pending_dm_call(pending));
            return Err(CallSignalingError::PeerMismatch);
        }

        tracing::info!(
            call_id = %call_id,
            peer_id = %peer_id,
            "Received call answer, deriving call key"
        );

        // Capture call view-data before `pending` is dropped (zeroizes the key).
        let is_video = pending.is_video;

        // Delegate all crypto to the Signal service (ECDH + identity lookup + HKDF)
        let priv_key = pending.ephemeral_key.private_key_bytes();
        let call_key = derive(priv_key).await?;

        if cancelled() {
            call_worker.rollback_setup(CallHandle::from(call_id)).await;
            return Err(CallSignalingError::NoPendingCall);
        }
        // Ephemeral key dropped here (zeroized)
        drop(pending);
        tracing::debug!("Ephemeral private key zeroized");

        tracing::info!(
            call_id = %call_id,
            "Call key derived successfully, updating LiveKit encryption"
        );

        // Update LiveKit encryption key
        let key_installed = call_worker.set_encryption_key(call_key.to_vec()).await;

        if cancelled() {
            // A retired session must not notify peers through replacement credentials.
            if call_worker.presentation_session_is_current() {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    self.send_call_hangup_to(peer_id, call_id, CallHangupReason::Normal),
                )
                .await;
            }
            call_worker.rollback_setup(CallHandle::from(call_id)).await;
            return Err(CallSignalingError::NoPendingCall);
        }

        // the DM caller's auto-publish was deferred at initiate time
        // (media would otherwise have leaked under the all-zero placeholder
        // key). Now that the real ECDH key is installed at LiveKit index 0,
        // this is the first safe moment to publish — do it exactly once here.
        // Guard on install success: if the re-root failed the room is still on
        // the placeholder key, so we must not publish.
        if key_installed {
            call_worker.publish_microphone(None).await.ok();
            if is_video && !cancelled() {
                call_worker.publish_camera(None).await.ok();
            }
        } else {
            tracing::error!(
                call_id = %call_id,
                "Real call key install failed — not publishing media to avoid placeholder-key leak"
            );
            call_worker.rollback_setup(CallHandle::from(call_id)).await;
            return Err(CallSignalingError::CallWorker(
                "call key installation failed".into(),
            ));
        }

        // Promote the ringing outgoing call (registered in `start_dm_call`) to
        // the orchestrator's active slot. The callee registers its slot via
        // `accept_presented_incoming_call`; this is the caller's equivalent.
        if cancelled() {
            call_worker.rollback_setup(CallHandle::from(call_id)).await;
            return Err(CallSignalingError::NoPendingCall);
        }
        call_worker
            .orch_mark_connected_reliably(
                crate::services::call_worker::CallHandle::from(call_id),
                crate::services::call_worker::OrchestratedCallKind::DmStandard,
                *peer_id,
                is_video,
            )
            .await
            .map_err(|e| CallSignalingError::CallWorker(e.to_string()))?;
        if cancelled() {
            call_worker.rollback_setup(CallHandle::from(call_id)).await;
            return Err(CallSignalingError::NoPendingCall);
        }

        // Derive verification emojis
        let verification = CallVerification::derive_verification_emojis(&call_key, call_id);
        let (e1, e2, e3, e4) = CallVerification::to_slint_tuple(&verification);

        let call_id_parsed = call_id
            .parse::<CallId>()
            .map_err(|_| CallSignalingError::KeyDerivationFailed("Invalid call ID".to_string()))?;

        Ok(CallSetupResult {
            call_id: call_id_parsed,
            channel_id: ChannelId::new(), // channel_id was consumed with pending
            verification_emojis: [e1, e2, e3, e4],
            is_video,
            // The call connects now — the answer just arrived. Counting from
            // the server's dial time ran the timer through the ring (and
            // through any server/device clock skew).
            started_at_ns: crate::util::time::TimestampNanos::now().as_u64(),
        })
    }

    /// Accept an incoming DM call: generate ephemeral key, derive shared secret, send answer, join call.
    ///
    /// Performs the callee's side of the ephemeral key exchange. Returns
    /// verification data for the UI.
    #[cfg(feature = "media")]
    pub async fn accept_dm_call(
        &self,
        caller_ephemeral_pk: [u8; 32],
        call_id: &str,
        caller_id: &UserId,
        signal_service: &SignalService,
        call_worker: &CallWorkerHandle,
        user_id: UserId,
    ) -> Result<CallSetupResult, CallSignalingError> {
        // Generate our ephemeral keypair
        let our_ephemeral = EphemeralCallKey::generate();
        let our_ephemeral_pk = our_ephemeral.public_key_bytes();

        // Delegate all crypto to the Signal service (ECDH + identity lookup + HKDF)
        let priv_key = our_ephemeral.private_key_bytes();
        let encryption_key = observe(
            Phase::DmCrypto,
            signal_service.actor.complete_call_key_exchange(
                priv_key,
                caller_ephemeral_pk,
                *caller_id,
                call_id.to_string(),
                false, // we_are_caller = false (we're the callee)
            ),
        )
        .await
        .map_err(|e| CallSignalingError::KeyDerivationFailed(e.to_string()))?;

        tracing::info!(
            call_id = %call_id,
            caller_id = %caller_id,
            "Derived call key, sending answer"
        );

        // Send answer with our ephemeral public key
        observe(
            Phase::DmAnswer,
            self.send_call_answer(caller_id, call_id, our_ephemeral_pk),
        )
        .await?;

        // Ephemeral key dropped here (zeroized)
        drop(our_ephemeral);
        tracing::debug!("Ephemeral private key zeroized");

        // Parse call ID
        let call_id_parsed = call_id
            .parse::<CallId>()
            .map_err(|_| CallSignalingError::KeyDerivationFailed("Invalid call ID".to_string()))?;

        // Join the call with derived key
        let call = call_worker
            .join_call(call_id_parsed, &encryption_key, user_id)
            .await
            .map_err(|e| CallSignalingError::CallWorker(e.to_string()))?;

        // Derive verification emojis
        let verification =
            CallVerification::derive_verification_emojis(&encryption_key, &call.id.to_string());
        let (e1, e2, e3, e4) = CallVerification::to_slint_tuple(&verification);

        Ok(CallSetupResult {
            call_id: call.id,
            // Already the DM with the caller: `join_call` rewrites the
            // caller-scoped id (= us) — see `joined_channel_id`.
            channel_id: call.channel_id,
            verification_emojis: [e1, e2, e3, e4],
            is_video: false, // caller provides this info via the offer
            // Connected as of the join above; the server's `started_at` is the
            // caller's dial time, which would count the ring.
            started_at_ns: crate::util::time::TimestampNanos::now().as_u64(),
        })
    }
}
#[cfg(all(test, feature = "media", feature = "testing"))]
mod named_dm_completion_tests {
    use super::*;
    use crate::{
        client::{ChatterClient, calls::CallStartAttempt},
        domain::call::{Call, CallType},
        services::call_worker::CallOrchestratorEvent,
    };
    use tokio::sync::oneshot;

    fn fixture() -> (tempfile::TempDir, Arc<ChatterClient>, CallWorkerHandle, u64) {
        let dir = tempfile::tempdir().unwrap();
        let dirs = crate::system::Directories::with_base_dir(dir.path().to_owned()).unwrap();
        std::fs::write(
            dirs.database_path()
                .parent()
                .unwrap()
                .join(crate::system::keyring::NO_KEYRING_MARKER),
            b"",
        )
        .unwrap();
        let client = ChatterClient::builder()
            .data_dir(dir.path().to_string_lossy())
            .build()
            .unwrap();
        client
            .auth_state()
            .set_logged_in(UserId::new(), 1, "Fixture".into());
        let revision = client.auth_state().get().session_revision;
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
        let worker = CallWorkerHandle::spawn(crate::network::grpc::CallClient::new(
            channel.into(),
            crate::network::grpc::interceptors::AuthInterceptor::new(),
        ));
        worker.incoming_presentation_state().begin_session(revision);
        (dir, client, worker, revision)
    }

    fn pending(service: &CallSignalingService, call: &Call, attempt: &CallStartAttempt) {
        service
            .start_attempts
            .lock()
            .insert(call.id.to_string(), attempt.clone());
        service.set_pending_dm_call(PendingDmCall {
            call_id: call.id,
            channel_id: call.channel_id,
            ephemeral_key: EphemeralCallKey::generate(),
            peer_id: call.initiator_id,
            call_type: "voice".into(),
            is_video: false,
        });
    }

    fn no_connected(events: &mut tokio::sync::broadcast::Receiver<CallOrchestratorEvent>) {
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, CallOrchestratorEvent::ActiveConnected { .. }),
                "retired answer advertised connected"
            );
        }
    }

    #[tokio::test]
    async fn dm_answer_from_other_peer_keeps_pending_and_never_derives() {
        let (_dir, client, worker, revision) = fixture();
        let service = client.call_signaling();
        let call = Call::new(ChannelId::new(), UserId::new(), CallType::Voice);
        let id = call.id.to_string();
        pending(service, &call, &CallStartAttempt::default());
        let result = service
            .complete_dm_call_with_crypto(
                &id,
                &UserId::new(),
                &worker.for_presentation_session(revision),
                |_| async { panic!("unrelated peer must never reach key derivation") },
            )
            .await;
        assert!(matches!(result, Err(CallSignalingError::PeerMismatch)));
        let kept = service
            .take_pending_dm_call()
            .expect("valid peer can still answer");
        assert_eq!(kept.peer_id, call.initiator_id);
        assert_eq!(kept.call_id, call.id);
    }

    #[tokio::test]
    async fn dm_hangup_authentication_keeps_peer_binding_after_connect() {
        use crate::services::call_worker::{OrchestratedCallKind, SignalHangupDisposition};
        let (_dir, _client, worker, revision) = fixture();
        let worker = worker.for_presentation_session(revision);
        let peer = UserId::new();
        let other = UserId::new();
        worker.orch_register_outgoing(
            CallHandle::from("call"),
            OrchestratedCallKind::DmStandard,
            peer,
            "Peer".into(),
            false,
        );
        assert!(!matches!(
            worker
                .authenticate_signal_hangup("call", other, false)
                .await,
            SignalHangupDisposition::Forward
        ));
        assert!(!matches!(
            worker
                .authenticate_signal_hangup("unknown", peer, false)
                .await,
            SignalHangupDisposition::Forward
        ));
        assert!(matches!(
            worker.authenticate_signal_hangup("call", peer, false).await,
            SignalHangupDisposition::Forward
        ));
        worker.orch_mark_connected(
            CallHandle::from("call"),
            OrchestratedCallKind::DmStandard,
            peer,
            false,
        );
        assert!(matches!(
            worker.authenticate_signal_hangup("call", peer, false).await,
            SignalHangupDisposition::Forward
        ));
        assert!(!matches!(
            worker
                .authenticate_signal_hangup("call", other, false)
                .await,
            SignalHangupDisposition::Forward
        ));
        assert!(!matches!(
            worker.authenticate_signal_hangup("call", peer, true).await,
            SignalHangupDisposition::Forward
        ));
    }

    #[tokio::test]
    async fn named_dm_crypto_wait_session_reset_cannot_uncancel_or_touch_replacement() {
        let (_dir, client, worker, revision) = fixture();
        let service = client.call_signaling().clone();
        let call = Call::new(ChannelId::new(), UserId::new(), CallType::Voice);
        let id = call.id.to_string();
        let peer = call.initiator_id;
        let attempt = CallStartAttempt::default();
        pending(&service, &call, &attempt);
        worker.test_prime_call(call).await;
        worker.test_dm_answer_backend(None).await;
        let mut events = worker.subscribe_call_events();
        let (entered, entered_rx) = oneshot::channel();
        let (resume, resume_rx) = oneshot::channel();
        let old_worker = worker.for_presentation_session(revision);
        let completion = tokio::spawn({
            let service = service.clone();
            let id = id.clone();
            async move {
                service
                    .complete_dm_call_with_crypto(&id, &peer, &old_worker, |_| async {
                        entered.send(()).unwrap();
                        resume_rx.await.unwrap();
                        Ok([3; 32])
                    })
                    .await
            }
        });
        entered_rx.await.unwrap();
        client.auth_state().invalidate_session();
        service.clear_call_session();
        worker.reset_presentation_session(revision).await.unwrap();
        assert!(service.start_attempts.lock().is_empty());
        assert!(attempt.is_cancelled());
        client
            .auth_state()
            .set_logged_in(UserId::new(), 1, "Replacement".into());
        worker
            .incoming_presentation_state()
            .begin_session(client.auth_state().get().session_revision);
        let replacement = Call::new(ChannelId::new(), UserId::new(), CallType::Voice);
        let replacement_id = replacement.id;
        worker.test_prime_call(replacement).await;
        resume.send(()).unwrap();
        assert!(matches!(
            completion.await.unwrap(),
            Err(CallSignalingError::NoPendingCall)
        ));
        assert_eq!(worker.test_dm_answer_effects().await, (0, 0));
        assert_eq!(worker.active_call().await.unwrap().id, replacement_id);
        assert!(service.take_pending_dm_call().is_none());
        no_connected(&mut events);
        worker.shutdown();
    }

    #[tokio::test]
    async fn named_dm_end_during_publish_drains_and_preserves_replacement() {
        let (_dir, client, worker, revision) = fixture();
        let service = client.call_signaling().clone();
        let call = Call::new(ChannelId::new(), UserId::new(), CallType::Voice);
        let id = call.id.to_string();
        let peer = call.initiator_id;
        let attempt = CallStartAttempt::default();
        pending(&service, &call, &attempt);
        worker.test_prime_call(call).await;
        let (entered, entered_rx) = oneshot::channel();
        let (resume, resume_rx) = oneshot::channel();
        worker
            .test_dm_answer_backend(Some((entered, resume_rx)))
            .await;
        let mut events = worker.subscribe_call_events();
        let old_worker = worker.for_presentation_session(revision);
        let completion = tokio::spawn({
            let service = service.clone();
            let id = id.clone();
            let old_worker = old_worker.clone();
            async move {
                service
                    .complete_dm_call_with_crypto(&id, &peer, &old_worker, |_| {
                        std::future::ready(Ok([3; 32]))
                    })
                    .await
            }
        });
        entered_rx.await.unwrap();
        assert!(service.cancel_start(&id)); // same synchronous End cancellation boundary
        let replacement = Call::new(ChannelId::new(), UserId::new(), CallType::Voice);
        let replacement_id = replacement.id;
        let installed = worker.test_queue_prime_call(replacement).await;
        let incoming = CallHandle::from("replacement-incoming");
        old_worker.orch_present_incoming(
            incoming.clone(),
            OrchestratedCallKind::DmStandard,
            UserId::new(),
            "New caller".into(),
            false,
            false,
            String::new(),
            None,
            None,
            String::new(),
        );
        resume.send(()).unwrap();
        installed.await.unwrap();
        assert!(matches!(
            completion.await.unwrap(),
            Err(CallSignalingError::NoPendingCall)
        ));
        assert_eq!(worker.test_dm_answer_effects().await, (1, 1));
        let late = old_worker.for_dm_answer(CallHandle::from(id.as_str()), Some(attempt));
        assert!(!late.set_encryption_key(vec![9; 32]).await);
        assert!(late.publish_microphone(None).await.is_err());
        assert!(late.publish_camera(None).await.is_err());
        assert!(
            late.orch_mark_connected_reliably(
                CallHandle::from(id.as_str()),
                OrchestratedCallKind::DmStandard,
                peer,
                false
            )
            .await
            .is_err()
        );
        // An uncancelled late answer must still fail the exact media identity fence.
        let wrong_identity = old_worker.for_dm_answer(
            CallHandle::from(id.as_str()),
            Some(CallStartAttempt::default()),
        );
        assert!(!wrong_identity.set_encryption_key(vec![7; 32]).await);
        assert!(wrong_identity.publish_microphone(None).await.is_err());
        assert!(wrong_identity.publish_camera(None).await.is_err());
        assert!(
            wrong_identity
                .orch_mark_connected_reliably(
                    CallHandle::from(id.as_str()),
                    OrchestratedCallKind::DmStandard,
                    peer,
                    false
                )
                .await
                .is_err()
        );
        assert_eq!(worker.test_dm_answer_effects().await, (1, 1));
        assert_eq!(worker.active_call().await.unwrap().id, replacement_id);
        let presentations = worker
            .incoming_presentation_snapshot()
            .await
            .unwrap()
            .presentations;
        assert!(presentations.iter().any(|p| matches!(p, CallOrchestratorEvent::IncomingPresented { call, .. } if call == &incoming)));
        no_connected(&mut events);
        worker.shutdown();
    }
}

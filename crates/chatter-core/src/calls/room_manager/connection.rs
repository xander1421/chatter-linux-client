use super::*;

impl RoomManager {
    /// Connect to a LiveKit room with E2EE.
    ///
    /// # Arguments
    /// * `url` - LiveKit server URL
    /// * `token` - LiveKit access token (from server)
    /// * `encryption_key` - 32-byte key for E2EE (from Signal derivation)
    pub async fn connect(
        &self,
        url: &str,
        token: &str,
        encryption_key: Vec<u8>,
    ) -> Result<(), CallError> {
        self.connect_timed(
            url,
            token,
            encryption_key,
            CallTiming::new("room_connect_requested"),
        )
        .await
    }

    pub(crate) fn timing_mark(&self, stage: &'static str) {
        if let Some(timing) = self.timing.read().as_ref() {
            timing.mark(stage);
        }
    }

    fn retire_timing(&self, expected: Option<&CallTiming>) {
        retire_timing_slot(&self.timing, expected);
    }

    pub(crate) async fn connect_timed(
        &self,
        url: &str,
        token: &str,
        encryption_key: Vec<u8>,
        timing: CallTiming,
    ) -> Result<(), CallError> {
        self.microphone_requested
            .store(false, std::sync::atomic::Ordering::SeqCst);
        // Disable the SDK ratchet: the application owns the HKDF epoch chain.
        let key_provider = KeyProvider::with_shared_key(
            KeyProviderOptions {
                ratchet_window_size: 0,
                key_ring_size: KEY_RING_SIZE,
                ..KeyProviderOptions::default()
            },
            encryption_key,
        );
        self.connect_with_provider(url, token, key_provider, 0, timing, None, None)
            .await
    }

    pub(crate) fn snapshot_reconnect_keys(
        &self,
        chain: &crate::crypto::call_key_provider::CallKeyState,
    ) -> Result<ReconnectKeys, CallError> {
        let e2ee = self.e2ee.read();
        let provider = e2ee.provider.clone().ok_or(CallError::NotInCall)?;
        if e2ee.epoch != chain.epoch()
            || provider.get_latest_key_index() != key_ring_index(chain.epoch())
        {
            return Err(CallError::CryptoError(
                "Reconnect key epoch mismatch".into(),
            ));
        }
        Ok(ReconnectKeys {
            provider,
            epoch: chain.epoch(),
            current: zeroize::Zeroizing::new(chain.current_key_vec()),
            next: zeroize::Zeroizing::new(
                chain
                    .peek_next_key()
                    .map_err(|e| CallError::CryptoError(e.to_string()))?
                    .to_vec(),
            ),
        })
    }

    /// Catch up on the provisional room, before its handle or Connected event
    /// can permit publication. The caller commits this staged chain on success.
    pub(crate) async fn connect_reusing_keys_aligned(
        &self,
        url: &str,
        token: &str,
        snapshot: ReconnectKeys,
        chain: &mut crate::crypto::call_key_provider::CallKeyState,
        started_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), CallError> {
        self.connect_with_provider(
            url,
            token,
            snapshot.provider.clone(),
            snapshot.epoch,
            CallTiming::new("room_reconnect_requested"),
            Some(snapshot),
            Some((chain, started_at)),
        )
        .await
    }

    async fn connect_with_provider(
        &self,
        url: &str,
        token: &str,
        key_provider: KeyProvider,
        epoch: u32,
        timing: CallTiming,
        snapshot: Option<ReconnectKeys>,
        catch_up: Option<(
            &mut crate::crypto::call_key_provider::CallKeyState,
            Option<chrono::DateTime<chrono::Utc>>,
        )>,
    ) -> Result<(), CallError> {
        // Signaling uses the same private CA as gRPC/storage, not the SDK's
        // default public root store. Validate before changing room state.
        let parsed = url::Url::parse(url)
            .map_err(|_| CallError::ConnectionFailed("invalid LiveKit endpoint".into()))?;
        let tls_root_ca_pem = match parsed.scheme() {
            "wss" | "https" => Some(
                crate::config::TLS_ROOT_CA_PEM
                    .ok_or_else(|| {
                        CallError::ConnectionFailed("missing pinned LiveKit TLS CA".into())
                    })?
                    .to_owned(),
            ),
            "ws" | "http" if cfg!(any(test, feature = "dev-trust-root")) => None,
            _ => {
                return Err(CallError::ConnectionFailed(
                    "LiveKit signaling requires WSS".into(),
                ));
            }
        };
        replace_timing_slot(&self.timing, timing.clone());
        timing.mark("room_connect_begin");
        let generation = self
            .connection_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.set_state(RoomState::Connecting);

        // Store key provider for later ratcheting
        {
            let mut e2ee = self.e2ee.write();
            e2ee.provider = Some(key_provider.clone());
            e2ee.epoch = epoch;
        }

        // Configure room with E2EE
        let mut room_options = RoomOptions::default();
        room_options.auto_subscribe = true;
        room_options.tls_root_ca_pem = tls_root_ca_pem;
        room_options.encryption = Some(E2eeOptions {
            encryption_type: EncryptionType::Gcm,
            key_provider,
        });

        // Connect to room (timeout prevents indefinite hang on slow/dead servers)
        let connect_started = std::time::Instant::now();
        tracing::info!(
            stage = "room_connect_begin",
            elapsed_us = 0u64,
            "Call stage"
        );
        let connected = tokio::time::timeout(
            tokio::time::Duration::from_secs(10),
            Room::connect(url, token, room_options),
        )
        .await;
        let stage = match &connected {
            Ok(Ok(_)) => "room_connect_succeeded",
            Ok(Err(_)) => "room_connect_failed",
            Err(_) => "room_connect_timed_out",
        };
        tracing::info!(
            stage,
            elapsed_us = connect_started.elapsed().as_micros() as u64,
            "Call stage"
        );
        timing.mark(stage);
        if !matches!(&connected, Ok(Ok(_))) {
            self.retire_timing(Some(&timing));
        }
        let (room, mut events) = connected
            .map_err(|_| {
                CallError::ConnectionFailed("LiveKit connection timed out (10s)".to_string())
            })?
            .map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

        // Complete key initialization before publishing the room handle, emitting
        // Connected, or permitting any new local media publication.
        let catch_up = catch_up.map(|(chain, started_at)| {
            let target = started_at
                .map(crate::services::call_worker::reconnect_wall_epoch)
                .unwrap_or(chain.epoch());
            (chain, target)
        });
        match initialize_provisional_keys(snapshot.as_ref(), catch_up, |epoch, current, next| {
            room.e2ee_manager().install_shared_media_key(
                key_ring_index(epoch),
                current,
                Some((key_ring_index(epoch.wrapping_add(1)), next)),
            )
        }) {
            Ok(Some(epoch)) => self.e2ee.write().epoch = epoch,
            Ok(None) => {}
            Err(error) => {
                let _ = room.close().await;
                self.set_state(RoomState::Disconnected);
                self.retire_timing(Some(&timing));
                return Err(error);
            }
        }

        // Enable E2EE
        room.e2ee_manager().set_enabled(true);

        let room_name = room.name().to_string();
        let remote_participants: Vec<_> = room.remote_participants().values().cloned().collect();
        let participant_count = remote_participants.len() + 1;
        timing.mark(if remote_participants.is_empty() {
            "remote_participant_waiting"
        } else {
            "remote_participant_present"
        });

        // Emit ParticipantJoined events for participants already in the room
        // (LiveKit only sends ParticipantConnected for NEW participants after we join)
        // Also subscribe to their existing tracks since TrackSubscribed events may have
        // already fired before we set up our event handler
        for participant in &remote_participants {
            let identity = participant.identity().to_string();
            let name = participant.name();
            tracing::info!(
                "RoomManager: Existing participant in room - identity={}, name={}",
                identity,
                name
            );
            let _ = self.event_tx.try_send(RoomEvent::ParticipantJoined {
                identity: identity.clone(),
                display_name: if name.is_empty() {
                    None
                } else {
                    Some(name.to_string())
                },
            });

            // Process existing tracks from this participant
            for (track_sid, track_publication) in participant.track_publications() {
                if let Some(track) = track_publication.track() {
                    match track {
                        RemoteTrack::Audio(audio_track) => {
                            tracing::info!(
                                "RoomManager: Existing audio track from {} (sid: {})",
                                identity,
                                track_sid
                            );
                            if let Some(playback) = self.audio_playback.read().as_ref() {
                                playback.add_track_timed(
                                    identity.clone(),
                                    audio_track,
                                    Some(timing.clone()),
                                );
                                tracing::info!(
                                    "RoomManager: Added existing audio track to playback"
                                );
                            }
                            let _ = self.event_tx.try_send(RoomEvent::TrackSubscribed {
                                participant_identity: identity.clone(),
                                track_sid: track_sid.to_string(),
                                is_video: false,
                                is_screen_share: false,
                            });
                        }
                        RemoteTrack::Video(video_track) => {
                            let source = video_track.source();
                            let is_screen_share = matches!(
                                source,
                                TrackSource::Screenshare | TrackSource::ScreenshareAudio
                            );
                            tracing::info!(
                                "RoomManager: Existing video track from {} (sid: {}, source: {:?})",
                                identity,
                                track_sid,
                                source
                            );
                            self.video_playback.read().add_track(
                                identity.clone(),
                                video_track,
                                is_screen_share,
                            );
                            tracing::info!("RoomManager: Added existing video track to playback");
                            let _ = self.event_tx.try_send(RoomEvent::TrackSubscribed {
                                participant_identity: identity.clone(),
                                track_sid: track_sid.to_string(),
                                is_video: true,
                                is_screen_share,
                            });
                        }
                    }
                }
            }
        }

        // Store room
        {
            let mut room_guard = self.room.write();
            *room_guard = Some(room);
        }

        self.set_state(RoomState::Connected {
            room_name,
            participant_count,
        });

        // Spawn event handler on LocalSet (LiveKit types are !Send)
        let event_tx = self.event_tx.clone();
        let state = self.state.clone();
        let room_arc = self.room.clone();
        let audio_playback = self.audio_playback.clone();
        let video_playback = self.video_playback.clone();

        // Must use spawn_local because:
        // 1. Call worker runs on a LocalSet with new_current_thread() runtime
        // 2. LiveKit Room/events are !Send and must stay on the same thread
        let event_task_slot = self.event_task.clone();
        let generation_slot = self.connection_generation.clone();
        let membership_revision = self.membership_revision.clone();
        let handle = tokio::task::spawn_local(async move {
            tracing::info!("Room event handler started");
            let mut cryptor_evidence = ReceiverCryptorEvidence::default();
            while let Some(event) = events.recv().await {
                // All effects, including track/playback and membership, belong to this connection.
                if generation_slot.load(std::sync::atomic::Ordering::SeqCst) != generation {
                    break;
                }
                // Invalidate grace before any awaited control delivery at either
                // SDK recovery boundary. Connection generation stays unchanged.
                if matches!(
                    &event,
                    livekit::RoomEvent::Reconnecting | livekit::RoomEvent::Reconnected
                ) {
                    membership_revision.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                // Avoid dumping arbitrary SDK event payloads and identities.
                if let livekit::RoomEvent::E2eeStateChanged { participant, state } = &event {
                    if cryptor_evidence.observe(
                        matches!(participant, livekit::participant::Participant::Remote(_)),
                        *state,
                    ) {
                        timing.mark("remote_frame_cryptor_ok_room_wide_track_unknown");
                        tracing::info!(
                            stage = "remote_frame_cryptor_ok",
                            "Remote SDK frame cryptor reported Ok (room-wide, track unavailable)"
                        );
                    }
                }
                // A detached handler (disconnect join timed out) must not
                // mirror its dead connection's lifecycle onto a newer one.
                let live = generation_slot.load(std::sync::atomic::Ordering::SeqCst) == generation;
                Self::handle_room_event(
                    event,
                    (generation, live),
                    &generation_slot,
                    &event_tx,
                    &state,
                    &room_arc,
                    &audio_playback,
                    &video_playback,
                    &timing,
                )
                .await;
                // Yield to allow other tasks (like forwarder) to run
                tokio::task::yield_now().await;
            }

            // Retire only this handler's observer: a detached old handler must
            // never retire a newer connection's timing.
            timing.retire();
            if generation_slot.load(std::sync::atomic::Ordering::SeqCst) != generation {
                return;
            }
            // Room disconnected - stop audio/video playback
            tracing::info!("Room event stream ended - marking disconnected");
            if let Some(playback) = audio_playback.read().as_ref() {
                playback.stop();
            }
            video_playback.read().stop();
            // Only this connection's handler may close its state, and a
            // terminal loss already recorded stays recorded (the worker
            // fences its cleanup on `Failed`).
            if generation_slot.load(std::sync::atomic::Ordering::SeqCst) == generation {
                let mut guard = state.write();
                if !matches!(*guard, RoomState::Failed { .. }) {
                    *guard = RoomState::Disconnected;
                    drop(guard);
                    let _ = event_tx.try_send(RoomEvent::StateChanged(RoomState::Disconnected));
                }
            }
        });
        *event_task_slot.write() = Some(handle);

        Ok(())
    }

    /// Disconnect from the current room.
    ///
    /// This method is safe to call from the UI thread. Capture threads are
    /// signaled to stop (non-blocking) and will clean themselves up.
    /// Thread joins happen in Drop to ensure proper cleanup.
    pub async fn disconnect(&self) {
        self.disconnect_inner(false).await;
    }

    /// Retire the room transport while the active call still owns its camera.
    pub(crate) async fn disconnect_for_reconnect(&self) {
        self.disconnect_inner(true).await;
    }

    async fn disconnect_inner(&self, preserve_camera: bool) {
        // Retire handler ownership before any await or reuse of shared playback.
        self.connection_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.retire_timing(None);
        // Update state immediately so UI can respond
        self.set_state(RoomState::Disconnected);

        // Stop audio playback
        if let Some(playback) = self.audio_playback.read().as_ref() {
            playback.stop();
            tracing::info!("Audio playback stopped");
        }

        // Stop video playback
        self.video_playback.read().stop();
        tracing::info!("Video playback stopped");

        // Signal all capture threads to stop (non-blocking, just sets atomic flags)
        // The threads will exit on their own, joins happen in Drop
        {
            let audio = self.audio_capture.read();
            audio.signal_stop();
            self.microphone_track_sid.write().take();
        }
        {
            let video = self.video_capture.read();
            self.camera_capture_retained.store(
                preserve_camera && video.is_running(),
                std::sync::atomic::Ordering::SeqCst,
            );
            if !preserve_camera {
                video.signal_stop();
            }
            // Track auto-unpublishes when the room closes; clear the flag + sid
            // so the next call re-publishes instead of inheriting a dead handle.
            self.camera_published
                .store(false, std::sync::atomic::Ordering::SeqCst);
            self.camera_track_sid.write().take();
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            let screen = self.screen_capture.read();
            screen.signal_stop();
            // Track auto-unpublishes when the room closes; clear the cached
            // sid so the next call doesn't inherit a stale handle.
            self.screen_share_track_sid.write().take();
        }
        #[cfg(target_os = "android")]
        {
            super::super::android_screen::stop();
            self.screen_share_track_sid.write().take();
        }

        // Take the room handle
        let room = {
            let mut room_guard = self.room.write();
            room_guard.take()
        };

        // Close the LiveKit room with timeout
        if let Some(room) = room {
            match tokio::time::timeout(tokio::time::Duration::from_secs(3), room.close()).await {
                Ok(_) => tracing::info!("Room closed successfully"),
                Err(_) => tracing::warn!("Room close timed out, forcing disconnect"),
            }
        }

        // Wait for event handler task to finish draining
        let event_handle = self.event_task.write().take();
        if let Some(handle) = event_handle {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await {
                Ok(Ok(())) => tracing::debug!("Event handler task joined cleanly"),
                Ok(Err(e)) => tracing::warn!("Event handler task panicked: {}", e),
                Err(_) => tracing::warn!("Event handler join timed out (2s), proceeding"),
            }
        }

        tracing::info!("Disconnect completed (capture cleanup deferred)");
    }

    /// Get current state.
    pub fn state(&self) -> RoomState {
        self.state.read().clone()
    }

    /// The session is alive: connected, or being recovered by the SDK with
    /// its keys still installed. Media publish gates that need a working
    /// transport check for `Connected` specifically.
    pub fn is_connected(&self) -> bool {
        matches!(
            self.state(),
            RoomState::Connected { .. } | RoomState::Reconnecting { .. }
        )
    }

    /// Identity of the current connect attempt ([`RoomEvent::ConnectionLost`]).
    pub fn connection_generation(&self) -> u64 {
        self.connection_generation
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Grace checks must belong to one uninterrupted SDK membership phase.
    pub fn membership_revision(&self) -> u64 {
        self.membership_revision
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Get current participant count.
    pub fn participant_count(&self) -> usize {
        match self.state() {
            RoomState::Connected {
                participant_count, ..
            }
            | RoomState::Reconnecting {
                participant_count, ..
            } => participant_count,
            _ => 0,
        }
    }

    /// Identities of the remote participants currently in the room.
    ///
    /// A LiveKit identity is the peer's `user_id` — the server mints call
    /// tokens with `SetIdentity(userID)` — so this is the live roster of who
    /// can decrypt media right now. Key rotation must target this, not channel
    /// membership: a peer who left the room is still a channel member.
    ///
    /// Empty when not connected.
    pub fn remote_participant_identities(&self) -> Vec<String> {
        match self.room.read().as_ref() {
            Some(room) => room
                .remote_participants()
                .keys()
                .map(|identity| identity.to_string())
                .collect(),
            None => Vec::new(),
        }
    }

    /// Our own LiveKit `joined_at_ms` and each remote participant's
    /// `(identity, joined_at_ms)` — the seniority the group-call key
    /// authority is chosen by. `None` when not connected.
    ///
    /// `joined_at_ms` is assigned by the LiveKit server, so every participant
    /// reads the same values; like the roster itself it is server-asserted.
    pub fn room_seniority(&self) -> Option<(i64, Vec<(String, i64)>)> {
        let room = self.room.read();
        let room = room.as_ref()?;
        let remote = room
            .remote_participants()
            .iter()
            .map(|(identity, participant)| (identity.to_string(), participant.joined_at()))
            .collect();
        Some((room.local_participant().joined_at(), remote))
    }
}

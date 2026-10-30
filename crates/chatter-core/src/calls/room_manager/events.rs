use super::*;

impl RoomManager {
    pub(super) fn set_state(&self, state: RoomState) {
        *self.state.write() = state.clone();
        let _ = self.event_tx.try_send(RoomEvent::StateChanged(state));
    }

    // Reserve capacity asynchronously: terminal control must survive a full data queue.
    // Recheck ownership after waiting, before placing the event in the queue.
    pub(super) async fn send_control(
        tx: &mpsc::Sender<RoomEvent>,
        owner: &std::sync::atomic::AtomicU64,
        generation: u64,
        event: RoomEvent,
    ) {
        if let Ok(permit) = tx.reserve().await {
            if owner.load(std::sync::atomic::Ordering::SeqCst) == generation {
                permit.send(event);
            }
        }
    }

    /// Canonical transport and membership are usable for peer-left policy.
    pub fn membership_is_current(&self) -> bool {
        matches!(self.state(), RoomState::Connected { .. })
            && self
                .room
                .read()
                .as_ref()
                .is_some_and(|r| r.connection_state() == livekit::ConnectionState::Connected)
    }

    pub(super) async fn handle_room_event(
        event: livekit::RoomEvent,
        (generation, live): (u64, bool),
        generation_slot: &std::sync::atomic::AtomicU64,
        event_tx: &mpsc::Sender<RoomEvent>,
        state: &Arc<RwLock<RoomState>>,
        room: &Arc<RwLock<Option<Room>>>,
        audio_playback: &Arc<RwLock<Option<AudioPlayback>>>,
        video_playback: &Arc<RwLock<VideoPlayback>>,
        timing: &CallTiming,
    ) {
        if !live {
            return;
        }
        match event {
            livekit::RoomEvent::ParticipantConnected(participant) => {
                timing.mark("remote_participant_present");
                let identity = participant.identity().to_string();
                let name = participant.name();
                tracing::info!(
                    "RoomManager: ParticipantConnected - identity={}, name={}",
                    identity,
                    name
                );
                let event = RoomEvent::ParticipantJoined {
                    identity: identity.clone(),
                    display_name: if name.is_empty() {
                        None
                    } else {
                        Some(name.to_string())
                    },
                };
                tracing::info!("RoomManager: Sending event to channel: {:?}", event);
                match event_tx.try_send(event) {
                    Ok(()) => tracing::info!("RoomManager: Event sent successfully"),
                    Err(e) => tracing::error!(
                        "RoomManager: Failed to send ParticipantJoined event: {:?}",
                        e
                    ),
                }

                // Update participant count (avoid holding read lock while getting write lock)
                let new_state = {
                    let room_guard = room.read();
                    if let Some(r) = room_guard.as_ref() {
                        let count = r.remote_participants().len() + 1;
                        let state_guard = state.read();
                        match &*state_guard {
                            RoomState::Connected { room_name, .. } => Some(RoomState::Connected {
                                room_name: room_name.clone(),
                                participant_count: count,
                            }),
                            RoomState::Reconnecting { room_name, .. } => {
                                Some(RoomState::Reconnecting {
                                    room_name: room_name.clone(),
                                    participant_count: count,
                                })
                            }
                            _ => None,
                        }
                    } else {
                        None
                    }
                }; // All read locks dropped here

                if let Some(new_state) = new_state {
                    *state.write() = new_state;
                }
            }
            livekit::RoomEvent::ParticipantDisconnected(participant) => {
                let participant_id = participant.identity().to_string();

                // Remove audio playback for this participant
                if let Some(playback) = audio_playback.read().as_ref() {
                    playback.remove_track(&participant_id);
                }

                // Remove video playback for this participant — both their camera
                // and screen-share tracks (the peer is gone entirely).
                {
                    let vp = video_playback.read();
                    vp.remove_track(&participant_id, false);
                    vp.remove_track(&participant_id, true);
                }

                // Full restart evicts participants before delivering Reconnecting.
                // Consult the SDK state as well as our mirrored phase.
                let membership_current = matches!(*state.read(), RoomState::Connected { .. })
                    && room.read().as_ref().is_some_and(|r| {
                        r.connection_state() == livekit::ConnectionState::Connected
                    });
                if membership_current {
                    let _ = event_tx.try_send(RoomEvent::ParticipantLeft {
                        identity: participant_id,
                    });
                    Self::send_control(
                        event_tx,
                        generation_slot,
                        generation,
                        RoomEvent::MembershipChanged { generation },
                    )
                    .await;
                }
            }
            livekit::RoomEvent::TrackSubscribed {
                track, participant, ..
            } => {
                let participant_id = participant.identity().to_string();
                let is_video = matches!(track, RemoteTrack::Video(_));

                // Determine if this is a screen share track
                let is_screen_share = match &track {
                    RemoteTrack::Video(video_track) => {
                        matches!(
                            video_track.source(),
                            TrackSource::Screenshare | TrackSource::ScreenshareAudio
                        )
                    }
                    _ => false,
                };

                // Handle audio/video track playback
                match &track {
                    RemoteTrack::Audio(audio_track) => {
                        tracing::info!(
                            "RoomManager: Audio track subscribed from {} (sid: {})",
                            participant_id,
                            track.sid()
                        );
                        if let Some(playback) = audio_playback.read().as_ref() {
                            playback.add_track_timed(
                                participant_id.clone(),
                                audio_track.clone(),
                                Some(timing.clone()),
                            );
                            tracing::info!("RoomManager: Added audio track to playback");
                        } else {
                            tracing::warn!(
                                "RoomManager: No audio playback available, cannot play remote audio"
                            );
                        }
                    }
                    RemoteTrack::Video(video_track) => {
                        let source = video_track.source();
                        tracing::info!(
                            "RoomManager: Video track subscribed from {} (sid: {}, source: {:?}, screen_share: {})",
                            participant_id,
                            track.sid(),
                            source,
                            is_screen_share
                        );
                        video_playback.read().add_track(
                            participant_id.clone(),
                            video_track.clone(),
                            is_screen_share,
                        );
                        tracing::info!("RoomManager: Added video track to playback");
                    }
                }

                let _ = event_tx.try_send(RoomEvent::TrackSubscribed {
                    participant_identity: participant_id,
                    track_sid: track.sid().to_string(),
                    is_video,
                    is_screen_share,
                });
            }
            livekit::RoomEvent::TrackUnsubscribed {
                track, participant, ..
            } => {
                let participant_id = participant.identity().to_string();
                let is_video = matches!(track, RemoteTrack::Video(_));
                let mut is_screen_share = false;

                // Remove audio playback if it's an audio track
                if matches!(track, RemoteTrack::Audio(_)) {
                    if let Some(playback) = audio_playback.read().as_ref() {
                        playback.remove_track_if_sid(&participant_id, &track.sid().to_string());
                        tracing::info!(
                            "RoomManager: Removed audio track from playback for {}",
                            participant_id
                        );
                    }
                }

                // Remove video playback if it's a video track
                if let RemoteTrack::Video(ref v) = track {
                    is_screen_share = matches!(
                        v.source(),
                        TrackSource::Screenshare | TrackSource::ScreenshareAudio
                    );
                    video_playback
                        .read()
                        .remove_track(&participant_id, is_screen_share);
                    tracing::info!(
                        "RoomManager: Removed video track from playback for {} (screen_share={})",
                        participant_id,
                        is_screen_share
                    );
                }

                let _ = event_tx.try_send(RoomEvent::TrackUnsubscribed {
                    participant_identity: participant_id,
                    track_sid: track.sid().to_string(),
                    is_video,
                    is_screen_share,
                });
            }
            livekit::RoomEvent::ConnectionQualityChanged {
                participant,
                quality,
            } => {
                use livekit::participant::ConnectionQuality as CQ;
                let score: u8 = match quality {
                    CQ::Excellent => 4,
                    CQ::Good => 3,
                    CQ::Poor => 1,
                    _ => 0,
                };
                let _ = event_tx.try_send(RoomEvent::ConnectionQualityChanged {
                    participant_identity: participant.identity().to_string(),
                    quality: score,
                });
            }
            livekit::RoomEvent::E2eeStateChanged {
                participant,
                state: enc_state,
            } => {
                use livekit::webrtc::native::frame_cryptor::EncryptionState;
                let is_encrypted = matches!(
                    enc_state,
                    EncryptionState::Ok | EncryptionState::KeyRatcheted
                );
                let _ = event_tx.try_send(RoomEvent::EncryptionStateChanged {
                    participant_identity: participant.identity().to_string(),
                    is_encrypted,
                });
            }
            livekit::RoomEvent::ActiveSpeakersChanged { speakers } => {
                let speaker_identities =
                    speakers.iter().map(|p| p.identity().to_string()).collect();
                let _ = event_tx.try_send(RoomEvent::ActiveSpeakersChanged { speaker_identities });
            }
            // SDK-owned recovery: the session, its tracks and its installed
            // keys survive; we only mirror the phase. Never join again here.
            livekit::RoomEvent::Reconnecting => {
                if !live {
                    return;
                }
                let next = match &*state.read() {
                    RoomState::Connected {
                        room_name,
                        participant_count,
                    } => Some(RoomState::Reconnecting {
                        room_name: room_name.clone(),
                        participant_count: *participant_count,
                    }),
                    _ => None,
                };
                if let Some(next) = next {
                    tracing::warn!("Room connection interrupted; SDK reconnecting");
                    *state.write() = next.clone();
                    let _ = event_tx.try_send(RoomEvent::StateChanged(next));
                    Self::send_control(
                        event_tx,
                        generation_slot,
                        generation,
                        RoomEvent::RecoveryPhase {
                            generation,
                            recovered: false,
                        },
                    )
                    .await;
                }
            }
            livekit::RoomEvent::Reconnected => {
                if !live {
                    return;
                }
                let count = room
                    .read()
                    .as_ref()
                    .map(|r| r.remote_participants().len() + 1)
                    .unwrap_or(1);
                let next = match &*state.read() {
                    RoomState::Reconnecting { room_name, .. } => Some(RoomState::Connected {
                        room_name: room_name.clone(),
                        participant_count: count,
                    }),
                    _ => None,
                };
                if let Some(next) = next {
                    tracing::info!("Room connection recovered by SDK");
                    *state.write() = next.clone();
                    Self::send_control(
                        event_tx,
                        generation_slot,
                        generation,
                        RoomEvent::RecoveryPhase {
                            generation,
                            recovered: true,
                        },
                    )
                    .await;
                    Self::send_control(
                        event_tx,
                        generation_slot,
                        generation,
                        RoomEvent::StateChanged(next),
                    )
                    .await;
                    Self::send_control(
                        event_tx,
                        generation_slot,
                        generation,
                        RoomEvent::MembershipChanged { generation },
                    )
                    .await;
                }
            }
            // Terminal for this connection: the SDK closes the room next and
            // the stream-end branch below stops playback. `ClientInitiated` is
            // our own `disconnect()`, already recorded there.
            livekit::RoomEvent::Disconnected { reason } => {
                if !live || reason == livekit::DisconnectReason::ClientInitiated {
                    return;
                }
                let lost = matches!(
                    &*state.read(),
                    RoomState::Connected { .. } | RoomState::Reconnecting { .. }
                );
                if !lost {
                    return;
                }
                let reason_text = format!("{reason:?}");
                tracing::warn!(reason = %reason_text, "Room connection lost");
                let failed = RoomState::Failed {
                    reason: reason_text.clone(),
                };
                *state.write() = failed.clone();
                Self::send_control(
                    event_tx,
                    generation_slot,
                    generation,
                    RoomEvent::StateChanged(failed),
                )
                .await;
                Self::send_control(
                    event_tx,
                    generation_slot,
                    generation,
                    RoomEvent::ConnectionLost {
                        generation,
                        reason: reason_text,
                        retryable: loss_is_retryable(reason),
                    },
                )
                .await;
            }
            // Redundant with Reconnecting/Reconnected/Disconnected above.
            livekit::RoomEvent::ConnectionStateChanged(_) => {}
            _ => {}
        }
    }
}
